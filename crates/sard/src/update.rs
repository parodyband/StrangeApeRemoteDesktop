//! Self-update from GitHub releases.
//!
//! Every release carries two small files next to the downloads:
//!
//! `latest.txt`, a line-based manifest (UTF-8, `\n` line ends, `#` comments and blank lines ignored, unknown
//! keys ignored so later versions can add fields):
//!
//! ```text
//! sard-update 1
//! version 0.2.0
//! notes https://github.com/parodyband/StrangeApeRemoteDesktop/releases/tag/v0.2.0
//! asset linux-x86_64 <sha256 hex> <size in bytes> https://…/sard-0.2.0-linux-x86_64.tar.gz
//! asset macos-arm64 <sha256 hex> <size in bytes> https://…/sard-0.2.0-macos-arm64.zip
//! asset windows-x86_64 <sha256 hex> <size in bytes> https://…/sard-0.2.0-windows-x86_64.zip
//! ```
//!
//! and `latest.txt.sig`: one or more lines, each a hex ed25519 signature over the exact bytes of `latest.txt`.
//! The manifest is accepted if any line verifies under any key in [`TRUSTED_KEYS`] (several lines/keys exist
//! only while rotating the key, see docs/RELEASING.md). The first manifest line doubles as the domain tag: a
//! signature over anything else can never be taken for a manifest.
//!
//! Nothing is trusted before the signature checks out: the download URL, its size and SHA-256 all come from
//! the signed manifest, and the download is verified before it is unpacked. Transport is `curl` (HTTPS only,
//! also for redirects), so sard links no TLS stack. Windows 10 (1803+) and 11 ship `curl.exe` and `tar.exe`
//! (bsdtar, which unpacks the zip) in System32.
//!
//! Only official release builds (compiled with `SARD_RELEASE_BUILD=1`, as CI does) replace themselves; a
//! development build checks but refuses to install, so a `cargo build` or a dev `Sard.app` holding macOS
//! permission grants is never overwritten behind the developer's back.

use crate::Result;
use crate::args::Args;
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use sard_core::crypto::{fingerprint, sha256};
use sard_core::wire::{hex, unhex};
use std::cmp::Ordering;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Read};
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// Public keys (hex) trusted to sign `latest.txt`. The private half lives in the `UPDATE_SIGNING_KEY` GitHub
/// Actions secret. To rotate, add the new key here first (see docs/RELEASING.md).
const TRUSTED_KEYS: &[&str] = &["dbc65f711fd60624c99d61e768c9d7757dee6f1d8702887eac612307d4ec7724"];

const DEFAULT_BASE_URL: &str = "https://github.com/parodyband/StrangeApeRemoteDesktop/releases/latest/download";
/// Overrides where `latest.txt` / `latest.txt.sig` are fetched from (testing with a local server). The
/// signature is still checked against the embedded keys, so this cannot be used to install anything unsigned.
const BASE_URL_ENV: &str = "SARD_UPDATE_URL";
const MANIFEST_MAGIC: &str = "sard-update 1";
const MAX_MANIFEST_BYTES: u64 = 64 << 10;
const MAX_SIG_BYTES: u64 = 4 << 10;
const MAX_ASSET_BYTES: u64 = 512 << 20;
const VERSION: &str = env!("CARGO_PKG_VERSION");
const RELEASE_BUILD: bool = option_env!("SARD_RELEASE_BUILD").is_some();
#[cfg(target_os = "macos")]
const BUNDLE_ID: &str = "com.strangeape.sard";
/// Set for the process [`restart`] starts on Windows, which may have to wait for the old one to let go of the
/// app lock (there is no `exec` there).
pub const RESTARTED_ENV: &str = "SARD_RESTARTED";

/// A downloadable build for one platform, as listed in the signed manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asset {
    /// `linux-x86_64`, `macos-arm64` or `windows-x86_64`.
    pub platform: String,
    pub sha256: [u8; 32],
    pub size: u64,
    pub url: String,
}

/// A newer release that can be installed on this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateInfo {
    /// The new version, e.g. `0.2.0`.
    pub version: String,
    /// Release notes page, when the manifest names one.
    pub notes: Option<String>,
    /// The download for this platform.
    pub asset: Asset,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Manifest {
    version: String,
    notes: Option<String>,
    assets: Vec<Asset>,
}

/// The manifest platform name of this build, if releases are published for it.
pub fn platform() -> Option<&'static str> {
    if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Some("linux-x86_64")
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Some("macos-arm64")
    } else if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        Some("windows-x86_64")
    } else {
        None
    }
}

/// Fetch and verify the latest release manifest. `Ok(None)` when this build is up to date.
pub fn check() -> Result<Option<UpdateInfo>> {
    let platform = platform().ok_or("automatic updates are not published for this platform")?;
    select(&fetch_manifest()?, VERSION, platform)
}

/// Download, verify and install `info`, replacing this installation: the running executable on Linux and
/// Windows, the enclosing `.app` bundle on macOS. The swap is a rename (on Windows the running executable is
/// renamed aside first); on any error the old version stays.
/// Running processes keep the old code until [`restart`].
pub fn install(info: &UpdateInfo) -> Result<()> {
    let target = install_target()?;
    let platform = platform().ok_or("automatic updates are not published for this platform")?;
    if info.asset.platform != platform {
        return Err(format!("update is for {}, this is {platform}", info.asset.platform).into());
    }
    if Version::parse(&info.version).is_none_or(|v| Version::parse(VERSION).is_some_and(|cur| v <= cur)) {
        return Err(format!("{} is not newer than the running {VERSION}", info.version).into());
    }
    #[cfg(target_os = "macos")]
    {
        install_bundle(info, &target)
    }
    #[cfg(windows)]
    {
        install_windows(info, &target)
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        install_binary(info, &target)
    }
}

/// What [`install`] replaces, or why this installation can't update itself (development build, not inside
/// `Sard.app`, read-only location).
pub fn install_target() -> Result<PathBuf> {
    static TARGET: OnceLock<PathBuf> = OnceLock::new();
    if let Some(t) = TARGET.get() {
        return Ok(t.clone());
    }
    if !RELEASE_BUILD {
        return Err("this is a development build; only official release builds update themselves \
                    (build with SARD_RELEASE_BUILD=1 to override)"
            .into());
    }
    let exe = current_exe()?;
    let target = if cfg!(target_os = "macos") {
        let bundle = enclosing_bundle(&exe).ok_or("not running from Sard.app; download the new version manually")?;
        if bundle.to_string_lossy().contains("/AppTranslocation/") {
            return Err(
                "Sard.app is running from a temporary read-only location; move it to /Applications first".into()
            );
        }
        bundle
    } else {
        exe
    };
    let dir = target.parent().ok_or("installation has no parent directory")?;
    if !writable(dir) {
        return Err(format!("cannot update {}: {} is not writable", target.display(), dir.display()).into());
    }
    Ok(TARGET.get_or_init(|| target).clone())
}

/// Replace this process with the (newly installed) version, keeping the command line. macOS relaunches the
/// bundle through LaunchServices (`open -n`) so permissions stay attributed to Sard.app; Windows starts the
/// new executable and exits.
pub fn restart() -> ! {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let target = install_target().or_else(|_| current_exe());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        match target {
            Ok(exe) => {
                let mut cmd = Command::new(&exe);
                cmd.args(&args).env(RESTARTED_ENV, "1").stdin(Stdio::null());
                if !sard_windows::os::has_console() {
                    // The tray app has no console: don't let the new one open a console window.
                    cmd.creation_flags(sard_windows::os::DETACHED_PROCESS);
                }
                match cmd.spawn() {
                    Ok(_) => std::process::exit(0),
                    Err(e) => eprintln!("restart: cannot start {}: {e}", exe.display()),
                }
            }
            Err(e) => eprintln!("restart: {e}"),
        }
        std::process::exit(1)
    }
    #[cfg(target_os = "macos")]
    if let Ok(bundle) = &target
        && bundle.extension().is_some_and(|e| e == "app")
    {
        let mut open = Command::new("/usr/bin/open");
        open.arg("-n").arg(bundle);
        if !args.is_empty() {
            open.arg("--args").args(&args);
        }
        match open.status() {
            Ok(s) if s.success() => std::process::exit(0),
            Ok(s) => eprintln!("restart: open exited with {s}"),
            Err(e) => eprintln!("restart: cannot run open: {e}"),
        }
        std::process::exit(1);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        match target {
            Ok(exe) => {
                let err = Command::new(&exe).args(&args).exec();
                eprintln!("restart: cannot exec {}: {err}", exe.display());
            }
            Err(e) => eprintln!("restart: {e}"),
        }
        std::process::exit(1)
    }
}

/// Windows: remove the previous executable that [`install`] renamed aside (it could not be deleted while it
/// was running). Best effort; call at startup.
pub fn remove_replaced() {
    #[cfg(windows)]
    if let Ok(exe) = std::env::current_exe() {
        let old = exe.with_extension("exe.old");
        if old.exists() {
            match fs::remove_file(&old) {
                Ok(()) => sard_core::debug!("removed {}", old.display()),
                Err(e) => sard_core::debug!("cannot remove {} yet: {e}", old.display()),
            }
        }
    }
}

/// A command that doesn't flash a console window when run from the (console-less) tray app on Windows.
fn quiet(program: impl AsRef<std::ffi::OsStr>) -> Command {
    #[allow(unused_mut)]
    let mut c = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(sard_windows::os::CREATE_NO_WINDOW);
    }
    c
}

/// A tool from System32 on Windows (never one from the current directory or PATH).
#[cfg(windows)]
fn system32(exe: &str) -> PathBuf {
    let root = std::env::var_os("SystemRoot").map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from);
    root.join("System32").join(exe)
}

/// `sard update [--check]` and the hidden `sard release-sign` (CI).
pub fn cli(cmd: &str, args: &Args) -> Result<()> {
    if cmd == "release-sign" {
        return release_sign(args);
    }
    let platform = platform().ok_or("automatic updates are not published for this platform")?;
    let manifest = fetch_manifest()?;
    let Some(info) = select(&manifest, VERSION, platform)? else {
        println!("sard {VERSION} is up to date (latest release: {}).", manifest.version);
        return Ok(());
    };
    println!("Update available: sard {VERSION} → {}", info.version);
    if let Some(n) = &info.notes {
        println!("Release notes: {n}");
    }
    if args.flag("check") {
        return Ok(());
    }
    let target = install_target()?;
    println!("Downloading {} ({:.1} MB) ...", info.asset.url, info.asset.size as f64 / 1e6);
    install(&info)?;
    println!("Installed sard {} at {}.", info.version, target.display());
    println!("Restart running sard instances (host, tray) to use it.");
    Ok(())
}

fn base_url() -> String {
    match std::env::var(BASE_URL_ENV) {
        Ok(u) if !u.is_empty() => u.trim_end_matches('/').to_string(),
        _ => DEFAULT_BASE_URL.to_string(),
    }
}

fn fetch_manifest() -> Result<Manifest> {
    let base = base_url();
    if base != DEFAULT_BASE_URL {
        sard_core::info!("using {BASE_URL_ENV}={base}");
    }
    let manifest = curl(&format!("{base}/latest.txt"), MAX_MANIFEST_BYTES, None, 60)?;
    let sig = curl(&format!("{base}/latest.txt.sig"), MAX_SIG_BYTES, None, 60)?;
    parse_verified(&manifest, &sig, &trusted_keys())
}

/// The update for `platform` if `m` is newer than `current`.
fn select(m: &Manifest, current: &str, platform: &str) -> Result<Option<UpdateInfo>> {
    let latest = Version::parse(&m.version).ok_or("bad version in update manifest")?;
    let current = Version::parse(current).ok_or("bad version of this build")?;
    if latest <= current {
        return Ok(None);
    }
    let asset = m
        .assets
        .iter()
        .find(|a| a.platform == platform)
        .ok_or_else(|| format!("release {} has no download for {platform}", m.version))?;
    Ok(Some(UpdateInfo { version: m.version.clone(), notes: m.notes.clone(), asset: asset.clone() }))
}

// ---------------------------------------------------------------------------------------------------------
// Manifest, signatures, versions

fn trusted_keys() -> Vec<[u8; 32]> {
    TRUSTED_KEYS.iter().map(|k| parse_key(k).expect("TRUSTED_KEYS entries are 64 hex digits")).collect()
}

fn parse_key(s: &str) -> Option<[u8; 32]> {
    unhex(s)?.try_into().ok()
}

/// Check the signature first, then parse: unsigned bytes are never interpreted.
fn parse_verified(manifest: &[u8], sig: &[u8], keys: &[[u8; 32]]) -> Result<Manifest> {
    verify_signature(manifest, sig, keys)?;
    parse_manifest(std::str::from_utf8(manifest).map_err(|_| "update manifest is not UTF-8")?)
}

fn verify_signature(manifest: &[u8], sig_file: &[u8], keys: &[[u8; 32]]) -> Result<()> {
    let keys: Vec<VerifyingKey> = keys.iter().filter_map(|k| VerifyingKey::from_bytes(k).ok()).collect();
    let text = std::str::from_utf8(sig_file).unwrap_or("");
    let sigs = text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')).take(8);
    for line in sigs {
        let Some(bytes) = unhex(line).and_then(|b| <[u8; 64]>::try_from(b).ok()) else { continue };
        let sig = Signature::from_bytes(&bytes);
        if keys.iter().any(|k| k.verify_strict(manifest, &sig).is_ok()) {
            return Ok(());
        }
    }
    Err("update manifest signature is invalid: not signed by a trusted sard release key".into())
}

fn parse_manifest(text: &str) -> Result<Manifest> {
    let mut lines = text.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l));
    if lines.next() != Some(MANIFEST_MAGIC) {
        return Err(format!("update manifest does not start with '{MANIFEST_MAGIC}'").into());
    }
    let (mut version, mut notes, mut assets) = (None, None, Vec::<Asset>::new());
    for (n, line) in lines.enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let bad = |what: &str| format!("update manifest line {}: {what}", n + 2);
        let f: Vec<&str> = line.split_ascii_whitespace().collect();
        match f[0] {
            "version" => {
                if f.len() != 2 || Version::parse(f[1]).is_none() {
                    return Err(bad("bad version").into());
                }
                if version.replace(f[1].to_string()).is_some() {
                    return Err(bad("duplicate version").into());
                }
            }
            "notes" if f.len() == 2 && is_web_url(f[1]) => notes = Some(f[1].to_string()),
            "asset" => {
                let [_, platform, sum, size, url] = f[..] else {
                    return Err(bad("expected 'asset <platform> <sha256> <size> <url>'").into());
                };
                let platform_ok = !platform.is_empty()
                    && platform.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_');
                if !platform_ok {
                    return Err(bad("bad platform").into());
                }
                let sha256 = (sum.len() == 64).then(|| unhex(sum)).flatten().and_then(|b| b.try_into().ok());
                let Some(sha256) = sha256 else { return Err(bad("bad sha256").into()) };
                let size = size.parse::<u64>().ok().filter(|s| (1..=MAX_ASSET_BYTES).contains(s));
                let Some(size) = size else { return Err(bad("bad size").into()) };
                if !is_web_url(url) {
                    return Err(bad("asset URL must be http(s)").into());
                }
                if assets.iter().any(|a| a.platform == platform) {
                    return Err(bad("duplicate platform").into());
                }
                assets.push(Asset { platform: platform.to_string(), sha256, size, url: url.to_string() });
            }
            _ => {} // newer field: ignore
        }
    }
    let version = version.ok_or("update manifest has no version")?;
    Ok(Manifest { version, notes, assets })
}

fn is_web_url(s: &str) -> bool {
    s.strip_prefix("https://").or_else(|| s.strip_prefix("http://")).is_some_and(|rest| !rest.is_empty())
}

/// Semantic version (`MAJOR.MINOR.PATCH[-PRE][+BUILD]`, optional leading `v`), ordered per semver 2.0.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Version {
    core: [u64; 3],
    pre: Vec<String>,
}

impl Version {
    fn parse(s: &str) -> Option<Self> {
        let s = s.strip_prefix('v').unwrap_or(s);
        let s = s.split_once('+').map_or(s, |(v, _build)| v);
        let (core, pre) = s.split_once('-').map_or((s, None), |(c, p)| (c, Some(p)));
        let mut parts = core.split('.');
        let mut n = [0u64; 3];
        for x in &mut n {
            let p = parts.next()?;
            if p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) || (p.len() > 1 && p.starts_with('0')) {
                return None;
            }
            *x = p.parse().ok()?;
        }
        if parts.next().is_some() {
            return None;
        }
        let pre: Vec<String> = pre.map_or(Vec::new(), |p| p.split('.').map(str::to_string).collect());
        if pre.iter().any(|i| i.is_empty() || !i.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')) {
            return None;
        }
        Some(Self { core: n, pre })
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        self.core.cmp(&other.core).then_with(|| match (self.pre.is_empty(), other.pre.is_empty()) {
            (true, true) => Ordering::Equal,
            (true, false) => Ordering::Greater, // 1.0.0 > 1.0.0-rc.1
            (false, true) => Ordering::Less,
            (false, false) => {
                for (a, b) in self.pre.iter().zip(&other.pre) {
                    let c = match (a.parse::<u64>(), b.parse::<u64>()) {
                        (Ok(x), Ok(y)) => x.cmp(&y),
                        (Ok(_), Err(_)) => Ordering::Less,
                        (Err(_), Ok(_)) => Ordering::Greater,
                        (Err(_), Err(_)) => a.cmp(b),
                    };
                    if c != Ordering::Equal {
                        return c;
                    }
                }
                self.pre.len().cmp(&other.pre.len())
            }
        })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

fn verify_asset(data: &[u8], asset: &Asset) -> Result<()> {
    if data.len() as u64 != asset.size {
        return Err(format!(
            "download is {} bytes, the signed manifest says {}; not installing",
            data.len(),
            asset.size
        )
        .into());
    }
    if sha256(&[data]) != asset.sha256 {
        return Err("download does not match the SHA-256 in the signed manifest; not installing".into());
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------------------
// Download and install

/// `curl` a URL into memory (`out` = None) or a file. HTTPS only (redirects included), except when
/// `SARD_UPDATE_URL` points at a plain-HTTP test server.
fn curl(url: &str, max_bytes: u64, out: Option<&Path>, timeout_secs: u32) -> Result<Vec<u8>> {
    if !is_web_url(url) {
        return Err(format!("refusing to download {url}").into());
    }
    let proto = if base_url().starts_with("http://") { "=http,https" } else { "=https" };
    #[cfg(windows)]
    let program = system32("curl.exe");
    #[cfg(not(windows))]
    let program = if cfg!(target_os = "macos") { "/usr/bin/curl" } else { "curl" };
    let mut cmd = quiet(program);
    cmd.args(["--fail", "--silent", "--show-error", "--location", "--max-redirs", "8", "--retry", "2"])
        .args(["--proto", proto, "--proto-redir", proto, "--connect-timeout", "20"])
        .arg("--max-time")
        .arg(timeout_secs.to_string())
        .arg("--max-filesize")
        .arg(max_bytes.to_string())
        .arg("--user-agent")
        .arg(format!("sard/{VERSION}"))
        .arg("--output")
        .arg(out.map_or(Path::new("-"), |p| p))
        .arg(url)
        .stdin(Stdio::null());
    let o = cmd.output().map_err(|e| format!("cannot run curl: {e}"))?;
    if !o.status.success() {
        let msg = String::from_utf8_lossy(&o.stderr);
        return Err(format!("downloading {url} failed: {}", msg.trim()).into());
    }
    if o.stdout.len() as u64 > max_bytes {
        return Err(format!("{url} is larger than {max_bytes} bytes").into());
    }
    Ok(o.stdout)
}

fn download_verified(asset: &Asset, dest: &Path) -> Result<()> {
    curl(&asset.url, asset.size, Some(dest), 1800)?;
    let mut data = Vec::with_capacity(asset.size as usize);
    fs::File::open(dest)?.take(asset.size + 1).read_to_end(&mut data)?;
    let ok = verify_asset(&data, asset);
    if ok.is_err() {
        let _ = fs::remove_file(dest);
    }
    ok
}

/// A private scratch directory, removed with everything in it on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new_in(parent: &Path, prefix: &str) -> Result<Self> {
        let path = parent.join(format!("{prefix}-{}", hex(&sard_core::rand::bytes::<6>())));
        #[cfg_attr(not(unix), allow(unused_mut))]
        let mut b = fs::DirBuilder::new();
        #[cfg(unix)]
        b.mode(0o700);
        b.create(&path).map_err(|e| format!("cannot create {}: {e}", path.display()))?;
        Ok(Self(path))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn current_exe() -> Result<PathBuf> {
    let exe = std::env::current_exe().map_err(|e| format!("cannot locate the sard executable: {e}"))?;
    // Windows: no canonicalize (it returns `\\?\` paths); the path is already absolute.
    #[cfg(windows)]
    let exe = Ok(exe);
    #[cfg(unix)]
    let exe = {
        use std::os::unix::ffi::OsStrExt;
        // Linux reports "<path> (deleted)" once the file has been replaced.
        match exe.as_os_str().as_bytes().strip_suffix(b" (deleted)") {
            Some(p) => Ok(PathBuf::from(std::ffi::OsStr::from_bytes(p))),
            None => Ok(fs::canonicalize(&exe).unwrap_or(exe)),
        }
    };
    exe
}

/// `…/Name.app` for an executable at `…/Name.app/Contents/MacOS/<exe>`.
fn enclosing_bundle(exe: &Path) -> Option<PathBuf> {
    let macos = exe.parent().filter(|p| p.file_name().is_some_and(|n| n == "MacOS"))?;
    let contents = macos.parent().filter(|p| p.file_name().is_some_and(|n| n == "Contents"))?;
    contents.parent().filter(|p| p.extension().is_some_and(|e| e == "app")).map(Path::to_path_buf)
}

#[cfg(windows)]
fn writable(dir: &Path) -> bool {
    let probe = dir.join(format!(".sard-write-test-{}", hex(&sard_core::rand::bytes::<4>())));
    let ok = fs::OpenOptions::new().write(true).create_new(true).open(&probe).is_ok();
    let _ = fs::remove_file(&probe);
    ok
}

#[cfg(unix)]
fn writable(dir: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c) = std::ffi::CString::new(dir.as_os_str().as_bytes()) else { return false };
    // SAFETY: valid NUL-terminated path.
    unsafe { libc::access(c.as_ptr(), libc::W_OK) == 0 }
}

fn run(cmd: &mut Command) -> Result<()> {
    let o = cmd.stdin(Stdio::null()).output().map_err(|e| format!("cannot run {:?}: {e}", cmd.get_program()))?;
    if o.status.success() {
        Ok(())
    } else {
        Err(format!("{:?} failed: {}", cmd.get_program(), String::from_utf8_lossy(&o.stderr).trim()).into())
    }
}

/// The only entry of `dir` (archives contain exactly one top-level directory / bundle).
fn single_entry(dir: &Path) -> Result<PathBuf> {
    let mut entries = fs::read_dir(dir)?.filter_map(|e| e.ok()).map(|e| e.path());
    match (entries.next(), entries.next()) {
        (Some(p), None) if fs::symlink_metadata(&p)?.is_dir() => Ok(p),
        _ => Err("unexpected archive layout".into()),
    }
}

/// Run the new binary's `version` before switching to it: catches a build that doesn't start on this machine
/// and a release whose binary doesn't carry the advertised version (which would update forever).
fn check_runs(bin: &Path, version: &str) -> Result<()> {
    let meta = fs::symlink_metadata(bin).map_err(|e| format!("{}: {e}", bin.display()))?;
    if !meta.is_file() {
        return Err(format!("{} is not a regular file", bin.display()).into());
    }
    let mut child = quiet(bin)
        .arg("version")
        .env("SARD_LOG", "error")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("cannot start the new version: {e}"))?;
    let deadline = Instant::now() + Duration::from_secs(30);
    while child.try_wait()?.is_none() {
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err("the new version did not start".into());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let out = child.wait_with_output()?;
    let printed = String::from_utf8_lossy(&out.stdout);
    if !out.status.success() || printed.trim() != format!("sard {version}") {
        return Err(format!("the new version does not run correctly (printed {:?})", printed.trim()).into());
    }
    Ok(())
}

/// Windows: unpack `sard-<version>-windows-x86_64/sard.exe` from the zip and put it in place of the running
/// executable, which Windows lets us rename (not overwrite or delete) while it runs. The old one is left as
/// `sard.exe.old` and removed at the next start ([`remove_replaced`]).
#[cfg(windows)]
fn install_windows(info: &UpdateInfo, exe: &Path) -> Result<()> {
    let dir = exe.parent().ok_or("executable has no parent directory")?;
    let tmp = TempDir::new_in(&std::env::temp_dir(), "sard-update")?;
    let zip = tmp.0.join("update.zip");
    download_verified(&info.asset, &zip)?;
    let x = tmp.0.join("x");
    fs::create_dir(&x)?;
    run(quiet(system32("tar.exe")).arg("-xf").arg(&zip).arg("-C").arg(&x))?;
    let new = single_entry(&x)?.join("sard.exe");
    check_runs(&new, &info.version)?;

    // Stage next to the target so the final rename stays on one volume.
    let staged = dir.join(format!(".sard-update-{}.exe", hex(&sard_core::rand::bytes::<6>())));
    fs::copy(&new, &staged).map_err(|e| format!("cannot write {}: {e}", staged.display()))?;
    let old = exe.with_extension("exe.old");
    let _ = fs::remove_file(&old);
    if let Err(e) = fs::rename(exe, &old) {
        let _ = fs::remove_file(&staged);
        return Err(format!("cannot move {} aside: {e}", exe.display()).into());
    }
    if let Err(e) = fs::rename(&staged, exe) {
        let _ = fs::rename(&old, exe);
        let _ = fs::remove_file(&staged);
        return Err(format!("cannot replace {}: {e}", exe.display()).into());
    }
    Ok(())
}

#[cfg(all(unix, not(target_os = "macos")))]
fn install_binary(info: &UpdateInfo, exe: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let dir = exe.parent().ok_or("executable has no parent directory")?;
    let tmp = TempDir::new_in(&std::env::temp_dir(), "sard-update")?;
    let tarball = tmp.0.join("update.tar.gz");
    download_verified(&info.asset, &tarball)?;
    let x = tmp.0.join("x");
    fs::create_dir(&x)?;
    run(Command::new("tar").arg("-xzf").arg(&tarball).arg("-C").arg(&x).arg("--no-same-owner"))?;
    let new = single_entry(&x)?.join("sard");
    check_runs(&new, &info.version)?;

    // Stage next to the target so the final rename is atomic (same filesystem).
    let staged = dir.join(format!(".sard-update-{}", hex(&sard_core::rand::bytes::<6>())));
    let result = (|| -> io::Result<()> {
        let mode = fs::metadata(exe).map_or(0o755, |m| m.permissions().mode() & 0o7777);
        let mut f = fs::OpenOptions::new().write(true).create_new(true).mode(0o700).open(&staged)?;
        io::copy(&mut fs::File::open(&new)?, &mut f)?;
        f.set_permissions(fs::Permissions::from_mode(mode))?;
        f.sync_all()?;
        fs::rename(&staged, exe)
    })();
    if let Err(e) = result {
        let _ = fs::remove_file(&staged);
        return Err(format!("cannot replace {}: {e}", exe.display()).into());
    }
    if let Ok(d) = fs::File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn install_bundle(info: &UpdateInfo, bundle: &Path) -> Result<()> {
    let parent = bundle.parent().ok_or("bundle has no parent directory")?;
    // Staged next to the bundle (same volume) so it can be swapped in with a rename.
    let stage = TempDir::new_in(parent, ".sard-update")?;
    let zip = stage.0.join("update.zip");
    download_verified(&info.asset, &zip)?;
    let x = stage.0.join("x");
    run(Command::new("/usr/bin/ditto").arg("-x").arg("-k").arg(&zip).arg(&x))?;
    let new_app = single_entry(&x)?;
    let plist = new_app.join("Contents/Info.plist");
    let id = Command::new("/usr/bin/plutil")
        .args(["-extract", "CFBundleIdentifier", "raw", "-o", "-"])
        .arg(&plist)
        .output()?;
    if String::from_utf8_lossy(&id.stdout).trim() != BUNDLE_ID {
        return Err("downloaded app has an unexpected bundle identifier".into());
    }
    run(Command::new("/usr/bin/codesign").args(["--verify", "--strict"]).arg(&new_app))
        .map_err(|e| format!("downloaded app's code signature is broken: {e}"))?;
    check_runs(&new_app.join("Contents/MacOS/sard"), &info.version)?;

    // Atomic swap: afterwards `new_app` holds the old bundle, removed with the stage directory.
    if let Err(e) = swap_paths(&new_app, bundle) {
        sard_core::debug!("RENAME_SWAP failed ({e}), replacing in two steps");
        let old = stage.0.join("old.app");
        fs::rename(bundle, &old).map_err(|e| format!("cannot move {} aside: {e}", bundle.display()))?;
        if let Err(e) = fs::rename(&new_app, bundle) {
            let _ = fs::rename(&old, bundle);
            return Err(format!("cannot install {}: {e}", bundle.display()).into());
        }
    }
    // Refresh LaunchServices' idea of the bundle (version, icon); best effort.
    let lsregister =
        "/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister";
    let _ = Command::new(lsregister).arg("-f").arg(bundle).stdout(Stdio::null()).stderr(Stdio::null()).status();
    Ok(())
}

#[cfg(target_os = "macos")]
fn swap_paths(a: &Path, b: &Path) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let a = std::ffi::CString::new(a.as_os_str().as_bytes())?;
    let b = std::ffi::CString::new(b.as_os_str().as_bytes())?;
    // SAFETY: valid NUL-terminated paths.
    if unsafe { libc::renamex_np(a.as_ptr(), b.as_ptr(), libc::RENAME_SWAP) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

// ---------------------------------------------------------------------------------------------------------
// Release signing (CI): `sard release-sign <key-file | -> <manifest>`, `sard release-sign --keygen <file>`

/// Private keys: one hex ed25519 seed per line; `#` comments and blank lines ignored.
fn parse_key_file(text: &str) -> Result<Vec<SigningKey>> {
    let keys: Option<Vec<SigningKey>> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| parse_key(l).map(|seed| SigningKey::from_bytes(&seed)))
        .collect();
    match keys {
        Some(k) if !k.is_empty() => Ok(k),
        _ => Err("signing key file must contain one or more 64-hex-digit ed25519 seeds".into()),
    }
}

fn sign_manifest(manifest: &[u8], keys: &[SigningKey]) -> String {
    keys.iter().map(|k| hex(&k.sign(manifest).to_bytes()) + "\n").collect()
}

fn release_sign(args: &Args) -> Result<()> {
    if let Some(out) = args.value("keygen") {
        return keygen(Path::new(out));
    }
    let (Some(key_file), Some(manifest_path)) = (args.positional(0), args.positional(1)) else {
        return Err(
            "usage: sard release-sign <key-file | -> <manifest>   |   sard release-sign --keygen <key-file>".into()
        );
    };
    let key_text = if key_file == "-" {
        let mut s = String::new();
        io::stdin().read_to_string(&mut s)?;
        s
    } else {
        fs::read_to_string(key_file).map_err(|e| format!("{key_file}: {e}"))?
    };
    let keys = parse_key_file(&key_text)?;
    let manifest = fs::read(manifest_path).map_err(|e| format!("{manifest_path}: {e}"))?;
    let m = parse_manifest(std::str::from_utf8(&manifest).map_err(|_| "manifest is not UTF-8")?)?;
    let sig = sign_manifest(&manifest, &keys);
    // Only publish what the shipped binaries will accept.
    verify_signature(&manifest, sig.as_bytes(), &trusted_keys()).map_err(
        |_| "none of the signing keys matches a public key in TRUSTED_KEYS (crates/sard/src/update.rs) of this build",
    )?;
    let sig_path = format!("{manifest_path}.sig");
    fs::write(&sig_path, &sig)?;
    let fps: Vec<String> = keys.iter().map(|k| fingerprint(&k.verifying_key().to_bytes())).collect();
    println!(
        "signed {manifest_path} (version {}, {} assets) with {}; wrote {sig_path}",
        m.version,
        m.assets.len(),
        fps.join(", ")
    );
    Ok(())
}

fn keygen(path: &Path) -> Result<()> {
    let seed: [u8; 32] = sard_core::rand::bytes();
    let public = SigningKey::from_bytes(&seed).verifying_key().to_bytes();
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        let mut b = fs::DirBuilder::new();
        b.recursive(true);
        #[cfg(unix)]
        b.mode(0o700);
        b.create(dir)?;
    }
    let contents = format!(
        "# sard update signing key (ed25519 seed). SECRET: it can push updates to every sard install.\n\
         # public key {} (fingerprint {})\n{}\n",
        hex(&public),
        fingerprint(&public),
        hex(&seed)
    );
    let mut o = fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    o.mode(0o600);
    let mut f = o.open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    io::Write::write_all(&mut f, contents.as_bytes())?;
    f.sync_all()?;
    println!("wrote private key to {}", path.display());
    println!("public key:  {}", hex(&public));
    println!("fingerprint: {}", fingerprint(&public));
    println!("add the public key to TRUSTED_KEYS in crates/sard/src/update.rs");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_key(n: u8) -> SigningKey {
        SigningKey::from_bytes(&[n; 32])
    }

    fn sample(sha: &[u8; 32]) -> String {
        format!(
            "{MANIFEST_MAGIC}\n# comment\nversion 0.2.0\nnotes https://example.com/notes\n\
             asset linux-x86_64 {} 1234 https://example.com/sard-0.2.0-linux-x86_64.tar.gz\n\
             asset macos-arm64 {} 99 https://example.com/sard-0.2.0-macos-arm64.zip\nfuture-field ignored\n",
            hex(sha),
            hex(&[0xab; 32])
        )
    }

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap_or_else(|| panic!("parse {s}"))
    }

    #[test]
    fn embedded_keys_are_valid_points() {
        for k in trusted_keys() {
            let k = VerifyingKey::from_bytes(&k).expect("embedded key is a valid ed25519 public key");
            assert!(!k.is_weak(), "embedded key is a small-order point");
        }
    }

    #[test]
    fn signed_manifest_roundtrip() {
        let key = test_key(7);
        let text = sample(&[1; 32]);
        let sig = sign_manifest(text.as_bytes(), std::slice::from_ref(&key));
        let m = parse_verified(text.as_bytes(), sig.as_bytes(), &[key.verifying_key().to_bytes()]).unwrap();
        assert_eq!(m.version, "0.2.0");
        assert_eq!(m.notes.as_deref(), Some("https://example.com/notes"));
        assert_eq!(m.assets.len(), 2);
        assert_eq!(
            m.assets[0],
            Asset {
                platform: "linux-x86_64".into(),
                sha256: [1; 32],
                size: 1234,
                url: "https://example.com/sard-0.2.0-linux-x86_64.tar.gz".into()
            }
        );
    }

    #[test]
    fn tampered_or_foreign_signatures_rejected() {
        let key = test_key(7);
        let trusted = [key.verifying_key().to_bytes()];
        let text = sample(&[1; 32]);
        let sig = sign_manifest(text.as_bytes(), &[key]);
        // One flipped character anywhere in the manifest.
        for i in [0, 20, text.len() - 2] {
            let mut bad = text.clone().into_bytes();
            bad[i] ^= 1;
            assert!(parse_verified(&bad, sig.as_bytes(), &trusted).is_err(), "tamper at {i}");
        }
        let swapped = text.replace("0.2.0", "9.9.9");
        assert!(parse_verified(swapped.as_bytes(), sig.as_bytes(), &trusted).is_err());
        // Signed by an untrusted key.
        let other = sign_manifest(text.as_bytes(), &[test_key(8)]);
        assert!(parse_verified(text.as_bytes(), other.as_bytes(), &trusted).is_err());
        // Garbage, empty and truncated signature files.
        for s in ["", "zz\n", &sig[..100], "# only a comment\n"] {
            assert!(parse_verified(text.as_bytes(), s.as_bytes(), &trusted).is_err(), "sig {s:?}");
        }
        // Flipped signature bit.
        let mut flipped = sig.clone().into_bytes();
        flipped[10] = if flipped[10] == b'0' { b'1' } else { b'0' };
        assert!(parse_verified(text.as_bytes(), &flipped, &trusted).is_err());
    }

    #[test]
    fn rotation_any_trusted_signature_suffices() {
        let (old, new) = (test_key(1), test_key(2));
        let text = sample(&[3; 32]);
        let both = sign_manifest(text.as_bytes(), &[old.clone(), new.clone()]);
        assert!(parse_verified(text.as_bytes(), both.as_bytes(), &[old.verifying_key().to_bytes()]).is_ok());
        assert!(parse_verified(text.as_bytes(), both.as_bytes(), &[new.verifying_key().to_bytes()]).is_ok());
        let old_only = sign_manifest(text.as_bytes(), &[old]);
        assert!(parse_verified(text.as_bytes(), old_only.as_bytes(), &[new.verifying_key().to_bytes()]).is_err());
    }

    #[test]
    fn malformed_manifests_rejected() {
        let good = sample(&[1; 32]);
        let h = hex(&[1; 32]);
        let cases = [
            good.replace(MANIFEST_MAGIC, "sard-update 2"),
            good.replacen(MANIFEST_MAGIC, "", 1),
            good.replace("version 0.2.0", "version banana"),
            good.replace("version 0.2.0\n", ""),
            good.replace("version 0.2.0", "version 0.2.0\nversion 0.3.0"),
            good.replace(&h, &h[..62]),
            good.replace(&h, &"g".repeat(64)),
            good.replace(" 1234 ", " 0 "),
            good.replace(" 1234 ", " -5 "),
            good.replace("https://example.com/sard-0.2.0-linux", "file:///etc/sard-0.2.0-linux"),
            good.replace("macos-arm64", "linux-x86_64"),
            good.replace("asset linux-x86_64", "asset Linux/../x"),
            good.replace(" 1234 https", " 1234"),
        ];
        for (i, c) in cases.iter().enumerate() {
            assert!(parse_manifest(c).is_err(), "case {i} accepted:\n{c}");
        }
        assert!(parse_manifest(&good.replace('\n', "\r\n")).is_ok(), "CRLF tolerated");
    }

    #[test]
    fn wrong_checksum_or_size_rejected() {
        let data = b"pretend this is a tarball".to_vec();
        let asset = Asset {
            platform: "linux-x86_64".into(),
            sha256: sha256(&[&data]),
            size: data.len() as u64,
            url: String::new(),
        };
        assert!(verify_asset(&data, &asset).is_ok());
        let mut bad = data.clone();
        bad[3] ^= 0x20;
        assert!(verify_asset(&bad, &asset).is_err(), "same size, different content");
        assert!(verify_asset(&data[..data.len() - 1], &asset).is_err(), "truncated");
        assert!(verify_asset(&data, &Asset { sha256: [0; 32], ..asset.clone() }).is_err(), "wrong sha256");
    }

    #[test]
    fn version_ordering() {
        let ordered = [
            "0.0.9",
            "0.1.0-alpha",
            "0.1.0-alpha.1",
            "0.1.0-alpha.beta",
            "0.1.0-beta",
            "0.1.0-beta.2",
            "0.1.0-beta.11",
            "0.1.0-rc.1",
            "0.1.0",
            "0.1.1",
            "0.2.0",
            "0.10.0",
            "1.0.0",
            "10.0.0",
        ];
        for w in ordered.windows(2) {
            assert!(v(w[0]) < v(w[1]), "{} < {}", w[0], w[1]);
        }
        assert_eq!(v("v1.2.3"), v("1.2.3"));
        assert_eq!(v("1.2.3+build.5").cmp(&v("1.2.3")), Ordering::Equal);
        for bad in ["", "1", "1.2", "1.2.3.4", "01.2.3", "1.2.x", "1.2.3-", "1.2.3-a..b", "1.2.3-a_b", " 1.2.3"] {
            assert!(Version::parse(bad).is_none(), "accepted {bad:?}");
        }
        assert!(Version::parse(VERSION).is_some(), "crate version parses");
    }

    #[test]
    fn select_only_newer_for_this_platform() {
        let m = parse_manifest(&sample(&[1; 32])).unwrap();
        let u = select(&m, "0.1.0", "linux-x86_64").unwrap().unwrap();
        assert_eq!((u.version.as_str(), u.asset.size), ("0.2.0", 1234));
        assert_eq!(select(&m, "0.1.0", "macos-arm64").unwrap().unwrap().asset.size, 99);
        assert!(select(&m, "0.2.0", "linux-x86_64").unwrap().is_none(), "same version");
        assert!(select(&m, "0.3.0", "linux-x86_64").unwrap().is_none(), "never downgrade");
        assert!(select(&m, "0.2.0-rc.1", "linux-x86_64").unwrap().is_some(), "prerelease → release");
        assert!(select(&m, "0.1.0", "linux-riscv64").is_err(), "newer, but no build for this platform");
    }

    #[test]
    fn key_file_parsing() {
        let k = test_key(5);
        let text = format!("# comment\n\n{}\n  {}  \n", hex(&k.to_bytes()), hex(&test_key(6).to_bytes()));
        let keys = parse_key_file(&text).unwrap();
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0].verifying_key(), k.verifying_key());
        assert!(parse_key_file("# nothing\n").is_err());
        assert!(parse_key_file("abcd\n").is_err());
    }

    #[test]
    fn bundle_detection() {
        assert_eq!(
            enclosing_bundle(Path::new("/Applications/Sard.app/Contents/MacOS/sard")),
            Some(PathBuf::from("/Applications/Sard.app"))
        );
        assert_eq!(enclosing_bundle(Path::new("/usr/local/bin/sard")), None);
        assert_eq!(enclosing_bundle(Path::new("/x/Sard/Contents/MacOS/sard")), None);
    }
}

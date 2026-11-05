# Releasing sard

Releases are built and published by GitHub Actions (`.github/workflows/release.yml`)
when a `v*` tag is pushed. Each release has these assets:

| Asset | What |
|---|---|
| `sard-<version>-linux-x86_64.tar.gz` | Linux binary + `install.sh` + desktop entry + icons |
| `sard-<version>-macos-arm64.zip` | `Sard.app` (Developer ID signed, notarized, stapled) |
| `latest.txt`, `latest.txt.sig` | Signed update manifest read by `sard update` and the tray |
| `SHA256SUMS` | Checksums of everything above |

## Cutting a release

1. Bump the version in the root `Cargo.toml` (`[workspace.package] version`), then run
   `cargo build` so `Cargo.lock` picks up the new version. Commit both to `main`.
   The version must be newer than the last release, or no installation will update to it.
2. Tag that commit and push the tag:

   ```sh
   git tag -a v0.2.0 -m "sard 0.2.0"
   git push origin v0.2.0
   ```

3. Watch the run (`gh run watch`, or the Actions tab). It fails early if the tag doesn't
   match the crate version.
4. When it finishes, the release is live and installed copies pick it up through
   `releases/latest/download/latest.txt`.

Versions with a pre-release part (`0.3.0-rc.1`, tag `v0.3.0-rc.1`) are published as
GitHub *pre-releases*. GitHub's "latest release" skips those, so the updater never offers
them.

**Test runs:** a tag containing `-test` (for example `v0.0.0-test.1`) runs the whole
pipeline but only creates a **draft** release. It is built from the current crate
version, and the tag doesn't have to match it. Drafts are invisible to the updater.
Clean up afterwards:

```sh
gh release delete v0.0.0-test.1 --yes --cleanup-tag   # removes the draft and the remote tag
git tag -d v0.0.0-test.1                              # and the local tag
```

## What the workflow does

1. **meta**: reads the crate version and checks it against the tag.
2. **linux** (`ubuntu-latest`): `scripts/package-linux.sh`, then installs the tarball
   into a scratch `$HOME` and runs it as a smoke test. The binary needs no system
   libraries at build time (GPU, PipeWire and Wayland libraries are `dlopen`ed).
   Its glibc floor is whatever `ubuntu-latest` ships; the job logs the highest
   `GLIBC_` symbol version it uses.
3. **macos** (`macos-latest`, Apple Silicon): builds with `MACOSX_DEPLOYMENT_TARGET=13.0`,
   then `scripts/macos-app.sh --release --zip`:
   - Imports the Developer ID certificate into a temporary keychain (deleted at the end
     of the job), and signs with hardened runtime, a secure timestamp and
     `packaging/macos/sard.entitlements`.
   - Submits the zip with `xcrun notarytool` (App Store Connect API key), staples the
     ticket into `Sard.app`, checks it with `spctl`, and zips it again.
   - **Without the Apple secrets** the job still succeeds. It ad-hoc signs the app, and
     the release notes start with a warning (Gatekeeper blocks first launch, and macOS
     forgets the permission grants on every update). With the certificate but no API key,
     the app is Developer ID signed but not notarized, and the notes warn about that too.
4. **release**: writes `latest.txt` (`scripts/release-manifest.sh`) and signs it with the
   `UPDATE_SIGNING_KEY` secret, using the freshly built Linux binary
   (`sard release-sign - latest.txt`). That command refuses to write a signature that
   doesn't verify against the public keys compiled into sard. The job then runs
   `sard update --check` against a local HTTP server holding the new assets, and finally
   `gh release create` with all assets. `gh` uploads into a draft and publishes at the
   end, so `latest.txt` never goes live before the packages it points to.

All binaries built by CI set `SARD_RELEASE_BUILD=1`. Only such builds install updates.
A `cargo build` or a dev `Sard.app` reports available updates but never replaces itself.

## Secrets

Set them under *Settings → Secrets and variables → Actions*, or with `gh secret set`
from a clone of the repo:

| Secret | Required | What |
|---|---|---|
| `UPDATE_SIGNING_KEY` | **yes** (set) | ed25519 private key for `latest.txt` |
| `MACOS_CERTIFICATE_P12` | for signed macOS builds | base64 of the *Developer ID Application* certificate + private key (`.p12`) |
| `MACOS_CERTIFICATE_PASSWORD` | with the above | password chosen when exporting the `.p12` |
| `APPLE_API_KEY_ID` | for notarization | App Store Connect API key ID (10 characters) |
| `APPLE_API_ISSUER_ID` | for notarization | App Store Connect issuer ID (UUID) |
| `APPLE_API_KEY_P8` | for notarization | base64 of the downloaded `AuthKey_<KEYID>.p8` |

### Developer ID Application certificate (`MACOS_CERTIFICATE_P12`, `MACOS_CERTIFICATE_PASSWORD`)

You need a paid Apple Developer Program membership. The *Apple Development* certificate
Xcode creates for free is not enough to distribute outside the App Store.

1. Create the certificate if you don't have one. Either:
   - **Xcode** → Settings → Accounts → select your team → *Manage Certificates…* →
     **+** → *Developer ID Application*; or
   - <https://developer.apple.com/account/resources/certificates/add> → *Developer ID
     Application* → upload a CSR made with Keychain Access → *Certificate Assistant* →
     *Request a Certificate From a Certificate Authority…* (saved to disk) → download
     the `.cer` and double-click it to add it to your login keychain.
2. Open **Keychain Access** → *login* keychain → *My Certificates*. Find
   **Developer ID Application: <Your Name> (<TEAMID>)** and expand it to check that a
   private key sits under it.
3. Right-click the certificate (not the key) → **Export…** → format *Personal Information
   Exchange (.p12)* → save as `DeveloperID.p12` and choose a strong password.
4. Upload both secrets (in a terminal inside a clone of the repo):

   ```sh
   base64 -i DeveloperID.p12 | gh secret set MACOS_CERTIFICATE_P12
   gh secret set MACOS_CERTIFICATE_PASSWORD        # paste the export password when prompted
   rm DeveloperID.p12                              # don't leave the key lying around
   ```

### App Store Connect API key (`APPLE_API_KEY_ID`, `APPLE_API_ISSUER_ID`, `APPLE_API_KEY_P8`)

1. Go to <https://appstoreconnect.apple.com/access/integrations/api> (*Users and Access* →
   *Integrations* → *App Store Connect API* → *Team Keys*). If asked, request access
   first. The Account Holder has to do this once.
2. **Generate API Key** (the **+** button), name it e.g. `sard notarization`, set the
   access role to **Developer**, and click *Generate*.
3. Note the **Key ID** of the new key and the **Issuer ID** shown above the list.
   Click **Download API Key**. You can download the `AuthKey_<KEYID>.p8` only once.
4. Upload:

   ```sh
   gh secret set APPLE_API_KEY_ID      --body "<KEY ID>"
   gh secret set APPLE_API_ISSUER_ID   --body "<ISSUER ID>"
   base64 -i AuthKey_<KEYID>.p8 | gh secret set APPLE_API_KEY_P8
   ```

   Keep the `.p8` somewhere safe (a password manager) or revoke and regenerate the key
   if it is lost.

Check what is configured with `gh secret list`. To check notarization end to end without
publishing anything, push a `-test` tag (see above) and look at the *Notarize and staple*
step.

## How the updater verifies

`crates/sard/src/update.rs` implements it. The format is documented at the top of the file.

- `latest.txt` is a short text file: `sard-update 1`, `version <semver>`,
  `notes <url>`, and one `asset <platform> <sha256> <size> <url>` line per platform.
  `latest.txt.sig` holds one hex ed25519 signature per line over the exact bytes of
  `latest.txt`.
- `sard update` / `update::check()` fetches both files from
  `https://github.com/parodyband/StrangeApeRemoteDesktop/releases/latest/download/` with
  `curl`, HTTPS only (redirects included). It checks the signature against the public keys
  in `TRUSTED_KEYS` before it parses anything, and only then compares the version with
  its own (semver; never downgrades).
- `update::install()` downloads the package for its platform and rejects it unless the
  size and SHA-256 match the signed manifest. It unpacks into a private scratch
  directory and runs the new binary's `sard version`, which must print the advertised
  version. Then it swaps the installation atomically:
  - **Linux**: copies the new binary next to the running executable and `rename`s it
    over (`~/.local/bin/sard` for `install.sh` installs).
  - **macOS**: extracts with `ditto` next to the enclosing `Sard.app`, checks the bundle
    ID and `codesign --verify --strict`, and exchanges the two bundles with
    `renamex_np(RENAME_SWAP)`.

  If anything fails, the old version stays in place. With Developer ID signing, macOS
  permission grants survive updates because they are tied to the team ID, not to the
  exact binary.
- `update::restart()` relaunches with the same arguments (`exec` on Linux, `open -n
  Sard.app --args …` on macOS).
- `SARD_UPDATE_URL=<base url>` fetches `latest.txt`/`.sig` from somewhere else (testing
  with a local server, plain HTTP allowed there). The signature check still uses the
  embedded keys, so this cannot be used to install unsigned code.

The signature is standard Ed25519 (RFC 8032), so you can check it independently. Take the
key line of the private key file (or the public key in `TRUSTED_KEYS`) and use OpenSSL:

```sh
# public key (from TRUSTED_KEYS) → DER, then verify
printf '302a300506032b6570032100%s' dbc65f711fd60624c99d61e768c9d7757dee6f1d8702887eac612307d4ec7724 | xxd -r -p > pub.der
head -n1 latest.txt.sig | xxd -r -p > sig.bin
openssl pkeyutl -verify -pubin -inkey pub.der -keyform DER -rawin -in latest.txt -sigfile sig.bin
```

## The update signing key

- Public key: `dbc65f711fd60624c99d61e768c9d7757dee6f1d8702887eac612307d4ec7724`
  (sard fingerprint `48dd-5388-7357-f2d6`), in `TRUSTED_KEYS` in
  `crates/sard/src/update.rs`.
- Private key: the `UPDATE_SIGNING_KEY` Actions secret. There is an offline backup in
  `~/.config/sard-release/update-signing.key` (mode 0600) on the maintainer's Linux box.
  Keep a second copy somewhere safe (a password manager). **It is never committed.**
  Anyone who has it can push code to every sard installation; anyone who loses it
  (with no backup) can only reach existing installs again by shipping a new key by hand.

Key file format: `#` comments, then one or more lines each holding a 64-hex-digit
ed25519 seed. `sard release-sign` signs with every key in the file (one signature line
each).

### Rotating the key

Installed copies trust only the keys compiled into them. To rotate:

1. Generate a new key (any sard build):

   ```sh
   sard release-sign --keygen ~/.config/sard-release/update-signing-2.key
   ```

   It prints the public key and fingerprint.
2. Add the new public key to `TRUSTED_KEYS` next to the old one, and put **both** private
   keys in the secret, so releases carry both signatures:

   ```sh
   cat ~/.config/sard-release/update-signing.key ~/.config/sard-release/update-signing-2.key \
     | gh secret set UPDATE_SIGNING_KEY
   ```

3. Release. Old installs verify with the old key, and the new build trusts both.
4. Once most installs have updated, remove the old key from `TRUSTED_KEYS` and the
   secret (`gh secret set UPDATE_SIGNING_KEY < ~/.config/sard-release/update-signing-2.key`),
   and release again. Installs older than step 3 then have to be updated by hand.

If the key is **compromised**, do the same, but drop the old key in the same release as
soon as possible. Existing installs keep trusting the old key until they update, so
tell users to update.

## Releasing by hand

The workflow is the reference. To release from your own machines instead:

```sh
# Linux
scripts/package-linux.sh --out dist
# macOS (signs with your Developer ID; notarize with notarytool as in release.yml)
scripts/macos-app.sh --release --zip --out dist
# then, with both packages in dist/:
scripts/release-manifest.sh dist 0.2.0 https://github.com/parodyband/StrangeApeRemoteDesktop/releases/download/v0.2.0
sard release-sign ~/.config/sard-release/update-signing.key dist/latest.txt   # an official (SARD_RELEASE_BUILD) or any build
gh release create v0.2.0 dist/sard-0.2.0-* dist/latest.txt dist/latest.txt.sig --generate-notes
```

### Testing the updater locally

```sh
scripts/package-linux.sh --out /tmp/serve                # build a newer version first
scripts/release-manifest.sh /tmp/serve 0.2.0 http://127.0.0.1:8000
sard release-sign ~/.config/sard-release/update-signing.key /tmp/serve/latest.txt
python3 -m http.server --bind 127.0.0.1 --directory /tmp/serve 8000 &
cp target/release/sard /tmp/bin/sard                     # an older SARD_RELEASE_BUILD=1 build
SARD_UPDATE_URL=http://127.0.0.1:8000 /tmp/bin/sard update
```

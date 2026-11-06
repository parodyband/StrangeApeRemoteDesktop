//! Windows (MSVC) only: embed the app icon and an application manifest into
//! sard.exe. The `.res` file is written directly (the format is simple), so
//! no resource compiler or extra crate is needed; link.exe accepts `.res`
//! files as inputs. Other targets: nothing to do.
//!
//! The manifest asks for Common Controls 6 (TaskDialog), per-monitor DPI
//! awareness v2, the UTF-8 code page and no elevation.

use std::path::Path;

const ICON_SIZES: [u32; 6] = [16, 32, 48, 64, 128, 256];

const MANIFEST: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <description>Sard remote desktop</description>
  <dependency>
    <dependentAssembly>
      <assemblyIdentity type="win32" name="Microsoft.Windows.Common-Controls" version="6.0.0.0"
        processorArchitecture="*" publicKeyToken="6595b64144ccf1df" language="*"/>
    </dependentAssembly>
  </dependency>
  <trustInfo xmlns="urn:schemas-microsoft-com:asm.v3">
    <security>
      <requestedPrivileges>
        <requestedExecutionLevel level="asInvoker" uiAccess="false"/>
      </requestedPrivileges>
    </security>
  </trustInfo>
  <compatibility xmlns="urn:schemas-microsoft-com:compatibility.v1">
    <application>
      <supportedOS Id="{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}"/>
    </application>
  </compatibility>
  <application xmlns="urn:schemas-microsoft-com:asm.v3">
    <windowsSettings>
      <dpiAware xmlns="http://schemas.microsoft.com/SMI/2005/WindowsSettings">true/pm</dpiAware>
      <dpiAwareness xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">PerMonitorV2</dpiAwareness>
      <activeCodePage xmlns="http://schemas.microsoft.com/SMI/2019/WindowsSettings">UTF-8</activeCodePage>
    </windowsSettings>
  </application>
</assembly>
"#;

const RT_ICON: u16 = 3;
const RT_GROUP_ICON: u16 = 14;
const RT_MANIFEST: u16 = 24;

/// One resource entry: header with ordinal type and name, then the data, DWORD-aligned.
fn resource(out: &mut Vec<u8>, kind: u16, id: u16, data: &[u8]) {
    let le32 = |out: &mut Vec<u8>, v: u32| out.extend_from_slice(&v.to_le_bytes());
    let le16 = |out: &mut Vec<u8>, v: u16| out.extend_from_slice(&v.to_le_bytes());
    le32(out, data.len() as u32); // DataSize
    le32(out, 32); // HeaderSize
    le16(out, 0xFFFF);
    le16(out, kind);
    le16(out, 0xFFFF);
    le16(out, id);
    le32(out, 0); // DataVersion
    le16(out, if kind == 0 { 0 } else { 0x1030 }); // MOVEABLE | PURE | DISCARDABLE
    le16(out, if kind == 0 { 0 } else { 0x0409 }); // en-US
    le32(out, 0); // Version
    le32(out, 0); // Characteristics
    out.extend_from_slice(data);
    while !out.len().is_multiple_of(4) {
        out.push(0);
    }
}

fn png_size(png: &[u8]) -> Option<(u32, u32)> {
    if png.len() < 24 || &png[..8] != b"\x89PNG\r\n\x1a\n" {
        return None;
    }
    Some((u32::from_be_bytes(png[16..20].try_into().ok()?), u32::from_be_bytes(png[20..24].try_into().ok()?)))
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let windows_msvc = std::env::var("CARGO_CFG_TARGET_OS").is_ok_and(|o| o == "windows")
        && std::env::var("CARGO_CFG_TARGET_ENV").is_ok_and(|e| e == "msvc");
    if !windows_msvc {
        return;
    }
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let icons = Path::new(&manifest_dir).join("../../assets/icons/app");
    let mut res = Vec::new();
    resource(&mut res, 0, 0, &[]); // the empty first entry every .res starts with
    let mut group = vec![0u8, 0, 1, 0]; // GRPICONDIR: reserved, type 1 (icon)
    group.extend_from_slice(&(ICON_SIZES.len() as u16).to_le_bytes());
    for (i, size) in ICON_SIZES.iter().enumerate() {
        let path = icons.join(format!("sard-{size}.png"));
        println!("cargo:rerun-if-changed={}", path.display());
        let png = std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        assert_eq!(png_size(&png), Some((*size, *size)), "{} is not {size}x{size}", path.display());
        let id = i as u16 + 1;
        resource(&mut res, RT_ICON, id, &png);
        // GRPICONDIRENTRY (14 bytes): width, height (0 = 256), colours, reserved, planes, bpp, size, id.
        group.push(if *size >= 256 { 0 } else { *size as u8 });
        group.push(if *size >= 256 { 0 } else { *size as u8 });
        group.extend_from_slice(&[0, 0]);
        group.extend_from_slice(&1u16.to_le_bytes());
        group.extend_from_slice(&32u16.to_le_bytes());
        group.extend_from_slice(&(png.len() as u32).to_le_bytes());
        group.extend_from_slice(&id.to_le_bytes());
    }
    resource(&mut res, RT_GROUP_ICON, 1, &group);
    resource(&mut res, RT_MANIFEST, 1, MANIFEST.as_bytes());
    let out = Path::new(&std::env::var("OUT_DIR").unwrap()).join("sard.res");
    std::fs::write(&out, res).unwrap();
    println!("cargo:rustc-link-arg-bins={}", out.display());
    // Our manifest is the only one (link.exe would otherwise generate a side-by-side file).
    println!("cargo:rustc-link-arg-bins=/MANIFEST:NO");
}

//! Packages the Windows installer payload (`waystone-host.exe`) and compiles
//! Win32 resources (application icon, version info, UAC `requireAdministrator`
//! + ComCtl32 v6 + PerMonitorV2 manifest) via `zig rc` on Windows targets.

use std::path::{Path, PathBuf};
use std::process::Command;

const ICON_SIZES: &[(u32, &str)] = &[
    (
        16,
        "../waystone-host/src/platform/windows/icons/tray-16.rgba",
    ),
    (
        24,
        "../waystone-host/src/platform/windows/icons/tray-24.rgba",
    ),
    (
        32,
        "../waystone-host/src/platform/windows/icons/tray-32.rgba",
    ),
    (
        48,
        "../waystone-host/src/platform/windows/icons/tray-48.rgba",
    ),
    (
        64,
        "../waystone-host/src/platform/windows/icons/tray-64.rgba",
    ),
];

fn main() {
    println!("cargo:rustc-check-cfg=cfg(embedded_payload)");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=WAYSTONE_HOST_EXE");
    for (_, path) in ICON_SIZES {
        println!("cargo:rerun-if-changed={path}");
    }

    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR is set by cargo"));
    prepare_payload(&out_dir);

    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os == "windows" {
        if let Err(err) = build_windows_resources(&out_dir) {
            println!("cargo:warning=skipping Win32 resource embedding: {err}");
        }
    }
}

fn prepare_payload(out_dir: &Path) {
    let payload_path = out_dir.join("waystone-host.payload");
    if let Ok(exe_path) = std::env::var("WAYSTONE_HOST_EXE") {
        let src = PathBuf::from(&exe_path);
        println!("cargo:rerun-if-changed={}", src.display());
        let bytes = std::fs::read(&src).unwrap_or_else(|e| {
            panic!("WAYSTONE_HOST_EXE={} could not be read: {e}", src.display())
        });
        assert!(
            !bytes.is_empty(),
            "WAYSTONE_HOST_EXE={} is empty",
            src.display()
        );
        std::fs::write(&payload_path, &bytes).expect("writing waystone-host.payload");
        println!("cargo:rustc-cfg=embedded_payload");
    } else {
        // Placeholder so workspace checks/tests succeed without prebuilding the Windows exe.
        std::fs::write(&payload_path, b"").expect("writing placeholder payload");
    }
}

fn build_windows_resources(out_dir: &Path) -> Result<(), String> {
    let ico_path = out_dir.join("waystone.ico");
    let ico_bytes = build_ico()?;
    std::fs::write(&ico_path, ico_bytes).map_err(|e| format!("writing ico: {e}"))?;

    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.1.0".into());
    let parts: Vec<u16> = version
        .split('.')
        .filter_map(|s| s.parse::<u16>().ok())
        .collect();
    let (maj, min, pat) = (
        *parts.first().unwrap_or(&0),
        *parts.get(1).unwrap_or(&1),
        *parts.get(2).unwrap_or(&0),
    );

    let manifest = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <assemblyIdentity version="{maj}.{min}.{pat}.0" processorArchitecture="*" name="Waystone.Setup" type="win32"/>
  <description>Waystone Host Installer</description>
  <trustInfo xmlns="urn:schemas-microsoft-com:asm.v3">
    <security>
      <requestedPrivileges>
        <requestedExecutionLevel level="requireAdministrator" uiAccess="false"/>
      </requestedPrivileges>
    </security>
  </trustInfo>
  <dependency>
    <dependentAssembly>
      <assemblyIdentity type="win32" name="Microsoft.Windows.Common-Controls" version="6.0.0.0" processorArchitecture="*" publicKeyToken="6595b64144ccf1df" language="*"/>
    </dependentAssembly>
  </dependency>
  <application xmlns="urn:schemas-microsoft-com:asm.v3">
    <windowsSettings>
      <dpiAwareness xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">PerMonitorV2</dpiAwareness>
      <dpiAware xmlns="http://schemas.microsoft.com/SMI/2005/WindowsSettings">true/pm</dpiAware>
    </windowsSettings>
  </application>
</assembly>
"#
    );
    let manifest_path = out_dir.join("setup.manifest");
    std::fs::write(&manifest_path, manifest).map_err(|e| format!("writing manifest: {e}"))?;

    let rc = format!(
        r#"#define RT_MANIFEST 24
1 ICON "waystone.ico"
1 RT_MANIFEST "setup.manifest"

1 VERSIONINFO
FILEVERSION {maj},{min},{pat},0
PRODUCTVERSION {maj},{min},{pat},0
FILEFLAGSMASK 0x3fL
FILEFLAGS 0x0L
FILEOS 0x40004L
FILETYPE 0x1L
FILESUBTYPE 0x0L
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904b0"
    BEGIN
      VALUE "CompanyName", "Waystone"
      VALUE "FileDescription", "Waystone Host Setup"
      VALUE "FileVersion", "{maj}.{min}.{pat}.0"
      VALUE "InternalName", "WaystoneSetup"
      VALUE "OriginalFilename", "WaystoneSetup.exe"
      VALUE "ProductName", "Waystone"
      VALUE "ProductVersion", "{maj}.{min}.{pat}.0"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x0409, 1200
  END
END
"#
    );
    let rc_path = out_dir.join("setup.rc");
    std::fs::write(&rc_path, rc).map_err(|e| format!("writing rc: {e}"))?;

    let res_path = out_dir.join("setup.res");
    let status = Command::new("zig")
        .args(["rc", "/fo"])
        .arg(&res_path)
        .arg(&rc_path)
        .current_dir(out_dir)
        .status()
        .map_err(|e| format!("running zig rc: {e}"))?;
    if !status.success() {
        return Err(format!("zig rc exited with {status}"));
    }

    // MinGW/lld-link accepts compiled `.res` files directly as linker inputs.
    println!("cargo:rustc-link-arg={}", res_path.display());
    Ok(())
}

/// Builds a standard multi-resolution 32-bpp RGBA Windows `.ico` file from the
/// raw icon bitmaps already stored in `waystone-host`.
fn build_ico() -> Result<Vec<u8>, String> {
    let mut images: Vec<(u32, Vec<u8>)> = Vec::with_capacity(ICON_SIZES.len());
    for &(size, rel_path) in ICON_SIZES {
        let rgba = std::fs::read(rel_path).map_err(|e| format!("reading {rel_path}: {e}"))?;
        let expected = (size * size * 4) as usize;
        if rgba.len() != expected {
            return Err(format!(
                "{rel_path}: expected {expected} bytes, got {}",
                rgba.len()
            ));
        }
        images.push((size, encode_dib(size, &rgba)));
    }

    let count = images.len() as u16;
    let header_len = 6 + images.len() * 16;
    let mut out =
        Vec::with_capacity(header_len + images.iter().map(|(_, d)| d.len()).sum::<usize>());
    out.extend_from_slice(&0u16.to_le_bytes()); // reserved
    out.extend_from_slice(&1u16.to_le_bytes()); // type = ICO
    out.extend_from_slice(&count.to_le_bytes());

    let mut offset = header_len as u32;
    for (size, data) in &images {
        let dim = if *size >= 256 { 0u8 } else { *size as u8 };
        out.push(dim);
        out.push(dim);
        out.push(0); // color count
        out.push(0); // reserved
        out.extend_from_slice(&1u16.to_le_bytes()); // planes
        out.extend_from_slice(&32u16.to_le_bytes()); // bpp
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
        offset += data.len() as u32;
    }
    for (_, data) in images {
        out.extend_from_slice(&data);
    }
    Ok(out)
}

fn encode_dib(size: u32, rgba: &[u8]) -> Vec<u8> {
    let mask_row = (size as usize).div_ceil(32) * 4;
    let mask_len = mask_row * size as usize;
    let xor_len = (size * size * 4) as usize;
    let mut dib = Vec::with_capacity(40 + xor_len + mask_len);

    // BITMAPINFOHEADER (40 bytes)
    dib.extend_from_slice(&40u32.to_le_bytes()); // biSize
    dib.extend_from_slice(&(size as i32).to_le_bytes()); // biWidth
    dib.extend_from_slice(&((size * 2) as i32).to_le_bytes()); // biHeight (XOR + AND)
    dib.extend_from_slice(&1u16.to_le_bytes()); // biPlanes
    dib.extend_from_slice(&32u16.to_le_bytes()); // biBitCount
    dib.extend_from_slice(&0u32.to_le_bytes()); // biCompression (BI_RGB)
    dib.extend_from_slice(&((xor_len + mask_len) as u32).to_le_bytes()); // biSizeImage
    dib.extend_from_slice(&0i32.to_le_bytes()); // biXPelsPerMeter
    dib.extend_from_slice(&0i32.to_le_bytes()); // biYPelsPerMeter
    dib.extend_from_slice(&0u32.to_le_bytes()); // biClrUsed
    dib.extend_from_slice(&0u32.to_le_bytes()); // biClrImportant

    // Bottom-up BGRA pixels.
    let stride = (size * 4) as usize;
    for y in (0..size as usize).rev() {
        let row = &rgba[y * stride..(y + 1) * stride];
        for px in row.chunks_exact(4) {
            dib.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
        }
    }
    // 1-bpp AND mask (all zero; alpha channel controls transparency).
    dib.resize(dib.len() + mask_len, 0);
    dib
}

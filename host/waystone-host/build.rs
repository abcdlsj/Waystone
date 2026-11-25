//! Compiles the Swift capture layer into a static library and links it in.
//!
//! Keeping the capture code in Swift means the host and the iOS app share one
//! mental model of ScreenCaptureKit and VideoToolbox, and it avoids hand written
//! Objective-C bindings in Rust.

use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=src/platform/macos/swift");
    println!("cargo:rerun-if-changed=build.rs");

    // The Swift layer is the macOS platform; other targets have their own in
    // Rust (src/platform).
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    if target_os == "windows" {
        let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR is set by cargo"));
        if let Err(err) = embed_windows_icon(&out_dir) {
            println!("cargo:warning=skipping Windows icon resource: {err}");
        }
        return;
    }
    if target_os != "macos" {
        return;
    }

    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR is set by cargo"));
    let mut sources: Vec<PathBuf> = std::fs::read_dir("src/platform/macos/swift")
        .expect("src/platform/macos/swift holds the media layer")
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "swift"))
        .collect();
    sources.sort();
    let object = out_dir.join("Capture.o");
    let archive = out_dir.join("libwaystone_capture.a");
    let module_cache = out_dir.join("swift-module-cache");
    std::fs::create_dir_all(&module_cache).ok();

    let arch = match target_arch.as_str() {
        "aarch64" => "arm64",
        "x86_64" => "x86_64",
        other => panic!("unsupported macOS architecture {other}"),
    };

    let status = Command::new("swiftc")
        .args([
            "-O",
            "-parse-as-library",
            "-wmo",
            "-emit-object",
            "-module-name",
            "WaystoneCapture",
            "-target",
            &format!("{arch}-apple-macos13.0"),
            "-o",
        ])
        .arg(&object)
        .args(&sources)
        .env("SWIFT_MODULECACHE_PATH", &module_cache)
        .env("CLANG_MODULE_CACHE_PATH", &module_cache)
        .status()
        .expect("swiftc must be available on a macOS build machine");
    assert!(
        status.success(),
        "swiftc failed to compile the capture layer"
    );

    let status = Command::new("libtool")
        .args(["-static", "-o"])
        .arg(&archive)
        .arg(&object)
        .status()
        .expect("libtool must be available on a macOS build machine");
    assert!(
        status.success(),
        "libtool failed to archive the capture layer"
    );

    println!("cargo:rustc-link-search=native={}", out_dir.display());
    println!("cargo:rustc-link-lib=static=waystone_capture");
    // Swift runtime and the frameworks the capture layer touches.
    println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");
    for framework in [
        "ScreenCaptureKit",
        "VideoToolbox",
        "CoreMedia",
        "CoreVideo",
        "CoreGraphics",
        "AudioToolbox",
        "AVFoundation",
        "ApplicationServices",
        "CoreAudio",
        "IOKit",
        "Foundation",
    ] {
        println!("cargo:rustc-link-lib=framework={framework}");
    }
}

fn embed_windows_icon(out_dir: &std::path::Path) -> Result<(), String> {
    const ICON_SIZES: &[(u32, &str)] = &[
        (16, "src/platform/windows/icons/tray-16.rgba"),
        (24, "src/platform/windows/icons/tray-24.rgba"),
        (32, "src/platform/windows/icons/tray-32.rgba"),
        (48, "src/platform/windows/icons/tray-48.rgba"),
        (64, "src/platform/windows/icons/tray-64.rgba"),
    ];
    for (_, path) in ICON_SIZES {
        println!("cargo:rerun-if-changed={path}");
    }
    let mut images = Vec::with_capacity(ICON_SIZES.len());
    for &(size, rel_path) in ICON_SIZES {
        let rgba = std::fs::read(rel_path).map_err(|e| format!("reading {rel_path}: {e}"))?;
        let mask_row = (size as usize).div_ceil(32) * 4;
        let mask_len = mask_row * size as usize;
        let xor_len = (size * size * 4) as usize;
        let mut dib = Vec::with_capacity(40 + xor_len + mask_len);
        dib.extend_from_slice(&40u32.to_le_bytes());
        dib.extend_from_slice(&(size as i32).to_le_bytes());
        dib.extend_from_slice(&((size * 2) as i32).to_le_bytes());
        dib.extend_from_slice(&1u16.to_le_bytes());
        dib.extend_from_slice(&32u16.to_le_bytes());
        dib.extend_from_slice(&0u32.to_le_bytes());
        dib.extend_from_slice(&((xor_len + mask_len) as u32).to_le_bytes());
        dib.extend_from_slice(&0i32.to_le_bytes());
        dib.extend_from_slice(&0i32.to_le_bytes());
        dib.extend_from_slice(&0u32.to_le_bytes());
        dib.extend_from_slice(&0u32.to_le_bytes());
        let stride = (size * 4) as usize;
        for y in (0..size as usize).rev() {
            let row = &rgba[y * stride..(y + 1) * stride];
            for px in row.chunks_exact(4) {
                dib.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
            }
        }
        dib.resize(dib.len() + mask_len, 0);
        images.push((size, dib));
    }

    let header_len = 6 + images.len() * 16;
    let mut ico =
        Vec::with_capacity(header_len + images.iter().map(|(_, d)| d.len()).sum::<usize>());
    ico.extend_from_slice(&0u16.to_le_bytes());
    ico.extend_from_slice(&1u16.to_le_bytes());
    ico.extend_from_slice(&(images.len() as u16).to_le_bytes());
    let mut offset = header_len as u32;
    for (size, data) in &images {
        let dim = if *size >= 256 { 0u8 } else { *size as u8 };
        ico.extend_from_slice(&[dim, dim, 0, 0]);
        ico.extend_from_slice(&1u16.to_le_bytes());
        ico.extend_from_slice(&32u16.to_le_bytes());
        ico.extend_from_slice(&(data.len() as u32).to_le_bytes());
        ico.extend_from_slice(&offset.to_le_bytes());
        offset += data.len() as u32;
    }
    for (_, data) in images {
        ico.extend_from_slice(&data);
    }

    let ico_path = out_dir.join("waystone.ico");
    std::fs::write(&ico_path, ico).map_err(|e| format!("writing ico: {e}"))?;
    let rc_path = out_dir.join("host.rc");
    std::fs::write(&rc_path, "1 ICON \"waystone.ico\"\n")
        .map_err(|e| format!("writing rc: {e}"))?;
    let res_path = out_dir.join("host.res");
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
    println!("cargo:rustc-link-arg={}", res_path.display());
    Ok(())
}

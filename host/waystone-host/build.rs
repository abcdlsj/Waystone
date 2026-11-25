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

    // Only macOS can build the host agent; other targets get an empty archive so
    // `cargo check --workspace` keeps working everywhere.
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    if target_os != "macos" {
        println!("cargo:warning=capture layer is only built for macOS hosts");
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

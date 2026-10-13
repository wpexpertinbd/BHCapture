fn main() {
    // NOTE: build.rs runs on the HOST, so `cfg!(target_os = ...)` here describes the build
    // machine, not what is being built. CARGO_CFG_TARGET_OS is the target -- using cfg! would
    // make the Windows CI job try to compile Objective-C.
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os != "macos" {
        return;
    }

    println!("cargo:rerun-if-changed=src/macos/sck.m");
    println!("cargo:rerun-if-changed=src/macos/sck.h");
    println!("cargo:rerun-if-changed=src/macos/rec.m");
    println!("cargo:rerun-if-changed=src/macos/rec.h");
    println!("cargo:rerun-if-changed=src/macos/cam.m");
    println!("cargo:rerun-if-changed=src/macos/cam.h");

    cc::Build::new()
        .file("src/macos/sck.m")
        .file("src/macos/rec.m")
        .file("src/macos/cam.m")
        .flag("-fobjc-arc")
        .flag("-mmacosx-version-min=14.0")
        .flag("-Wall")
        .compile("bhc_sck");

    // `@available(macOS 15, *)` compiles to a call into clang's runtime (`__isPlatformVersionAtLeast`).
    // Without LTO the linker found it on its own; with LTO (the small release build) it does not, so
    // link clang's runtime explicitly.
    if let Ok(out) = std::process::Command::new("xcrun").args(["clang", "--print-runtime-dir"]).output() {
        let dir = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !dir.is_empty() && std::path::Path::new(&dir).join("libclang_rt.osx.a").exists() {
            println!("cargo:rustc-link-search=native={dir}");
            println!("cargo:rustc-link-lib=static=clang_rt.osx");
        }
    }

    for fw in ["ScreenCaptureKit", "CoreGraphics", "CoreFoundation", "Foundation", "CoreMedia", "CoreVideo", "AppKit", "AVFoundation", "Accelerate", "Vision", "IOKit", "ServiceManagement"] {
        println!("cargo:rustc-link-lib=framework={fw}");
    }
}

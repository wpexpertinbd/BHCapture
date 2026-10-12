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

    cc::Build::new()
        .file("src/macos/sck.m")
        .flag("-fobjc-arc")
        .flag("-mmacosx-version-min=14.0")
        .flag("-Wall")
        .compile("bhc_sck");

    for fw in ["ScreenCaptureKit", "CoreGraphics", "CoreFoundation", "Foundation", "CoreMedia", "CoreVideo", "AppKit"] {
        println!("cargo:rustc-link-lib=framework={fw}");
    }
}

fn main() {
    // Reuse 0b's capture shim so the overlay's visibility can be cross-checked against a
    // real screen capture, not just against callbacks firing.
    let shim = "../0b-sck-capture/src/sck_shim.m";
    println!("cargo:rerun-if-changed={shim}");
    cc::Build::new()
        .file(shim)
        .include("../0b-sck-capture/src")
        .flag("-fobjc-arc")
        .flag("-mmacosx-version-min=12.3")
        .compile("sck_shim");
    for fw in ["ScreenCaptureKit", "CoreGraphics", "CoreFoundation", "Foundation", "CoreMedia", "CoreVideo"] {
        println!("cargo:rustc-link-lib=framework={fw}");
    }
    tauri_build::build();
}

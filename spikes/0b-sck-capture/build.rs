fn main() {
    println!("cargo:rerun-if-changed=src/sck_shim.m");
    println!("cargo:rerun-if-changed=src/sck_shim.h");

    cc::Build::new()
        .file("src/sck_shim.m")
        .flag("-fobjc-arc")
        .flag("-mmacosx-version-min=12.3")
        .flag("-Wall")
        .compile("sck_shim");

    for fw in ["ScreenCaptureKit", "CoreGraphics", "CoreFoundation", "Foundation", "CoreMedia", "CoreVideo"] {
        println!("cargo:rustc-link-lib=framework={fw}");
    }
}

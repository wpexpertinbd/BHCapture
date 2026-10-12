fn main() {
    // Reuse 0b's shim rather than duplicating it -- the capture path is already proved.
    let shim = "../0b-sck-capture/src/sck_shim.m";
    println!("cargo:rerun-if-changed={shim}");
    println!("cargo:rerun-if-env-changed=BHC_MARKER");

    // The marker is what makes the rebuild produce a DIFFERENT binary. Without it a
    // byte-identical rebuild would keep its TCC grant even when ad-hoc signed, and the
    // whole test would pass for the wrong reason.
    let marker = std::env::var("BHC_MARKER").unwrap_or_else(|_| "unset".into());
    println!("cargo:rustc-env=BHC_MARKER={marker}");

    cc::Build::new()
        .file(shim)
        .include("../0b-sck-capture/src")
        .flag("-fobjc-arc")
        .flag("-mmacosx-version-min=12.3")
        .define("BHC_MARKER_PAD", Some(format!("\"{marker}\"").as_str()))
        .compile("sck_shim");

    for fw in ["ScreenCaptureKit", "CoreGraphics", "CoreFoundation", "Foundation", "CoreMedia", "CoreVideo"] {
        println!("cargo:rustc-link-lib=framework={fw}");
    }
}

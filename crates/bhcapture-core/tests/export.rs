use bhcapture_core::export::*;
use std::path::Path;

/// 4x2: opaque red, opaque green, 50%-transparent blue, fully transparent black (top row),
/// and white / grey / opaque black / fully transparent (bottom row).
fn sample() -> (Vec<u8>, u32, u32) {
    let px: [[u8; 4]; 8] = [
        [255, 0, 0, 255], [0, 255, 0, 255], [0, 0, 255, 128], [0, 0, 0, 0],
        [255, 255, 255, 255], [128, 128, 128, 255], [0, 0, 0, 255], [10, 20, 30, 0],
    ];
    (px.concat(), 4, 2)
}

#[test]
fn section0_png_round_trips_exactly() {
    let (rgba, w, h) = sample();
    let bytes = encode(&rgba, w, h, ImageFormat::Png, 90).unwrap();
    let back = image::load_from_memory(&bytes).unwrap().to_rgba8();
    assert_eq!((back.width(), back.height()), (w, h));
    assert_eq!(back.into_raw(), rgba, "PNG must be lossless, including alpha");
}

#[test]
fn every_format_produces_a_file_its_own_decoder_accepts() {
    let (rgba, w, h) = sample();
    for &f in ALL_FORMATS {
        let bytes = encode(&rgba, w, h, f, 90).unwrap_or_else(|e| panic!("{f:?}: {e}"));
        let guessed = image::guess_format(&bytes).unwrap_or_else(|e| panic!("{f:?}: {e}"));
        let want = match f {
            ImageFormat::Png => image::ImageFormat::Png, ImageFormat::Jpeg => image::ImageFormat::Jpeg,
            ImageFormat::Webp => image::ImageFormat::WebP, ImageFormat::Tiff => image::ImageFormat::Tiff,
            ImageFormat::Bmp => image::ImageFormat::Bmp, ImageFormat::Gif => image::ImageFormat::Gif,
        };
        assert_eq!(guessed, want, "{f:?} wrote the wrong kind of file");
        let img = image::load_from_memory(&bytes).unwrap_or_else(|e| panic!("{f:?} does not decode: {e}"));
        assert_eq!((img.width(), img.height()), (w, h), "{f:?} changed the size");
    }
}

#[test]
fn lossless_formats_keep_exact_colours() {
    let (rgba, w, h) = sample();
    for f in [ImageFormat::Webp, ImageFormat::Tiff] {
        let back = image::load_from_memory(&encode(&rgba, w, h, f, 90).unwrap()).unwrap().to_rgba8();
        assert_eq!(back.into_raw(), rgba, "{f:?} altered pixels");
    }
}

#[test]
fn jpeg_flattens_transparency_onto_white_not_black() {
    let (rgba, w, h) = sample();
    let img = image::load_from_memory(&encode(&rgba, w, h, ImageFormat::Jpeg, 100).unwrap()).unwrap().to_rgb8();
    // Fully transparent pixels: what is stored under them must NOT show through (that is the
    // "transparent became black" bug); they must come out white.
    for (x, y) in [(3, 0), (3, 1)] {
        let p = img.get_pixel(x, y);
        assert!(p[0] > 235 && p[1] > 235 && p[2] > 235, "transparent pixel at ({x},{y}) is {p:?}, not white");
    }
    // Opaque colours survive (JPEG is lossy, so approximately).
    let red = img.get_pixel(0, 0);
    assert!(red[0] > 200 && red[1] < 60 && red[2] < 60, "red became {red:?}");
}

#[test]
fn jpeg_quality_changes_the_output() {
    // A noisy image, so quality genuinely matters to the size.
    let (w, h) = (64u32, 64u32);
    let rgba: Vec<u8> = (0..w * h).flat_map(|i| {
        let v = (i.wrapping_mul(2654435761) >> 24) as u8; [v, v.wrapping_mul(3), v ^ 0x5a, 255]
    }).collect();
    let lo = encode(&rgba, w, h, ImageFormat::Jpeg, 20).unwrap().len();
    let hi = encode(&rgba, w, h, ImageFormat::Jpeg, 95).unwrap().len();
    assert!(hi > lo, "quality is being ignored: q95 {hi} bytes vs q20 {lo} bytes");
}

#[test]
fn a_typed_extension_picks_the_format_case_insensitively() {
    assert_eq!(ImageFormat::from_path(Path::new("/x/shot.JPG")), Some(ImageFormat::Jpeg));
    assert_eq!(ImageFormat::from_path(Path::new("/x/shot.jpeg")), Some(ImageFormat::Jpeg));
    assert_eq!(ImageFormat::from_path(Path::new("/x/shot.tif")), Some(ImageFormat::Tiff));
    assert_eq!(ImageFormat::from_path(Path::new("/x/shot.webp")), Some(ImageFormat::Webp));
    assert_eq!(ImageFormat::from_path(Path::new("/x/shot.heic")), None, "not offered");
    assert_eq!(ImageFormat::from_path(Path::new("/x/shot")), None);
}

#[test]
fn a_wrong_size_buffer_is_refused_rather_than_misread() {
    assert!(matches!(encode(&[0u8; 10], 4, 2, ImageFormat::Png, 90), Err(ExportError::BadBuffer { .. })));
    assert!(matches!(encode(&[], 0, 0, ImageFormat::Png, 90), Err(ExportError::BadBuffer { .. })));
}

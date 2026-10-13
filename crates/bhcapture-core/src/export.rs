//! Encoding a finished capture into the file format the user chose.
//!
//! Pure Rust (the `image` crate) so macOS and Windows write byte-for-byte the same kind of file.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ImageFormat {
    #[default]
    Png,
    Jpeg,
    Webp,
    Tiff,
    Bmp,
    Gif,
}

pub const ALL_FORMATS: &[ImageFormat] = &[
    ImageFormat::Png, ImageFormat::Jpeg, ImageFormat::Webp,
    ImageFormat::Tiff, ImageFormat::Bmp, ImageFormat::Gif,
];

pub const DEFAULT_JPEG_QUALITY: u8 = 90;

impl ImageFormat {
    /// The extension written for a new file.
    pub fn extension(self) -> &'static str {
        match self {
            ImageFormat::Png => "png", ImageFormat::Jpeg => "jpg", ImageFormat::Webp => "webp",
            ImageFormat::Tiff => "tiff", ImageFormat::Bmp => "bmp", ImageFormat::Gif => "gif",
        }
    }

    /// Every extension that means this format, for Save dialogs and for reading a typed name.
    pub fn extensions(self) -> &'static [&'static str] {
        match self {
            ImageFormat::Png => &["png"],
            ImageFormat::Jpeg => &["jpg", "jpeg"],
            ImageFormat::Webp => &["webp"],
            ImageFormat::Tiff => &["tiff", "tif"],
            ImageFormat::Bmp => &["bmp"],
            ImageFormat::Gif => &["gif"],
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            ImageFormat::Png => "PNG", ImageFormat::Jpeg => "JPEG", ImageFormat::Webp => "WebP",
            ImageFormat::Tiff => "TIFF", ImageFormat::Bmp => "BMP", ImageFormat::Gif => "GIF",
        }
    }

    /// The format a typed file name asks for, if its extension is one we write. Case-insensitive,
    /// so `Shot.JPG` is a JPEG.
    pub fn from_path(path: &std::path::Path) -> Option<ImageFormat> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        ALL_FORMATS.iter().copied().find(|f| f.extensions().contains(&ext.as_str()))
    }

    pub fn parse(s: &str) -> Option<ImageFormat> {
        let s = s.trim().to_ascii_lowercase();
        ALL_FORMATS.iter().copied().find(|f| f.extensions().contains(&s.as_str()) || f.label().eq_ignore_ascii_case(&s))
    }
}

#[derive(Debug)]
pub enum ExportError {
    BadBuffer { got: usize, need: usize },
    Encode(String),
}

impl std::fmt::Display for ExportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExportError::BadBuffer { got, need } => write!(f, "pixel buffer is {got} bytes, need {need}"),
            ExportError::Encode(e) => write!(f, "could not encode the image: {e}"),
        }
    }
}
impl std::error::Error for ExportError {}

/// Encode tightly packed RGBA8 (straight, not premultiplied) as `format`.
///
/// Formats without transparency (JPEG, and BMP as most viewers read it) are flattened onto WHITE
/// rather than having their alpha silently dropped, which would turn transparent pixels into
/// whatever colour happened to be stored under them -- usually black.
pub fn encode(rgba: &[u8], width: u32, height: u32, format: ImageFormat, jpeg_quality: u8)
    -> Result<Vec<u8>, ExportError>
{
    let need = width as usize * height as usize * 4;
    if rgba.len() != need || width == 0 || height == 0 {
        return Err(ExportError::BadBuffer { got: rgba.len(), need });
    }
    let img = image::RgbaImage::from_raw(width, height, rgba.to_vec())
        .ok_or(ExportError::BadBuffer { got: rgba.len(), need })?;
    let mut out = std::io::Cursor::new(Vec::new());
    let enc = |e: image::ImageError| ExportError::Encode(e.to_string());

    match format {
        ImageFormat::Jpeg | ImageFormat::Bmp => {
            let flat = flatten_on_white(&img);
            if format == ImageFormat::Jpeg {
                let q = jpeg_quality.clamp(1, 100);
                image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, q)
                    .encode_image(&flat).map_err(enc)?;
            } else {
                flat.write_to(&mut out, image::ImageFormat::Bmp).map_err(enc)?;
            }
        }
        ImageFormat::Png => img.write_to(&mut out, image::ImageFormat::Png).map_err(enc)?,
        ImageFormat::Webp => img.write_to(&mut out, image::ImageFormat::WebP).map_err(enc)?,
        ImageFormat::Tiff => img.write_to(&mut out, image::ImageFormat::Tiff).map_err(enc)?,
        ImageFormat::Gif => img.write_to(&mut out, image::ImageFormat::Gif).map_err(enc)?,
    }
    Ok(out.into_inner())
}

fn flatten_on_white(img: &image::RgbaImage) -> image::RgbImage {
    let mut out = image::RgbImage::new(img.width(), img.height());
    for (x, y, p) in img.enumerate_pixels() {
        let a = p[3] as u32;
        let mix = |c: u8| ((c as u32 * a + 255 * (255 - a) + 127) / 255) as u8;
        out.put_pixel(x, y, image::Rgb([mix(p[0]), mix(p[1]), mix(p[2])]));
    }
    out
}

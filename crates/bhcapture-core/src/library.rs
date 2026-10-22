//! The start window's material (decision 40): the latest saved captures in a folder, small previews
//! of them, and opening an image file for editing. Portable; the shell supplies every path.
//!
//! Every file here is untrusted input -- a folder can hold anything, and Open Image… accepts any
//! file the user picks -- so decoding runs under hard limits: a crafted or simply enormous image
//! is refused rather than allowed to take all memory.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::export::ImageFormat;

/// Images larger than this on either side are refused (a 16K-wide screenshot is still far below).
pub const MAX_SIDE: u32 = 20_000;
/// And larger than this in total: 100 megapixels is ~400 MB as RGBA, several 6K screens side by side.
/// Checked from the header BEFORE decoding, so a tiny, highly compressed file cannot claim gigabytes.
pub const MAX_PIXELS: u64 = 100_000_000;
/// Decoder allocation ceiling.
const MAX_ALLOC: u64 = 512 << 20;
/// Files bigger than this are not opened at all.
const MAX_FILE_BYTES: u64 = 256 << 20;

#[derive(Debug)]
pub enum LibraryError {
    Io(std::io::Error),
    TooLarge,
    Decode(String),
}

impl std::fmt::Display for LibraryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LibraryError::Io(e) => write!(f, "{e}"),
            LibraryError::TooLarge => write!(f, "this image is too large to open"),
            LibraryError::Decode(e) => write!(f, "this file could not be read as an image ({e})"),
        }
    }
}
impl std::error::Error for LibraryError {}

/// The newest `max` image files directly inside `dir` (not its subfolders), newest first.
/// Only regular files: a symlink is skipped, so the list cannot be pointed somewhere else.
/// A missing or unreadable folder is simply an empty list.
pub fn recent_images(dir: &Path, max: usize) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut v: Vec<(SystemTime, PathBuf)> = rd.flatten().filter_map(|e| {
        let md = std::fs::symlink_metadata(e.path()).ok()?;
        if !md.file_type().is_file() { return None; }
        let p = e.path();
        ImageFormat::from_path(&p)?;
        Some((md.modified().unwrap_or(SystemTime::UNIX_EPOCH), p))
    }).collect();
    v.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    v.into_iter().take(max).map(|(_, p)| p).collect()
}

/// Open for reading. `follow_links: false` refuses a symlink at `path` (O_NOFOLLOW), so an entry in
/// the save folder swapped for a link after it was listed cannot redirect the read elsewhere. A file
/// the user picked in a dialog may be a link on purpose, so that path follows.
fn open_read(path: &Path, follow_links: bool) -> std::io::Result<std::fs::File> {
    let mut o = std::fs::OpenOptions::new();
    o.read(true);
    #[cfg(unix)]
    if !follow_links {
        use std::os::unix::fs::OpenOptionsExt;
        o.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(not(unix))]
    let _ = follow_links;
    let f = o.open(path)?;
    if !f.metadata()?.is_file() {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "not a file"));
    }
    Ok(f)
}

/// Decode an image file to straight RGBA8, under the limits above.
pub fn decode_file(path: &Path) -> Result<(Vec<u8>, u32, u32), LibraryError> { decode(path, true) }

/// [`decode_file`] for a file listed by [`recent_images`]: a symlink put there since is refused.
pub fn decode_listed(path: &Path) -> Result<(Vec<u8>, u32, u32), LibraryError> { decode(path, false) }

fn decode(path: &Path, follow_links: bool) -> Result<(Vec<u8>, u32, u32), LibraryError> {
    let f = open_read(path, follow_links).map_err(LibraryError::Io)?;
    if f.metadata().map_err(LibraryError::Io)?.len() > MAX_FILE_BYTES { return Err(LibraryError::TooLarge); }
    let mut bytes = Vec::new();
    f.take(MAX_FILE_BYTES + 1).read_to_end(&mut bytes).map_err(LibraryError::Io)?;
    if bytes.len() as u64 > MAX_FILE_BYTES { return Err(LibraryError::TooLarge); }
    // The header's size first: the full decode (plus its RGBA copy) is only attempted when the
    // result fits the budget.
    let (hw, hh) = image::ImageReader::new(std::io::Cursor::new(&bytes[..]))
        .with_guessed_format().map_err(LibraryError::Io)?
        .into_dimensions().map_err(|e| LibraryError::Decode(e.to_string()))?;
    if hw > MAX_SIDE || hh > MAX_SIDE || hw as u64 * hh as u64 > MAX_PIXELS { return Err(LibraryError::TooLarge); }
    let mut r = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format().map_err(LibraryError::Io)?;
    let mut lim = image::Limits::default();
    lim.max_image_width = Some(MAX_SIDE);
    lim.max_image_height = Some(MAX_SIDE);
    lim.max_alloc = Some(MAX_ALLOC);
    r.limits(lim);
    let img = r.decode().map_err(|e| match e {
        image::ImageError::Limits(_) => LibraryError::TooLarge,
        e => LibraryError::Decode(e.to_string()),
    })?;
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    if w == 0 || h == 0 { return Err(LibraryError::Decode("empty image".into())); }
    Ok((rgba.into_raw(), w, h))
}

/// A JPEG preview no wider/taller than `max_side`, plus the original size.
/// Only for files listed by [`recent_images`] (links refused, see [`decode_listed`]).
pub fn thumbnail_jpeg(path: &Path, max_side: u32) -> Result<(Vec<u8>, u32, u32), LibraryError> {
    let (rgba, w, h) = decode_listed(path)?;
    let img = image::RgbaImage::from_raw(w, h, rgba).ok_or_else(|| LibraryError::Decode("bad buffer".into()))?;
    let k = (max_side as f64 / w.max(h) as f64).min(1.0);
    let (tw, th) = (((w as f64 * k).round() as u32).max(1), ((h as f64 * k).round() as u32).max(1));
    let small = image::imageops::thumbnail(&img, tw, th);
    let (sw, sh) = small.dimensions();
    let jpeg = crate::export::encode(small.as_raw(), sw, sh, ImageFormat::Jpeg, 80)
        .map_err(|e| LibraryError::Decode(e.to_string()))?;
    Ok((jpeg, w, h))
}

/// The name for an edited copy saved next to `original`: "<stem> edited", never the original's own
/// name (decision 40: an opened image is never overwritten by Save).
pub fn edited_stem(original: &Path) -> String {
    let stem = original.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let stem = stem.trim();
    if stem.is_empty() { "edited".into() } else { format!("{stem} edited") }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("bhc-lib-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn png(path: &Path, w: u32, h: u32) {
        let bytes = crate::export::encode(&vec![200u8; (w * h * 4) as usize], w, h, ImageFormat::Png, 90).unwrap();
        std::fs::write(path, bytes).unwrap();
    }

    #[test]
    fn lists_only_images_newest_first_and_skips_symlinks_and_folders() {
        let d = scratch("list");
        png(&d.join("a.png"), 4, 4);
        std::thread::sleep(std::time::Duration::from_millis(20));
        png(&d.join("b.png"), 4, 4);
        std::fs::write(d.join("notes.txt"), "x").unwrap();
        std::fs::create_dir(d.join("sub.png")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(d.join("a.png"), d.join("link.png")).unwrap();
        let names: Vec<_> = recent_images(&d, 10).iter().map(|p| p.file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(names, ["b.png", "a.png"]);
        assert_eq!(recent_images(&d, 1).len(), 1);
        assert!(recent_images(&d.join("missing"), 5).is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn decodes_a_real_image_and_refuses_junk_and_giants() {
        let d = scratch("dec");
        png(&d.join("ok.png"), 3, 2);
        let (rgba, w, h) = decode_file(&d.join("ok.png")).unwrap();
        assert_eq!((w, h, rgba.len()), (3, 2, 24));
        std::fs::write(d.join("junk.png"), b"not an image at all").unwrap();
        assert!(matches!(decode_file(&d.join("junk.png")), Err(LibraryError::Decode(_))));
        // A header claiming 30000 x 30000: refused before any pixel buffer exists.
        let mut hdr = Vec::new();
        {
            use image::ImageEncoder;
            let small = vec![0u8; 4];
            image::codecs::png::PngEncoder::new(&mut hdr).write_image(&small, 1, 1, image::ExtendedColorType::Rgba8).unwrap();
        }
        // Patch the IHDR width/height (bytes 16..24) to 30000 x 30000; the CRC is wrong too, so a
        // decoder either refuses on the limits or on the corrupt header -- never allocates 3.6 GB.
        hdr[16..20].copy_from_slice(&30000u32.to_be_bytes());
        hdr[20..24].copy_from_slice(&30000u32.to_be_bytes());
        std::fs::write(d.join("giant.png"), &hdr).unwrap();
        assert!(decode_file(&d.join("giant.png")).is_err());
        let (jpeg, ow, oh) = thumbnail_jpeg(&d.join("ok.png"), 320).unwrap();
        assert_eq!((ow, oh), (3, 2));
        assert_eq!(&jpeg[..2], &[0xFF, 0xD8]);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A 1x1 PNG whose header is rewritten to claim `w` x `h`, with a VALID header checksum, so the
    /// claim reaches the size check rather than being rejected as a corrupt file.
    fn png_claiming(w: u32, h: u32) -> Vec<u8> {
        use image::ImageEncoder;
        let mut f = Vec::new();
        image::codecs::png::PngEncoder::new(&mut f).write_image(&[0u8; 4], 1, 1, image::ExtendedColorType::Rgba8).unwrap();
        f[16..20].copy_from_slice(&w.to_be_bytes());
        f[20..24].copy_from_slice(&h.to_be_bytes());
        // CRC-32 over the chunk type + data (bytes 12..29), stored at 29..33.
        let mut crc = 0xFFFF_FFFFu32;
        for &b in &f[12..29] {
            crc ^= b as u32;
            for _ in 0..8 { crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 }; }
        }
        f[29..33].copy_from_slice(&(!crc).to_be_bytes());
        f
    }

    #[test]
    fn a_small_file_claiming_a_huge_image_is_refused_before_decoding() {
        let d = scratch("budget");
        // 15000 x 9000 = 135 MP: each side is allowed, the total is not.
        std::fs::write(d.join("wide.png"), png_claiming(15_000, 9_000)).unwrap();
        assert!(matches!(decode_file(&d.join("wide.png")), Err(LibraryError::TooLarge)));
        std::fs::write(d.join("tall.png"), png_claiming(1, 30_000)).unwrap();
        assert!(matches!(decode_file(&d.join("tall.png")), Err(LibraryError::TooLarge)));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[cfg(unix)]
    #[test]
    fn a_listed_file_swapped_for_a_link_is_refused_but_a_picked_link_opens() {
        let d = scratch("nofollow");
        let outside = d.join("outside.png");
        png(&outside, 2, 2);
        let listed = d.join("shot.png");
        std::os::unix::fs::symlink(&outside, &listed).unwrap();
        assert!(decode_listed(&listed).is_err(), "a recent-captures entry must not follow a link");
        assert!(thumbnail_jpeg(&listed, 64).is_err());
        assert_eq!(decode_file(&listed).unwrap().1, 2, "a file the user picked may be a link");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn an_edited_copy_never_takes_the_original_name() {
        assert_eq!(edited_stem(Path::new("/x/BHCapture 2026-10-22 at 15.09.20.jpg")), "BHCapture 2026-10-22 at 15.09.20 edited");
        assert_eq!(edited_stem(Path::new("/x/.png")), ".png edited");
        assert_eq!(edited_stem(Path::new("/x/ ")), "edited");
    }
}

//! Camera frames for the recording bubble (decision 34/35): crop to the bubble's shape, scale,
//! mirror, enhance, and replace the background behind the person.
//!
//! Portable on purpose: macOS gets the person mask from Vision, Windows/Linux from a small
//! segmentation model, but the compositing is this one function everywhere, so the bubble looks the
//! same on every OS. Output is straight RGBA, ready for a canvas.

/// What goes behind the person.
#[derive(Clone, Debug, PartialEq)]
pub enum Background {
    /// The camera as it is.
    Keep,
    /// The room, blurred (radius in output pixels).
    Blur(u32),
    /// A solid colour, RGB.
    Color([u8; 3]),
    /// An image, RGBA, already at the OUTPUT size (`bg_w` x `bg_h` = `out_w` x `out_h`).
    Image(Vec<u8>),
}

#[derive(Clone, Debug)]
pub struct FrameOptions {
    /// Output size in pixels (the bubble's backing size).
    pub out_w: u32,
    pub out_h: u32,
    /// Flip left-right, like a mirror (what people expect from their own camera).
    pub mirror: bool,
    /// Lift a dark or flat picture: auto levels + a little saturation.
    pub enhance: bool,
    pub background: Background,
}

/// Source frame: BGRA rows of `stride` bytes.
pub struct Source<'a> {
    pub bgra: &'a [u8],
    pub w: u32,
    pub h: u32,
    pub stride: usize,
}

/// Person mask: one byte per pixel, 255 = person, at its own size.
pub struct Mask<'a> {
    pub data: &'a [u8],
    pub w: u32,
    pub h: u32,
    pub stride: usize,
}

/// The centred crop of a `w`x`h` frame that has the output's aspect ratio.
pub fn crop_rect(w: u32, h: u32, out_w: u32, out_h: u32) -> (f64, f64, f64, f64) {
    let (w, h) = (w as f64, h as f64);
    let want = out_w.max(1) as f64 / out_h.max(1) as f64;
    if w / h > want {
        let cw = h * want;
        ((w - cw) / 2.0, 0.0, cw, h)
    } else {
        let ch = w / want;
        (0.0, (h - ch) / 2.0, w, ch)
    }
}

/// Built-in backgrounds (decision 35): drawn here, so no image files ship with the app.
pub const PRESETS: &[(&str, &str)] = &[
    ("studio", "Studio grey"), ("ocean", "Ocean"), ("sunset", "Sunset"),
    ("forest", "Forest"), ("night", "Night"), ("blush", "Blush"),
];

/// "#rrggbb" -> RGB.
pub fn parse_hex_color(s: &str) -> Option<[u8; 3]> {
    let h = s.strip_prefix('#')?;
    if h.len() != 6 || !h.is_ascii() { return None; }
    let v = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).ok();
    Some([v(0)?, v(2)?, v(4)?])
}

/// A preset background at `w` x `h`, RGBA: a soft diagonal gradient with a light vignette.
pub fn preset_image(name: &str, w: u32, h: u32) -> Option<Vec<u8>> {
    let (a, b): ([f32; 3], [f32; 3]) = match name {
        "studio" => ([92., 96., 104.], [38., 40., 46.]),
        "ocean" => ([40., 140., 200.], [16., 40., 92.]),
        "sunset" => ([250., 160., 90.], [150., 50., 110.]),
        "forest" => ([90., 150., 90.], [20., 60., 40.]),
        "night" => ([60., 60., 130.], [10., 12., 30.]),
        "blush" => ([250., 200., 210.], [190., 140., 200.]),
        _ => return None,
    };
    let (w, h) = (w.max(1) as usize, h.max(1) as usize);
    let mut out = vec![0u8; w * h * 4];
    for y in 0..h { for x in 0..w {
        let t = ((x as f32 / w as f32) * 0.35 + (y as f32 / h as f32) * 0.65).clamp(0.0, 1.0);
        let (dx, dy) = (x as f32 / w as f32 - 0.5, y as f32 / h as f32 - 0.4);
        let vig = 1.0 - (dx * dx + dy * dy).sqrt() * 0.35;
        let i = (y * w + x) * 4;
        for c in 0..3 { out[i + c] = ((a[c] + (b[c] - a[c]) * t) * vig).clamp(0.0, 255.0) as u8; }
        out[i + 3] = 255;
    } }
    Some(out)
}

/// The user's own background picture, decoded and scaled to cover the output. `None` when the file
/// is missing, unreadable or not a supported picture (PNG, JPEG, WebP, BMP, TIFF, GIF).
pub fn background_from_file(path: &std::path::Path, out_w: u32, out_h: u32) -> Option<Vec<u8>> {
    // A regular file only (not a FIFO or device, which could block or never end), not huge.
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > 40 * 1024 * 1024 { return None; }
    let mut reader = image::ImageReader::open(path).ok()?.with_guessed_format().ok()?;
    // The file size does not bound the pixel count: cap the dimensions and the memory (a 40 MB
    // picture can otherwise declare a 50+ megapixel image).
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(8192);
    limits.max_image_height = Some(8192);
    limits.max_alloc = Some(300 * 1024 * 1024);
    reader.limits(limits);
    let img = reader.decode().ok()?;
    // Shrink first (the bubble is at most 1280x720), so the copies below stay small.
    let img = img.thumbnail(out_w.saturating_mul(2).max(64), out_h.saturating_mul(2).max(64)).to_rgba8();
    cover(img.as_raw(), img.width(), img.height(), out_w, out_h)
}

/// Scale an RGBA picture to cover `out_w` x `out_h` (centre crop), like CSS `object-fit: cover`.
pub fn cover(rgba: &[u8], w: u32, h: u32, out_w: u32, out_h: u32) -> Option<Vec<u8>> {
    if w == 0 || h == 0 || rgba.len() < w as usize * h as usize * 4 { return None; }
    // Treat it as BGRA-agnostic: render() swaps R and B, so swap them first to cancel out.
    let swapped: Vec<u8> = rgba.chunks_exact(4).flat_map(|p| [p[2], p[1], p[0], 255]).collect();
    render(&Source { bgra: &swapped, w, h, stride: w as usize * 4 }, None,
           &FrameOptions { out_w, out_h, mirror: false, enhance: false, background: Background::Keep })
}

/// Render one frame. Returns RGBA, `out_w * out_h * 4` bytes. A source or mask that is too small
/// for its stated size yields `None` rather than reading out of bounds.
pub fn render(src: &Source, mask: Option<&Mask>, opt: &FrameOptions) -> Option<Vec<u8>> {
    let (ow, oh) = (opt.out_w as usize, opt.out_h as usize);
    if ow == 0 || oh == 0 || src.w == 0 || src.h == 0 { return None; }
    if src.stride < src.w as usize * 4 || src.bgra.len() < src.stride * (src.h as usize - 1) + src.w as usize * 4 { return None; }
    let mask = mask.filter(|m| m.w > 0 && m.h > 0 && m.stride >= m.w as usize
        && m.data.len() >= m.stride * (m.h as usize - 1) + m.w as usize);
    let (cx, cy, cw, ch) = crop_rect(src.w, src.h, opt.out_w, opt.out_h);

    // Scale the crop down with an area average (bilinear would alias at 1080p -> 230 px).
    let mut out = vec![0u8; ow * oh * 4];
    let sx = cw / ow as f64;
    let sy = ch / oh as f64;
    for oy in 0..oh {
        let y0 = cy + oy as f64 * sy;
        let (ya, yb) = (y0.floor() as usize, ((y0 + sy).ceil() as usize).min(src.h as usize).max(y0.floor() as usize + 1));
        for ox in 0..ow {
            let fx = if opt.mirror { ow - 1 - ox } else { ox };
            let x0 = cx + fx as f64 * sx;
            let (xa, xb) = (x0.floor() as usize, ((x0 + sx).ceil() as usize).min(src.w as usize).max(x0.floor() as usize + 1));
            // Sample at most 4x4 points per output pixel: enough to average, cheap at any size.
            let stepy = ((yb - ya) / 4).max(1);
            let stepx = ((xb - xa) / 4).max(1);
            let (mut b, mut g, mut r, mut n) = (0u32, 0u32, 0u32, 0u32);
            let mut y = ya;
            while y < yb {
                let row = y * src.stride;
                let mut x = xa;
                while x < xb {
                    let i = row + x * 4;
                    b += src.bgra[i] as u32; g += src.bgra[i + 1] as u32; r += src.bgra[i + 2] as u32; n += 1;
                    x += stepx;
                }
                y += stepy;
            }
            let o = (oy * ow + ox) * 4;
            out[o] = (r / n) as u8; out[o + 1] = (g / n) as u8; out[o + 2] = (b / n) as u8; out[o + 3] = 255;
        }
    }

    if opt.enhance { enhance(&mut out); }

    let Some(m) = mask else { return Some(out) };
    if opt.background == Background::Keep { return Some(out); }

    // The mask, sampled at the output's pixels (it covers the same full frame as the source).
    let alpha: Vec<u8> = (0..oh * ow).map(|i| {
        let (ox, oy) = (i % ow, i / ow);
        let fx = if opt.mirror { ow - 1 - ox } else { ox };
        let x = ((cx + (fx as f64 + 0.5) * sx) / src.w as f64 * m.w as f64) as usize;
        let y = ((cy + (oy as f64 + 0.5) * sy) / src.h as f64 * m.h as f64) as usize;
        m.data[y.min(m.h as usize - 1) * m.stride + x.min(m.w as usize - 1)]
    }).collect();
    // Soften the edge a little so the cut-out does not look pasted on.
    let alpha = box_blur_1ch(&alpha, ow, oh, 1);

    let behind: Vec<u8> = match &opt.background {
        Background::Keep => unreachable!(),
        Background::Blur(r) => box_blur_rgba(&out, ow, oh, (*r).max(1) as usize),
        Background::Color(c) => (0..ow * oh).flat_map(|_| [c[0], c[1], c[2], 255]).collect(),
        Background::Image(img) if img.len() == ow * oh * 4 => img.clone(),
        Background::Image(_) => return Some(out), // wrong size: show the camera rather than garbage
    };
    for i in 0..ow * oh {
        let a = alpha[i] as u32;
        for c in 0..3 {
            let o = i * 4 + c;
            out[o] = ((out[o] as u32 * a + behind[o] as u32 * (255 - a) + 127) / 255) as u8;
        }
    }
    Some(out)
}

/// Auto levels (1st-99th percentile of luma stretched to 8-247, gain capped at 2.2 so noise does
/// not turn into snow), a mid-tone lift for dark rooms, plus 12% saturation.
fn enhance(px: &mut [u8]) {
    let mut hist = [0u32; 256];
    for p in px.chunks_exact(4) {
        hist[((p[0] as u32 * 77 + p[1] as u32 * 150 + p[2] as u32 * 29) >> 8) as usize] += 1;
    }
    let total: u32 = hist.iter().sum();
    let pct = |q: f64| { let t = (total as f64 * q) as u32; let mut acc = 0; for (i, h) in hist.iter().enumerate() { acc += h; if acc > t { return i as f64; } } 255.0 };
    let (lo, hi) = (pct(0.01), pct(0.99));
    let gain = (239.0 / (hi - lo).max(1.0)).clamp(1.0, 2.2);
    let lift = 8.0 - lo * gain;
    let level = |i: f64| (i * gain + lift).clamp(0.0, 255.0);
    // A dark room: also lift the mid-tones (gamma) so the mean lands near 110, capped at 0.55 so
    // shadows do not turn into grey noise. A well-lit picture is left alone.
    let mean = hist.iter().enumerate().map(|(i, h)| level(i as f64) * *h as f64).sum::<f64>() / total.max(1) as f64;
    let gamma = if mean > 1.0 && mean < 100.0 { ((110.0f64 / 255.0).ln() / (mean / 255.0).ln()).clamp(0.55, 1.0) } else { 1.0 };
    let mut lut = [0u8; 256];
    for (i, v) in lut.iter_mut().enumerate() { *v = (255.0 * (level(i as f64) / 255.0).powf(gamma)).round().clamp(0.0, 255.0) as u8; }
    for p in px.chunks_exact_mut(4) {
        let (r, g, b) = (lut[p[0] as usize] as f32, lut[p[1] as usize] as f32, lut[p[2] as usize] as f32);
        let l = 0.299 * r + 0.587 * g + 0.114 * b;
        let s = |c: f32| (l + (c - l) * 1.12).round().clamp(0.0, 255.0) as u8;
        p[0] = s(r); p[1] = s(g); p[2] = s(b);
    }
}

/// Three box passes approximate a Gaussian.
fn box_blur_rgba(px: &[u8], w: usize, h: usize, r: usize) -> Vec<u8> {
    let mut a = px.to_vec();
    let mut tmp = vec![0u8; a.len()];
    for _ in 0..3 {
        blur_pass(&a, &mut tmp, w, h, r, 4, true);
        blur_pass(&tmp, &mut a, w, h, r, 4, false);
    }
    a
}

fn box_blur_1ch(px: &[u8], w: usize, h: usize, r: usize) -> Vec<u8> {
    let mut a = px.to_vec();
    let mut tmp = vec![0u8; a.len()];
    blur_pass(&a, &mut tmp, w, h, r, 1, true);
    blur_pass(&tmp, &mut a, w, h, r, 1, false);
    a
}

/// One running-sum box blur pass over rows (`horizontal`) or columns, edges clamped.
fn blur_pass(src: &[u8], dst: &mut [u8], w: usize, h: usize, r: usize, ch: usize, horizontal: bool) {
    let (len, lines) = if horizontal { (w, h) } else { (h, w) };
    let idx = |line: usize, k: usize| if horizontal { (line * w + k) * ch } else { (k * w + line) * ch };
    let win = (2 * r + 1) as u32;
    for line in 0..lines {
        for c in 0..ch {
            let at = |k: isize| src[idx(line, k.clamp(0, len as isize - 1) as usize) + c] as u32;
            let mut sum: u32 = (-(r as isize)..=r as isize).map(at).sum();
            for k in 0..len {
                dst[idx(line, k) + c] = (sum / win) as u8;
                sum += at(k as isize + r as isize + 1);
                sum -= at(k as isize - r as isize);
            }
        }
        if ch == 4 { for k in 0..len { dst[idx(line, k) + 3] = 255; } }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 64x48 frame: left half blue, right half red (BGRA).
    fn frame() -> Vec<u8> {
        let mut v = vec![0u8; 64 * 48 * 4];
        for y in 0..48 { for x in 0..64 {
            let i = (y * 64 + x) * 4;
            v[i..i + 4].copy_from_slice(if x < 32 { &[255, 0, 0, 255] } else { &[0, 0, 255, 255] });
        } }
        v
    }
    fn opts(w: u32, h: u32) -> FrameOptions {
        FrameOptions { out_w: w, out_h: h, mirror: false, enhance: false, background: Background::Keep }
    }
    fn src(v: &[u8]) -> Source<'_> { Source { bgra: v, w: 64, h: 48, stride: 256 } }

    #[test]
    fn square_output_crops_the_centre_and_converts_to_rgba() {
        assert_eq!(crop_rect(64, 48, 10, 10), (8.0, 0.0, 48.0, 48.0));
        let f = frame();
        let out = render(&src(&f), None, &opts(10, 10)).unwrap();
        assert_eq!(out.len(), 400);
        assert_eq!(&out[0..4], &[0, 0, 255, 255], "left is blue, as RGBA");
        assert_eq!(&out[(9) * 4..(9) * 4 + 4], &[255, 0, 0, 255], "right is red");
    }

    #[test]
    fn mirror_swaps_left_and_right() {
        let f = frame();
        let out = render(&src(&f), None, &FrameOptions { mirror: true, ..opts(10, 10) }).unwrap();
        assert_eq!(&out[0..4], &[255, 0, 0, 255]);
    }

    #[test]
    fn background_is_replaced_only_where_the_mask_says_not_person() {
        let f = frame();
        // Person = top half of the frame.
        let mut m = vec![0u8; 16 * 12];
        for y in 0..6 { for x in 0..16 { m[y * 16 + x] = 255; } }
        let mask = Mask { data: &m, w: 16, h: 12, stride: 16 };
        let out = render(&src(&f), Some(&mask), &FrameOptions { background: Background::Color([0, 255, 0]), ..opts(8, 8) }).unwrap();
        assert_eq!(&out[0..3], &[0, 0, 255], "top-left: the person (blue) is kept");
        let bl = (7 * 8) * 4;
        assert_eq!(&out[bl..bl + 3], &[0, 255, 0], "bottom-left: background replaced by green");
    }

    #[test]
    fn bad_buffers_are_refused_not_read_past() {
        let short = vec![0u8; 100];
        assert!(render(&Source { bgra: &short, w: 64, h: 48, stride: 256 }, None, &opts(10, 10)).is_none());
        let f = frame();
        let tiny = [255u8; 4];
        let bad_mask = Mask { data: &tiny, w: 16, h: 12, stride: 16 };
        // A bad mask is ignored, not trusted.
        let out = render(&src(&f), Some(&bad_mask), &FrameOptions { background: Background::Color([0, 255, 0]), ..opts(8, 8) }).unwrap();
        assert_eq!(&out[0..3], &[0, 0, 255]);
    }

    #[test]
    fn presets_colours_and_cover() {
        for (id, _) in PRESETS { assert_eq!(preset_image(id, 20, 10).unwrap().len(), 800, "{id}"); }
        assert!(preset_image("nope", 2, 2).is_none());
        assert_eq!(parse_hex_color("#1f6feb"), Some([0x1f, 0x6f, 0xeb]));
        assert_eq!(parse_hex_color("1f6feb"), None);
        assert_eq!(parse_hex_color("#zzzzzz"), None);
        let red: Vec<u8> = (0..16).flat_map(|_| [255, 0, 0, 255]).collect();
        assert_eq!(&cover(&red, 4, 4, 2, 2).unwrap()[0..4], &[255, 0, 0, 255], "colours survive (no R/B swap)");
    }

    #[test]
    fn a_background_picture_must_be_a_real_picture_file() {
        let dir = std::env::temp_dir();
        assert!(background_from_file(&dir, 8, 8).is_none(), "a folder is refused");
        assert!(background_from_file(&dir.join("bhc-no-such-file.png"), 8, 8).is_none());
        let junk = dir.join(format!("bhc-junk-{}.png", std::process::id()));
        std::fs::write(&junk, b"not a picture").unwrap();
        assert!(background_from_file(&junk, 8, 8).is_none(), "garbage is refused, not guessed");
        let _ = std::fs::remove_file(&junk);
    }

    #[test]
    fn enhance_lifts_a_dark_flat_picture() {
        let mut px: Vec<u8> = (0..100).flat_map(|i| { let v = 20 + (i % 30) as u8; [v, v, v, 255] }).collect();
        let before: u32 = px.chunks(4).map(|p| p[0] as u32).sum();
        enhance(&mut px);
        let after: u32 = px.chunks(4).map(|p| p[0] as u32).sum();
        assert!(after > before * 2, "{before} -> {after}");
    }

    #[test]
    fn enhance_leaves_a_well_lit_full_range_picture_nearly_alone() {
        let mut px: Vec<u8> = (0..256).flat_map(|i| { let v = i as u8; [v, v, v, 255] }).collect();
        let orig = px.clone();
        enhance(&mut px);
        let drift: i32 = px.chunks(4).zip(orig.chunks(4)).map(|(a, b)| (a[0] as i32 - b[0] as i32).abs()).max().unwrap();
        assert!(drift <= 12, "a normal picture changed by up to {drift}");
    }
}

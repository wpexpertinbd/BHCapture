//! Screen-capture cross-check, reusing 0b's shim. Callbacks firing proves a callback
//! fired; only pixels prove the overlay is actually on screen and on top.

use std::os::raw::c_char;

#[repr(C)]
#[derive(Default)]
struct BhcImage { data: *mut u8, width: i32, height: i32, stride: i32 }

extern "C" {
    fn bhc_capture_display(display_id: u32, out: *mut BhcImage, err: *mut c_char, errcap: i32) -> i32;
    fn bhc_image_free(img: *mut BhcImage);
}

/// Fraction of sampled pixels that look like the overlay's magenta tint.
pub fn magenta_fraction() -> Result<f64, String> {
    let mut img = BhcImage::default();
    let mut err = [0 as c_char; 512];
    let rc = unsafe { bhc_capture_display(1, &mut img, err.as_mut_ptr(), 512) };
    if rc != 0 {
        let b: Vec<u8> = err.iter().take_while(|&&c| c != 0).map(|&c| c as u8).collect();
        return Err(String::from_utf8_lossy(&b).into_owned());
    }
    let (w, h, stride) = (img.width as usize, img.height as usize, img.stride as usize);
    let d = unsafe { std::slice::from_raw_parts(img.data, stride * h) };
    let (mut hit, mut n) = (0u64, 0u64);
    for y in (0..h).step_by((h / 200).max(1)) {
        for x in (0..w).step_by((w / 200).max(1)) {
            let o = y * stride + x * 4;
            let (b, g, r) = (d[o] as i32, d[o + 1] as i32, d[o + 2] as i32);
            // magenta tint: red and blue both well above green
            if r > g + 40 && b > g + 25 { hit += 1; }
            n += 1;
        }
    }
    unsafe { bhc_image_free(&mut img) };
    Ok(hit as f64 / n as f64)
}

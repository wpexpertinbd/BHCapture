//! BHCapture Phase-0b spike: prove a Rust -> ScreenCaptureKit path returns REAL pixels.
//!
//! Exit criterion (docs/PLAN.md): a correct PNG of the real screen, from Rust, under ~150 ms.
//!
//! "Correct" is doing real work here. A missing Screen Recording grant does not
//! usually error -- it hands back a uniformly BLACK frame. So this spike refuses to
//! report success on a frame it cannot prove is real content, and a human still has
//! to open the PNG and look at it.

use std::os::raw::c_char;
use std::time::Instant;

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct BhcDisplayInfo {
    display_id: u32,
    x: i32,
    y: i32,
    pt_width: i32,
    pt_height: i32,
    px_width: i32,
    px_height: i32,
}

#[repr(C)]
#[derive(Debug, Default)]
struct BhcImage {
    data: *mut u8,
    width: i32,
    height: i32,
    stride: i32,
}

extern "C" {
    fn bhc_preflight_permission() -> i32;
    fn bhc_list_displays(out: *mut BhcDisplayInfo, cap: i32, err: *mut c_char, errcap: i32) -> i32;
    fn bhc_capture_display(display_id: u32, out: *mut BhcImage, err: *mut c_char, errcap: i32) -> i32;
    fn bhc_image_free(img: *mut BhcImage);
    fn bhc_peak_rss_bytes() -> u64;
}

const ERRCAP: i32 = 512;

fn take_err(buf: &[c_char]) -> String {
    let bytes: Vec<u8> = buf.iter().take_while(|&&c| c != 0).map(|&c| c as u8).collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// What we can prove about a frame without a human eye on it.
struct FrameStats {
    uniform: bool,
    mean_luma: f64,
    distinct_sampled: usize,
    opaque_everywhere: bool,
}

fn analyse(data: &[u8], w: usize, h: usize, stride: usize) -> FrameStats {
    let first = [data[0], data[1], data[2], data[3]];
    let mut uniform = true;
    let mut luma_sum = 0f64;
    let mut n = 0u64;
    let mut seen = std::collections::HashSet::new();
    let mut opaque = true;

    // Sample a grid rather than every pixel: enough to characterise, cheap on 6K.
    let step_y = (h / 240).max(1);
    let step_x = (w / 240).max(1);
    for y in (0..h).step_by(step_y) {
        for x in (0..w).step_by(step_x) {
            let o = y * stride + x * 4;
            let (b, g, r, a) = (data[o], data[o + 1], data[o + 2], data[o + 3]);
            if [b, g, r, a] != first {
                uniform = false;
            }
            if a != 255 {
                opaque = false;
            }
            luma_sum += 0.2126 * r as f64 + 0.7152 * g as f64 + 0.0722 * b as f64;
            n += 1;
            if seen.len() < 100_000 {
                seen.insert((r, g, b));
            }
        }
    }
    FrameStats {
        uniform,
        mean_luma: luma_sum / n as f64,
        distinct_sampled: seen.len(),
        opaque_everywhere: opaque,
    }
}

fn bgra_to_rgba(src: &[u8], w: usize, h: usize, stride: usize) -> Vec<u8> {
    let mut out = vec![0u8; w * h * 4];
    for y in 0..h {
        let s = y * stride;
        let d = y * w * 4;
        for x in 0..w {
            let so = s + x * 4;
            let dof = d + x * 4;
            out[dof] = src[so + 2]; // R <- R (byte 2 in BGRA memory order)
            out[dof + 1] = src[so + 1]; // G
            out[dof + 2] = src[so]; // B
            out[dof + 3] = src[so + 3]; // A
        }
    }
    out
}

fn main() {
    let runs: usize = std::env::args()
        .skip_while(|a| a != "--runs")
        .nth(1)
        .and_then(|v| v.parse().ok())
        .unwrap_or(5);

    println!("== BHCapture Phase-0b spike ==\n");

    // ---- permission ------------------------------------------------------
    let granted = unsafe { bhc_preflight_permission() } == 1;
    println!(
        "Screen Recording (CGPreflightScreenCaptureAccess): {}",
        if granted { "GRANTED" } else { "NOT GRANTED" }
    );
    if !granted {
        println!("  note: a CLI run from a terminal is attributed to the PARENT app's TCC identity,");
        println!("        not to this binary. 0a is what proves the .app-bundle path.");
    }
    println!();

    // ---- displays --------------------------------------------------------
    let mut infos = [BhcDisplayInfo::default(); 16];
    let mut err = [0 as c_char; ERRCAP as usize];
    let n = unsafe { bhc_list_displays(infos.as_mut_ptr(), 16, err.as_mut_ptr(), ERRCAP) };
    if n < 0 {
        eprintln!("FAIL: bhc_list_displays: {}", take_err(&err));
        std::process::exit(1);
    }
    println!("Displays found: {n}");
    for d in infos.iter().take(n as usize) {
        let scale = if d.pt_width > 0 {
            d.px_width as f64 / d.pt_width as f64
        } else {
            0.0
        };
        println!(
            "  id={:<10} origin=({:>5},{:>5})  points={}x{}  pixels={}x{}  scale={scale:.2}x",
            d.display_id, d.x, d.y, d.pt_width, d.pt_height, d.px_width, d.px_height
        );
    }
    if n == 0 {
        eprintln!("FAIL: no displays returned");
        std::process::exit(1);
    }
    println!();

    // ---- capture, timed --------------------------------------------------
    let target = infos[0];
    println!("Capturing display {} x{runs}...", target.display_id);
    let mut timings = Vec::new();
    let mut last: Option<(Vec<u8>, usize, usize, usize)> = None;

    for i in 0..runs {
        let mut img = BhcImage::default();
        let mut e = [0 as c_char; ERRCAP as usize];
        let t0 = Instant::now();
        let rc = unsafe { bhc_capture_display(target.display_id, &mut img, e.as_mut_ptr(), ERRCAP) };
        let dt = t0.elapsed();
        if rc != 0 {
            eprintln!("FAIL: bhc_capture_display (run {}): {}", i + 1, take_err(&e));
            std::process::exit(1);
        }
        let (w, h, stride) = (img.width as usize, img.height as usize, img.stride as usize);
        let slice = unsafe { std::slice::from_raw_parts(img.data, stride * h) };
        timings.push(dt);
        println!(
            "  run {:>2}: {:>7.1} ms   {w}x{h}  stride={stride}",
            i + 1,
            dt.as_secs_f64() * 1000.0
        );
        if i + 1 == runs {
            last = Some((slice.to_vec(), w, h, stride));
        }
        unsafe { bhc_image_free(&mut img) };
    }

    let (buf, w, h, stride) = last.unwrap();
    let cold = timings[0].as_secs_f64() * 1000.0;
    let warm: Vec<f64> = timings[1..].iter().map(|d| d.as_secs_f64() * 1000.0).collect();
    let warm_avg = if warm.is_empty() {
        cold
    } else {
        warm.iter().sum::<f64>() / warm.len() as f64
    };
    let warm_best = warm.iter().cloned().fold(f64::INFINITY, f64::min);

    println!(
        "\nTiming: cold {cold:.1} ms | warm avg {warm_avg:.1} ms | warm best {:.1} ms",
        if warm_best.is_finite() { warm_best } else { cold }
    );

    // ---- is it real content? --------------------------------------------
    let st = analyse(&buf, w, h, stride);
    println!("\nFrame analysis:");
    println!("  uniform (every sampled pixel identical): {}", st.uniform);
    println!("  mean luma (0-255):                       {:.2}", st.mean_luma);
    println!("  distinct RGB values sampled:             {}", st.distinct_sampled);
    println!("  fully opaque:                            {}", st.opaque_everywhere);

    let dims_ok = w as i32 == target.px_width && h as i32 == target.px_height;
    println!(
        "  matches display physical size {}x{}:  {dims_ok}",
        target.px_width, target.px_height
    );

    // ---- write the PNG ---------------------------------------------------
    let out_path = std::env::temp_dir().join("bhcapture-0b-spike.png");
    let rgba = bgra_to_rgba(&buf, w, h, stride);
    {
        let file = std::fs::File::create(&out_path).expect("create png");
        let bw = std::io::BufWriter::new(file);
        let mut enc = png::Encoder::new(bw, w as u32, h as u32);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut writer = enc.write_header().expect("png header");
        writer.write_image_data(&rgba).expect("png data");
    }
    let size = std::fs::metadata(&out_path).map(|m| m.len()).unwrap_or(0);
    println!("\nPNG written: {} ({} bytes)", out_path.display(), size);
    println!("Peak RSS: {:.1} MB", unsafe { bhc_peak_rss_bytes() } as f64 / 1_048_576.0);

    // ---- verdict ---------------------------------------------------------
    println!("\n== verdict ==");
    let mut fails = Vec::new();
    if st.uniform {
        fails.push("frame is a uniform colour -- almost certainly a denied-permission black frame");
    }
    if st.distinct_sampled < 50 {
        fails.push("fewer than 50 distinct colours -- not a real desktop");
    }
    if !dims_ok {
        fails.push("captured size != display physical size");
    }
    if warm_avg > 150.0 {
        fails.push("warm average exceeds the 150 ms budget");
    }
    if fails.is_empty() {
        println!("PASS (machine-checkable parts). A HUMAN MUST STILL OPEN THE PNG AND LOOK AT IT.");
    } else {
        for f in &fails {
            println!("FAIL: {f}");
        }
        std::process::exit(1);
    }
}

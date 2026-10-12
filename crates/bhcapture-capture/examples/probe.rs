//! Manual verification of the macOS backend against a real screen.
//!
//! Not a unit test: it needs a display, a window server and a granted permission. Writes PNGs
//! so a human can look at them, because "the call returned 0" has never been proof of a correct
//! image.

use std::time::Instant;
use bhcapture_capture::{macos::MacCapturer, CaptureError, Capturer};
use bhcapture_core::geom::LogRect;
use bhcapture_core::image::Frame;

fn save(f: &Frame, name: &str) -> String {
    let path = std::env::temp_dir().join(name);
    let file = std::fs::File::create(&path).unwrap();
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), f.width(), f.height());
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().unwrap().write_image_data(&f.to_rgba8()).unwrap();
    path.display().to_string()
}

/// A frame that is a single flat colour is the classic denied-permission result, not a capture.
fn describe(f: &Frame) -> String {
    let (w, h) = (f.width(), f.height());
    let first = f.pixel(0, 0).unwrap();
    let mut uniform = true;
    let mut seen = std::collections::HashSet::new();
    for y in (0..h).step_by((h as usize / 200).max(1)) {
        for x in (0..w).step_by((w as usize / 200).max(1)) {
            let p = f.pixel(x, y).unwrap();
            if p != first { uniform = false; }
            seen.insert((p[0], p[1], p[2]));
        }
    }
    format!("{w}x{h} @{:.2}x  uniform={uniform}  distinct={}  -> {}",
        f.scale().get(), seen.len(),
        if !uniform && seen.len() >= 50 { "REAL PIXELS" } else { "SUSPECT (flat frame)" })
}

fn main() {
    println!("== BHCapture macOS backend probe ==\n");
    println!("Screen Recording granted: {}", MacCapturer::has_permission());

    let cap = MacCapturer::new();
    let t = Instant::now();
    let targets = match cap.list_targets() {
        Ok(t) => t,
        Err(CaptureError::PermissionDenied) => {
            eprintln!("\nPermissionDenied. Grant Screen Recording, then RELAUNCH -- \
                       a running process never sees a new grant.");
            std::process::exit(1);
        }
        Err(e) => { eprintln!("\nlist_targets failed: {e}"); std::process::exit(1); }
    };
    println!("list_targets: {:.1} ms", t.elapsed().as_secs_f64() * 1000.0);

    println!("\nDisplays ({}):", targets.layout.displays().len());
    for d in targets.layout.displays() {
        println!("  id={:<9} origin=({:>5},{:>5}) {}x{}pt  {}x{}px  scale={:.2}x",
            d.id.0, d.bounds.min_x(), d.bounds.min_y(),
            d.bounds.size.w, d.bounds.size.h, d.physical.w, d.physical.h, d.scale.get());
    }
    println!("Virtual desktop: {:?}", targets.layout.virtual_bounds());

    let onscreen: Vec<_> = targets.windows.iter()
        .filter(|w| w.on_screen && !w.title.is_empty() && w.bounds.size.w > 200.0).collect();
    println!("\nWindows: {} total, {} on-screen with a title", targets.windows.len(), onscreen.len());
    for w in onscreen.iter().take(6) {
        println!("  [{}] {:<22} {:.0}x{:.0}  \"{}\"", w.id.0, w.app,
            w.bounds.size.w, w.bounds.size.h,
            w.title.chars().take(40).collect::<String>());
    }

    // --- display ---
    let d = targets.layout.displays()[0];
    let t = Instant::now();
    let frame = cap.capture_display(d.id).expect("capture_display");
    println!("\ncapture_display: {:.1} ms", t.elapsed().as_secs_f64() * 1000.0);
    println!("  {}", describe(&frame));
    println!("  {}", save(&frame, "bhcapture-probe-display.png"));

    // --- region (a quarter of the display, offset so it is clearly a crop) ---
    let b = d.bounds;
    let region = LogRect::new(b.min_x() + b.size.w * 0.25, b.min_y() + b.size.h * 0.25,
                              b.size.w * 0.5, b.size.h * 0.5);
    let t = Instant::now();
    let rf = cap.capture_region(region).expect("capture_region");
    println!("\ncapture_region {:?}: {:.1} ms", region, t.elapsed().as_secs_f64() * 1000.0);
    println!("  {}", describe(&rf));
    println!("  {}", save(&rf, "bhcapture-probe-region.png"));

    // --- window ---
    if let Some(w) = onscreen.first() {
        let t = Instant::now();
        match cap.capture_window(w.id) {
            Ok(wf) => {
                println!("\ncapture_window [{}] \"{}\": {:.1} ms",
                    w.id.0, w.app, t.elapsed().as_secs_f64() * 1000.0);
                println!("  {}", describe(&wf));
                println!("  {}", save(&wf, "bhcapture-probe-window.png"));
            }
            Err(e) => println!("\ncapture_window failed: {e}"),
        }
    }

    // --- error paths ---
    println!("\nError paths:");
    use bhcapture_capture::WindowId;
    use bhcapture_core::geom::DisplayId;
    println!("  bogus display -> {:?}", cap.capture_display(DisplayId(999_999)).err());
    println!("  bogus window  -> {:?}", cap.capture_window(WindowId(999_999_999)).err());
    println!("  offscreen region -> {:?}", cap.capture_region(LogRect::new(99_000.0, 99_000.0, 10.0, 10.0)).err());
    println!("  empty region  -> {:?}", cap.capture_region(LogRect::new(0.0, 0.0, 0.0, 0.0)).err());
}

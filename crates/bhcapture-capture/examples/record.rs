//! Record a region for N seconds, with an optional pause in the middle.
//! `cargo run --example record -- <out.mp4> <seconds> [x y w h] [--sys] [--mic] [--pause]`
use std::time::Duration;

use bhcapture_capture::macos::{MacCapturer, MacRecorder};
use bhcapture_capture::record::RecordOptions;
use bhcapture_capture::Capturer;
use bhcapture_core::geom::LogRect;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let out = std::path::PathBuf::from(&a[1]);
    let secs: f64 = a[2].parse().unwrap();
    let nums: Vec<f64> = a[3..].iter().filter_map(|s| s.parse().ok()).collect();
    let flag = |f: &str| a.iter().any(|x| x == f);
    let layout = MacCapturer::new().list_displays().expect("displays");
    let d = layout.displays().first().expect("a display").clone();
    let region = if nums.len() == 4 { LogRect::new(nums[0], nums[1], nums[2], nums[3]) }
                 else { LogRect::new(0.0, 0.0, d.bounds.size.w, d.bounds.size.h) };
    let rec = MacRecorder::start(&RecordOptions {
        display: d.id, region, fps: 30, show_cursor: true,
        system_audio: flag("--sys"), microphone: flag("--mic"), path: out.clone(),
    }).expect("start");
    let step = Duration::from_millis(250);
    let mut t = 0.0;
    while t < secs {
        std::thread::sleep(step); t += 0.25;
        if flag("--pause") && (t - secs / 3.0).abs() < 0.01 { rec.set_paused(true); println!("paused"); }
        if flag("--pause") && (t - 2.0 * secs / 3.0).abs() < 0.01 { rec.set_paused(false); println!("resumed"); }
        let s = rec.status();
        if (t * 4.0) as i64 % 4 == 0 {
            println!("t={t:.1} state={:?} rec={:.2}s frames={} {}x{} sys={:.2} mic={:.2} {:?}",
                     s.state, s.seconds, s.frames, s.width, s.height, s.level_system, s.level_mic, s.error);
        }
    }
    match rec.stop() { Ok(()) => println!("STOPPED OK -> {}", out.display()), Err(e) => println!("STOP ERROR: {e}") }
}

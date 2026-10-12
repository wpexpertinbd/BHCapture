//! Does repeated capture grow memory? Regression harness for a real leak: drawing a captured CGImage
//! into a bitmap leaked ~20 MB per capture on macOS 27. Run: `leak capture` / `leak snapshot`.
//! Captures a display in a loop, drops each frame, and prints
//! the process's resident size. A flat line means frames are freed; ~19 MB per step means a leak.
use bhcapture_capture::{macos::MacCapturer, Capturer, Snapshot};

fn rss_mb() -> f64 {
    let out = std::process::Command::new("ps").args(["-o", "rss=", "-p", &std::process::id().to_string()]).output().unwrap();
    String::from_utf8_lossy(&out.stdout).trim().parse::<f64>().unwrap_or(0.0) / 1024.0
}

fn main() {
    let c = MacCapturer::new();
    let layout = c.list_displays().expect("displays");
    let id = layout.displays()[0].id;
    let mode = std::env::args().nth(1).unwrap_or_else(|| "capture".into());
    println!("mode={mode}  start: {:.0} MB", rss_mb());
    for i in 1..=12 {
        match mode.as_str() {
            // the raw backend call: shim buffer -> Rust Frame -> dropped
            "capture" => { let f = c.capture_display(id).expect("capture"); drop(f); }
            // what a selector session does: a Snapshot, then dropped
            "snapshot" => { let s = Snapshot::take(&c, &layout).expect("snapshot"); drop(s); }
            // just the window/display listing, no pixels
            "list" => { let _ = c.list_targets().expect("list"); }
            _ => panic!("unknown mode"),
        }
        println!("  iteration {i:>2}: {:.0} MB", rss_mb());
    }
    if std::env::var("LEAK_HOLD").is_ok() {
        println!("holding for inspection (pid {})", std::process::id());
        std::thread::sleep(std::time::Duration::from_secs(40));
    }
}

//! `cargo run --example export -- <in.mp4> <out> mov|hevc`
use bhcapture_capture::macos::{export_video, VideoExport};
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let k = if a[3] == "hevc" { VideoExport::Hevc } else { VideoExport::Mov };
    let t0 = std::time::Instant::now();
    let mut last = -1.0;
    let r = export_video(a[1].as_ref(), a[2].as_ref(), k, |p| { if p - last >= 0.25 { println!("{:.0}%", p * 100.0); last = p; } true });
    println!("{r:?} in {:.2}s", t0.elapsed().as_secs_f64());
}

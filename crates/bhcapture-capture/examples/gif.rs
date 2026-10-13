//! `cargo run --example gif -- <in.mp4> <out.gif>` : export with the app's defaults, and report.
use bhcapture_core::gif_export::{GifWriter, DEFAULT_FPS, DEFAULT_MAX_SIDE};
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let t0 = std::time::Instant::now();
    let mut w: Option<GifWriter<std::io::BufWriter<std::fs::File>>> = None;
    let mut end = 0.0;
    bhcapture_capture::macos::video_frames(a[1].as_ref(), DEFAULT_FPS, DEFAULT_MAX_SIDE, |px, wd, ht, t, dur| {
        if w.is_none() {
            w = Some(GifWriter::new(std::io::BufWriter::new(std::fs::File::create(&a[2]).unwrap()), wd, ht).unwrap());
        }
        end = dur;
        w.as_mut().unwrap().push(px, t).is_ok()
    }).expect("decode");
    let (i, o) = w.unwrap().finish(end).unwrap();
    println!("frames in {i}, written {o}, {:.1}s, took {:.2}s", end, t0.elapsed().as_secs_f64());
}

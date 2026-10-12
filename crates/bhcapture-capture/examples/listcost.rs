//! A/B the two listing calls, so the claim about window-enumeration cost is measured, not assumed.
use std::time::Instant;
use bhcapture_capture::{macos::MacCapturer, Capturer};

fn main() {
    let c = MacCapturer::new();
    let _ = c.list_targets(); // warm up: the first SCShareableContent call pays start-up costs
    let n = 12;
    let mut d = Vec::new();
    let mut t = Vec::new();
    for _ in 0..n {
        let s = Instant::now(); let _ = c.list_displays(); d.push(s.elapsed().as_secs_f64() * 1000.0);
        let s = Instant::now(); let r = c.list_targets(); t.push(s.elapsed().as_secs_f64() * 1000.0);
        if let Ok(r) = r { std::hint::black_box(r.windows.len()); }
    }
    let med = |v: &mut Vec<f64>| { v.sort_by(|a, b| a.partial_cmp(b).unwrap()); v[v.len() / 2] };
    let (dm, tm) = (med(&mut d), med(&mut t));
    println!("list_displays  median {dm:>6.1} ms   (n={n})");
    println!("list_targets   median {tm:>6.1} ms   (n={n})");
    println!("window enumeration costs {:.1} ms on top", tm - dm);
}

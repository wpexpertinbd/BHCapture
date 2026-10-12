//! Prints on-screen windows in the order the backend reports. Compare against what is actually
//! frontmost on the desktop before trusting snap-to-window.
use bhcapture_capture::{macos::MacCapturer, Capturer};

fn main() {
    let t = MacCapturer::new().list_targets().expect("list_targets");
    let mut w: Vec<_> = t.windows.iter().filter(|w| w.z_order.is_some()).collect();
    w.sort_by_key(|w| w.z_order);
    println!("{:>3} {:>5} {:>6}  {:<20} {:>9}  title", "z", "layer", "id", "app", "size");
    for w in w.iter().take(14) {
        println!("{:>3} {:>5} {:>6}  {:<20} {:>4.0}x{:<4.0}  {}", w.z_order.unwrap(), w.layer, w.id.0,
            w.app.chars().take(20).collect::<String>(), w.bounds.size.w, w.bounds.size.h,
            w.title.chars().take(36).collect::<String>());
    }
    let listed = t.windows.iter().filter(|w| w.z_order.is_some()).count();
    println!("\n{} windows total, {} listed front-to-back, {} not listed (minimised / other Spaces)",
        t.windows.len(), listed, t.windows.len() - listed);
}

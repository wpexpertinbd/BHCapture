//! `cargo run --example mix -- <in.mp4> <out.mp4>`
fn main() {
    let a: Vec<String> = std::env::args().collect();
    match bhcapture_capture::macos::mix_audio_tracks(a[1].as_ref(), a[2].as_ref()) {
        Ok(()) => println!("MIXED OK"),
        Err(e) => println!("MIX ERROR: {e}"),
    }
}

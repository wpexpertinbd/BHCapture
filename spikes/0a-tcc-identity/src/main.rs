//! BHCapture Phase-0a spike: does the Screen Recording grant survive a rebuild?
//!
//! Exit criterion (docs/PLAN.md): two consecutive rebuilds keep the grant, with no new prompt.
//!
//! Runs as a real .app bundle launched through LaunchServices (`open`), because a binary
//! spawned straight from a shell is attributed to the PARENT process's TCC identity --
//! which is exactly what made 0b's "GRANTED" meaningless as evidence for this question.
//!
//! Appends one record per launch to a log the harness reads back.

use std::io::Write;
use std::os::raw::c_char;

#[repr(C)]
#[derive(Default)]
struct BhcImage {
    data: *mut u8,
    width: i32,
    height: i32,
    stride: i32,
}

extern "C" {
    fn bhc_preflight_permission() -> i32;
    fn bhc_request_permission() -> i32;
    fn bhc_capture_display(display_id: u32, out: *mut BhcImage, err: *mut c_char, errcap: i32) -> i32;
    fn bhc_image_free(img: *mut BhcImage);
}

const ERRCAP: i32 = 512;

fn take_err(buf: &[c_char]) -> String {
    let b: Vec<u8> = buf.iter().take_while(|&&c| c != 0).map(|&c| c as u8).collect();
    String::from_utf8_lossy(&b).into_owned()
}

fn main() {
    let log_path = std::env::args()
        .skip_while(|a| a != "--log")
        .nth(1)
        .unwrap_or_else(|| "/tmp/bhcapture-0a.log".into());

    let marker = env!("BHC_MARKER");
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_default();

    let mut rec = String::new();
    rec.push_str(&format!("---- launch marker={marker}\n"));
    rec.push_str(&format!("     exe={exe}\n"));

    // Preflight NEVER prompts -- this is the state the grant is actually in.
    let pre = unsafe { bhc_preflight_permission() } == 1;
    rec.push_str(&format!("     preflight={}\n", if pre { "GRANTED" } else { "DENIED" }));

    // Only ask when we have to. A request on an already-granted app is a no-op, but on a
    // denied one it raises the system prompt -- which is the thing that must NOT reappear
    // after a rebuild.
    if !pre {
        let req = unsafe { bhc_request_permission() } == 1;
        rec.push_str(&format!("     requested -> {} (prompt raised if undecided)\n", req));

        // The probe otherwise exits in milliseconds and takes the system dialog with it.
        // Hold the process alive and poll so the prompt stays on screen and the moment the
        // grant lands is actually observable.
        let hold: u64 = std::env::args()
            .skip_while(|a| a != "--hold")
            .nth(1)
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        if hold > 0 {
            rec.push_str(&format!("     holding up to {hold}s for a decision...\n"));
            let mut granted_after = None;
            for i in 1..=hold {
                std::thread::sleep(std::time::Duration::from_secs(1));
                if unsafe { bhc_preflight_permission() } == 1 {
                    granted_after = Some(i);
                    break;
                }
            }
            match granted_after {
                Some(i) => rec.push_str(&format!("     GRANT LANDED after {i}s\n")),
                None => rec.push_str("     no grant within the hold window\n"),
            }
        }
    }

    // Preflight returning true is weaker evidence than it looks: a stale/mismatched grant
    // can preflight true and still hand back a black frame. So actually capture.
    let mut img = BhcImage::default();
    let mut err = [0 as c_char; ERRCAP as usize];
    let rc = unsafe { bhc_capture_display(1, &mut img, err.as_mut_ptr(), ERRCAP) };
    if rc != 0 {
        rec.push_str(&format!("     capture=ERROR {}\n", take_err(&err)));
    } else {
        let (w, h, stride) = (img.width as usize, img.height as usize, img.stride as usize);
        let data = unsafe { std::slice::from_raw_parts(img.data, stride * h) };
        let first = [data[0], data[1], data[2], data[3]];
        let mut uniform = true;
        let mut seen = std::collections::HashSet::new();
        for y in (0..h).step_by((h / 200).max(1)) {
            for x in (0..w).step_by((w / 200).max(1)) {
                let o = y * stride + x * 4;
                if data[o..o + 4] != first {
                    uniform = false;
                }
                seen.insert((data[o], data[o + 1], data[o + 2]));
            }
        }
        rec.push_str(&format!(
            "     capture=OK {w}x{h} uniform={uniform} distinct={} -> {}\n",
            seen.len(),
            if !uniform && seen.len() >= 50 { "REAL PIXELS" } else { "BLACK/FAKE FRAME" }
        ));
        unsafe { bhc_image_free(&mut img) };
    }

    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&log_path) {
        let _ = f.write_all(rec.as_bytes());
    }
}

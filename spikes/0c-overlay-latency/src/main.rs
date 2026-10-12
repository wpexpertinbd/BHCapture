//! BHCapture Phase-0c spike: can a PRE-CREATED Tauri webview overlay appear fast enough?
//!
//! Exit criterion (docs/PLAN.md): hotkey -> visible overlay under 100 ms. If a webview
//! overlay cannot hit that, the region selector must be native AppKit/Win32 and the
//! webview keeps only the editor -- an architecture fork, which is why this runs now.
//!
//! Measures three separate things, because "show() returned" and "pixels on screen" are
//! very different numbers and only the second one is the product question:
//!   show    - hotkey -> show() returned                     (native cost only)
//!   painted - hotkey -> webview's post-paint callback        (end-to-end, includes IPC)
//!   ipc     - emit -> webview -> invoke round trip, no window work (measurement overhead)

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use global_hotkey::{hotkey::{Code, HotKey, Modifiers}, GlobalHotKeyEvent, GlobalHotKeyManager};
use tauri::{Emitter, Manager, WebviewUrl, WebviewWindowBuilder};

mod capture;

#[derive(Default)]
struct Shared {
    starts: Mutex<HashMap<u64, Instant>>,
    results: Mutex<Vec<(String, f64)>>,
}

#[tauri::command]
fn report(seq: u64, phase: String, state: tauri::State<Arc<Shared>>) {
    let t0 = state.starts.lock().unwrap().get(&seq).copied();
    if let Some(t0) = t0 {
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        println!("    seq {seq:>3}  {phase:<8} {ms:>7.1} ms");
        state.results.lock().unwrap().push((phase, ms));
    }
}

fn fire(app: &tauri::AppHandle, shared: &Arc<Shared>, seq: u64) {
    let t0 = Instant::now();
    shared.starts.lock().unwrap().insert(seq, t0);
    for w in app.webview_windows().values() {
        let _ = w.show();
    }
    let ms = t0.elapsed().as_secs_f64() * 1000.0;
    println!("    seq {seq:>3}  {:<8} {ms:>7.1} ms", "show");
    shared.results.lock().unwrap().push(("show".into(), ms));
    let _ = app.emit("show-now", seq);
}

fn hide_all(app: &tauri::AppHandle) {
    for w in app.webview_windows().values() {
        let _ = w.hide();
    }
}

fn main() {
    let shared = Arc::new(Shared::default());
    let seqno = Arc::new(AtomicU64::new(0));

    tauri::Builder::default()
        .manage(shared.clone())
        .invoke_handler(tauri::generate_handler![report])
        .setup(move |app| {
            let handle = app.handle().clone();

            // Pre-create one hidden overlay per display AT LAUNCH. Creating a webview window
            // on hotkey-press is visibly slow -- that is the whole point of Blocker 4.
            let monitors = app.available_monitors()?;
            println!("Pre-creating {} overlay window(s) at launch...", monitors.len());
            for (i, m) in monitors.iter().enumerate() {
                let sf = m.scale_factor();
                let (p, s) = (m.position(), m.size());
                WebviewWindowBuilder::new(app, format!("overlay{i}"), WebviewUrl::App("index.html".into()))
                    .position(p.x as f64 / sf, p.y as f64 / sf)
                    .inner_size(s.width as f64 / sf, s.height as f64 / sf)
                    .decorations(false)
                    .transparent(true)
                    .always_on_top(true)
                    .visible(false)
                    .resizable(false)
                    .shadow(false)
                    .title("BHCapture overlay probe")
                    .build()?;
                println!("  overlay{i}: {}x{} @{:.1}x at ({},{})", s.width, s.height, sf, p.x, p.y);
            }

            // Real global hotkey. global-hotkey uses Carbon RegisterEventHotKey on macOS,
            // which needs no Accessibility permission.
            let manager = GlobalHotKeyManager::new()?;
            let hk = HotKey::new(Some(Modifiers::SUPER | Modifiers::SHIFT | Modifiers::CONTROL), Code::Digit9);
            manager.register(hk)?;
            println!("\nGlobal hotkey registered: Cmd+Shift+Ctrl+9");
            std::mem::forget(manager); // keep it alive for the process lifetime

            {
                let (h, sh, sq) = (handle.clone(), shared.clone(), seqno.clone());
                std::thread::spawn(move || {
                    let rx = GlobalHotKeyEvent::receiver();
                    while let Ok(ev) = rx.recv() {
                        if ev.state == global_hotkey::HotKeyState::Pressed {
                            let n = sq.fetch_add(1, Ordering::SeqCst);
                            println!("  [hotkey]");
                            fire(&h, &sh, n);
                            std::thread::sleep(Duration::from_millis(700));
                            hide_all(&h);
                        }
                    }
                });
            }

            // Self-trigger run: same code path as the hotkey, so samples can be collected
            // without a human at the keyboard.
            {
                let (h, sh, sq) = (handle.clone(), shared.clone(), seqno.clone());
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(2500)); // let the webviews settle

                    println!("\n-- IPC baseline (no window work) --");
                    for _ in 0..5 {
                        let n = sq.fetch_add(1, Ordering::SeqCst);
                        sh.starts.lock().unwrap().insert(n, Instant::now());
                        let _ = h.emit("ping", n);
                        std::thread::sleep(Duration::from_millis(300));
                    }

                    println!("\n-- overlay show/paint, 10 runs --");
                    for i in 0..10 {
                        let n = sq.fetch_add(1, Ordering::SeqCst);
                        fire(&h, &sh, n);
                        std::thread::sleep(Duration::from_millis(400));
                        // Cross-check ONCE that the overlay is genuinely on screen, by
                        // capturing it. Callbacks firing is not proof of pixels.
                        if i == 0 {
                            match capture::magenta_fraction() {
                                Ok(f) => println!("    [capture cross-check] overlay tint covers {:.1}% of the screen", f * 100.0),
                                Err(e) => println!("    [capture cross-check] FAILED: {e}"),
                            }
                        }
                        hide_all(&h);
                        std::thread::sleep(Duration::from_millis(500));
                    }

                    // Summary
                    let r = sh.results.lock().unwrap();
                    println!("\n==================== 0c SUMMARY ====================");
                    for phase in ["ipc", "show", "painted"] {
                        let v: Vec<f64> = r.iter().filter(|(p, _)| p == phase).map(|(_, m)| *m).collect();
                        if v.is_empty() { continue; }
                        let mut s = v.clone();
                        s.sort_by(|a, b| a.partial_cmp(b).unwrap());
                        let avg = v.iter().sum::<f64>() / v.len() as f64;
                        println!("  {phase:<8} n={:<3} min {:>6.1}  median {:>6.1}  avg {:>6.1}  max {:>6.1} ms",
                            v.len(), s[0], s[s.len() / 2], avg, s[s.len() - 1]);
                    }
                    println!("====================================================");
                    println!("Press Cmd+Shift+Ctrl+9 to test the real hotkey path, or Ctrl+C to quit.");
                });
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("tauri run");
}

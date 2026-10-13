import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";

const IS_MAC = navigator.userAgent.includes("Mac");

/** The chord a key event spells, in the words the Rust parser takes ("Ctrl+Shift+4"). Null for a
 *  modifier pressed on its own (the user is still building the chord). */
function chordOf(e: KeyboardEvent): string | null {
  let key: string | null = null;
  const c = e.code;
  if (/^Key[A-Z]$/.test(c)) key = c.slice(3);
  else if (/^Digit[0-9]$/.test(c)) key = c.slice(5);
  else if (/^F([1-9]|1[0-9])$/.test(c)) key = c;
  else if (c === "PrintScreen") key = "PrintScreen";
  if (!key) return null;
  const parts: string[] = [];
  if (e.ctrlKey) parts.push("Ctrl");
  if (e.altKey) parts.push(IS_MAC ? "Option" : "Alt");
  if (e.shiftKey) parts.push("Shift");
  if (e.metaKey) parts.push(IS_MAC ? "Cmd" : "Win");
  parts.push(key);
  return parts.join("+");
}

function heldModifiers(e: KeyboardEvent): string {
  const p: string[] = [];
  if (e.ctrlKey) p.push(IS_MAC ? "⌃" : "Ctrl+");
  if (e.altKey) p.push(IS_MAC ? "⌥" : "Alt+");
  if (e.shiftKey) p.push(IS_MAC ? "⇧" : "Shift+");
  if (e.metaKey) p.push(IS_MAC ? "⌘" : "Win+");
  return p.join("");
}

/**
 * Snagit-style shortcut box: click it and PRESS the keys. While it listens, BHCapture's own global
 * shortcuts are switched off -- otherwise pressing the current capture shortcut to re-assign it would
 * open the selector instead. The chord is checked by the same Rust rules that will register it, so
 * a shortcut the OS owns is refused here, with the reason, before it is ever saved.
 */
export function ShortcutField(p: { id: string; value: string; onChange: (v: string) => void }) {
  const [listening, setListening] = useState(false);
  const [held, setHeld] = useState("");
  const [error, setError] = useState<string | null>(null);
  const boxRef = useRef<HTMLButtonElement>(null);

  const stop = () => {
    setListening(false);
    setHeld("");
    void invoke("hotkeys_suspend", { suspended: false });
  };

  useEffect(() => {
    if (!listening) return;
    const accept = (e: KeyboardEvent) => {
      e.preventDefault(); e.stopPropagation();
      if (e.key === "Escape" && !e.ctrlKey && !e.metaKey && !e.altKey && !e.shiftKey) { stop(); return; }
      const chord = chordOf(e);
      if (!chord) { setHeld(heldModifiers(e)); return; }
      invoke<string>("hotkey_check", { spec: chord })
        .then((canonical) => { p.onChange(canonical); setError(null); stop(); })
        .catch((err) => { setError(String(err)); setHeld(""); });
    };
    const down = (e: KeyboardEvent) => { if (e.code !== "PrintScreen") accept(e); };
    // Windows delivers Print Screen to web pages only as a key-UP.
    const up = (e: KeyboardEvent) => { if (e.code === "PrintScreen") accept(e); else setHeld(heldModifiers(e)); };
    window.addEventListener("keydown", down, true);
    window.addEventListener("keyup", up, true);
    return () => { window.removeEventListener("keydown", down, true); window.removeEventListener("keyup", up, true); };
  }, [listening]); // eslint-disable-line react-hooks/exhaustive-deps

  // Never leave the app's shortcuts switched off: leaving the box, or the window, ends listening.
  useEffect(() => () => { void invoke("hotkeys_suspend", { suspended: false }); }, []);

  return (
    <span className="shortcut-wrap">
      <button id={p.id} ref={boxRef} type="button" className={`shortcut${listening ? " listening" : ""}`}
              onClick={() => { if (listening) { stop(); return; } setError(null); setListening(true); void invoke("hotkeys_suspend", { suspended: true }); }}
              onBlur={() => { if (listening) stop(); }}>
        {listening ? (held || "Press the keys…") : p.value}
      </button>
      {listening && <span className="shortcut-hint">Esc to cancel</span>}
      {error && <span className="shortcut-error">{error}</span>}
    </span>
  );
}

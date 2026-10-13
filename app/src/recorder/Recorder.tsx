import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "../events";

type Phase = "ready" | "recording" | "paused" | "finishing";
type Info = {
  phase: Phase | null;
  width: number; height: number;
  cursor: boolean; system_audio: boolean; microphone: boolean; noise_filter: boolean; clicks: boolean; camera: boolean; countdown: boolean;
  mic_supported: boolean;
  shortcut: string;
  pause_shortcut: string;
};
type Status = { seconds: number; level_system: number; level_mic: number; paused: boolean };

function clock(s: number) {
  const t = Math.max(0, Math.floor(s));
  const h = Math.floor(t / 3600), m = Math.floor((t % 3600) / 60), sec = t % 60;
  const mm = String(m).padStart(h ? 2 : 1, "0"), ss = String(sec).padStart(2, "0");
  return h ? `${h}:${mm}:${ss}` : `${mm}:${ss}`;
}

/** A level that rises at once and falls back gently, so a meter reads as a meter and not a flicker. */
function useDecay(target: number) {
  const [v, setV] = useState(0);
  useEffect(() => { setV((old) => (target > old ? target : old * 0.6 + target * 0.4)); }, [target]);
  return v;
}

function Meter({ level, off }: { level: number; off?: boolean }) {
  const v = useDecay(off ? 0 : level);
  // Decibels, -54 dB .. 0 dB across the bars, like any recording meter. A linear scale left normal
  // speech (about -30 dB, i.e. 0.03) at zero bars, which read as "no sound".
  const bars = 10;
  const db = v > 0 ? 20 * Math.log10(v) : -120;
  const lit = Math.max(0, Math.min(bars, Math.ceil(((db + 54) / 54) * bars)));
  return (
    <span className={`rc-meter${off ? " off" : ""}`} aria-hidden>
      {Array.from({ length: bars }, (_, i) => <i key={i} className={i < lit ? (i >= 8 ? "hot" : "on") : ""} />)}
    </span>
  );
}

/**
 * The recording control bar. Before recording: options + Record. While recording: Stop, Pause,
 * the timer and level meters. It is a non-activating panel left out of the recording itself.
 */
export function Recorder() {
  const [info, setInfo] = useState<Info | null>(null);
  const [status, setStatus] = useState<Status>({ seconds: 0, level_system: 0, level_mic: 0, paused: false });
  const [count, setCount] = useState<number | null>(null);
  const [error, setError] = useState<string | null>(null);
  const counting = useRef(false);

  const load = useCallback(() => {
    invoke<Info>("recorder_info").then((i) => {
      setInfo(i);
      if (i.phase === "ready" || i.phase === null) { setStatus({ seconds: 0, level_system: 0, level_mic: 0, paused: false }); }
    }).catch(() => {});
  }, []);
  useEffect(load, [load]);
  useEffect(() => {
    const a = listen("recorder-state", () => { setError(null); load(); });
    const b = listen<Status>("recorder-status", (e) => setStatus(e.payload));
    return () => { a.then((f) => f()); b.then((f) => f()); };
  }, [load]);

  // Reads the state FRESH from Rust instead of trusting this render's copy: after Restart the
  // "start" request arrives before the bar has re-rendered as ready, and a stale "recording" copy
  // made it ignore the request (found by a real-click test).
  const start = useCallback(async () => {
    if (counting.current) return;
    counting.current = true;
    setError(null);
    try {
      const fresh = await invoke<Info>("recorder_info");
      setInfo(fresh);
      if (fresh.phase !== "ready") return;
      if (fresh.countdown) {
        for (const n of [3, 2, 1]) { setCount(n); await new Promise((r) => setTimeout(r, 1000)); }
      }
      setCount(null);
      await invoke("recorder_start");
    } catch (e) {
      setCount(null);
      setError(String(e));
    } finally {
      counting.current = false;
    }
  }, []);

  // The record shortcut pressed while the bar is waiting = press Record.
  useEffect(() => {
    const un = listen("recorder-start-requested", () => void start());
    return () => { un.then((f) => f()); };
  }, [start]);

  const toggle = (name: keyof Info, value: boolean) => {
    if (!info) return;
    setInfo({ ...info, [name]: value });
    invoke("recorder_option", { name, value }).catch((e) => { setError(String(e)); load(); });
  };

  if (!info || info.phase === null) return <div className="recorder" />;

  if (info.phase === "ready") {
    return (
      <div className="recorder">
        <div className="rc-bar">
          {count !== null ? (
            <div className="rc-count">Recording in {count}…</div>
          ) : (
            <button className="rc-rec" onClick={() => void start()} title={`Record (${info.shortcut})`}>
              <span className="rc-dot" /> Record
            </button>
          )}
          <Toggle on={info.cursor} label="Cursor" onChange={(v) => toggle("cursor", v)} disabled={count !== null} />
          <Toggle on={info.clicks} label="Clicks" onChange={(v) => toggle("clicks", v)} disabled={count !== null}
                  title="Shows a ring in the video wherever you click" />
          <Toggle on={info.camera} label="Camera" onChange={(v) => toggle("camera", v)} disabled={count !== null}
                  title="Shows your camera in a bubble you can drag anywhere in the recording" />
          <Toggle on={info.system_audio} label="System audio" onChange={(v) => toggle("system_audio", v)} disabled={count !== null} />
          <Toggle on={info.microphone} label="Microphone" onChange={(v) => toggle("microphone", v)}
                  disabled={count !== null || !info.mic_supported}
                  title={info.mic_supported ? undefined : "Needs macOS 15 or later"} />
          <Toggle on={info.noise_filter && info.microphone} label="Noise filter" onChange={(v) => toggle("noise_filter", v)}
                  disabled={count !== null || !info.microphone}
                  title="Removes steady background noise from the microphone: fans, AC, hum, hiss" />
          <Toggle on={info.countdown} label="Countdown" onChange={(v) => toggle("countdown", v)} disabled={count !== null} />
          <span className="rc-size">{info.width} × {info.height}</span>
          <button className="rc-x" title="Cancel" onClick={() => void invoke("recorder_cancel")}>✕</button>
        </div>
        {error && <div className="rc-error">{error}</div>}
      </div>
    );
  }

  const paused = info.phase === "paused";
  return (
    <div className="recorder">
      <div className="rc-bar">
        <button className="rc-stop" title={`Stop (${info.shortcut})`} onClick={() => void invoke("recorder_stop")}
                disabled={info.phase === "finishing"}>
          <span className="rc-square" /> Stop
        </button>
        <button className="rc-btn" onClick={() => void invoke("recorder_pause", { paused: !paused })}
                disabled={info.phase === "finishing"} title={`${paused ? "Resume" : "Pause"} (${info.pause_shortcut})`}>
          {paused ? "▶ Resume" : "❚❚ Pause"}
        </button>
        <span className={`rc-time${paused ? " paused" : ""}`}>
          {info.phase === "finishing" ? "Saving…" : <>{!paused && <span className="rc-live" />}{clock(status.seconds)}</>}
        </span>
        <span className="rc-chan" title="System audio">🔊 <Meter level={status.level_system} off={!info.system_audio} /></span>
        <span className="rc-chan" title="Microphone">🎙 <Meter level={status.level_mic} off={!info.microphone} /></span>
        <button className="rc-btn" onClick={() => void invoke("recorder_restart")} disabled={info.phase === "finishing"}
                title="Throw this take away and record again from the countdown">
          ↺ Restart
        </button>
        <button className="rc-btn" onClick={() => void invoke("recorder_hide")} disabled={info.phase === "finishing"}
                title={`Hide this bar. Stop with ${info.shortcut} or from the BHCapture menu-bar icon, which also brings the bar back.`}>
          Hide
        </button>
        <button className="rc-x" title="Discard this recording" onClick={() => void invoke("recorder_cancel")}
                disabled={info.phase === "finishing"}>✕</button>
      </div>
      {error && <div className="rc-error">{error}</div>}
    </div>
  );
}

function Toggle(p: { on: boolean; label: string; onChange: (v: boolean) => void; disabled?: boolean; title?: string }) {
  return (
    <button className={`rc-toggle${p.on ? " on" : ""}`} disabled={p.disabled} title={p.title}
            onClick={() => p.onChange(!p.on)} aria-pressed={p.on}>
      <span className="rc-led" />{p.label}
    </button>
  );
}

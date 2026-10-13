import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "../events";

interface Available {
  version: string;
  asset_name: string | null;
  asset_url: string | null;
  asset_size: number;
  page_url: string;
}
interface UpdateView {
  current: string;
  auto: boolean;
  status: "idle" | "checking" | "uptodate" | "available" | "downloading" | "ready" | "failed";
  available: Available | null;
  error: string | null;
}

const PLATFORM = navigator.userAgent.includes("Mac") ? "mac" : navigator.userAgent.includes("Windows") ? "win" : "linux";

/** Settings › General › Updates -- BHServe's section: version, the automatic switch, a check
 *  button, and Download & Install when something newer is out. */
export function Updates() {
  const [v, setV] = useState<UpdateView | null>(null);
  const [problem, setProblem] = useState<string | null>(null);

  useEffect(() => {
    void invoke<UpdateView>("update_info").then(setV);
    const un = listen<UpdateView>("update-state", (e) => setV((old) => ({ ...e.payload, auto: old?.auto ?? e.payload.auto })));
    return () => { void un.then((f) => f()); };
  }, []);
  if (!v) return null;

  const run = (cmd: string, args?: Record<string, unknown>) => {
    setProblem(null);
    return invoke(cmd, args).catch((e) => setProblem(String(e)));
  };
  const mb = (n: number) => (n > 0 ? ` (${(n / 1048576).toFixed(1)} MB)` : "");
  const busy = v.status === "checking" || v.status === "downloading";

  return (
    <div className="row">
      <div className="title">Updates</div>
      <div className="help">You have BHCapture <b>v{v.current}</b>.</div>
      <label className="check">
        <input type="checkbox" checked={v.auto}
               onChange={(e) => { const on = e.target.checked; setV({ ...v, auto: on }); void run("update_set_auto", { enabled: on }); }} />
        <span><span className="t">Check for updates automatically</span>
          <span className="d">At launch and every few hours. Nothing is installed without your click.</span></span>
      </label>

      {v.status === "available" && v.available && (
        <div className="banner update">
          <b>BHCapture {v.available.version} is available</b>
          <div className="with-button">
            {v.available.asset_url && (
              <button className="primary" onClick={() => void run("update_install")}>
                Download &amp; Install{mb(v.available.asset_size)}
              </button>
            )}
            <button onClick={() => void run("update_open_page")}>What's New</button>
          </div>
          <div className="help">
            {PLATFORM === "mac" && "Opens the installer; BHCapture closes while it updates and reopens by itself. Your Screen Recording permission is kept."}
            {PLATFORM === "win" && "Starts the installer and closes BHCapture so it can be updated."}
            {PLATFORM === "linux" && "Opens the package in your software installer. Restart BHCapture when it has finished."}
          </div>
        </div>
      )}
      {v.status === "downloading" && <div className="help">Downloading the update…</div>}
      {v.status === "ready" && <div className="help">The installer has been opened — follow it to finish.</div>}
      {v.status === "uptodate" && <div className="help">✓ You're on the latest version.</div>}
      {v.status === "failed" && <div className="help">Couldn't check: {v.error}</div>}
      {problem && <div className="help">{problem}</div>}

      <div className="with-button">
        <button disabled={busy} onClick={() => void run("update_check")}>
          {v.status === "checking" ? "Checking…" : "Check Now"}
        </button>
      </div>
    </div>
  );
}

import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";

interface Device { id: string; name: string }
interface Draft {
  camera_enabled: boolean; camera_device: string; camera_shape: string; camera_size: number;
  camera_corner: string; camera_mirror: boolean; camera_enhance: boolean; camera_background: string;
  camera_bg_color: string; camera_bg_image: string | null;
}

const PRESETS: [string, string][] = [
  ["studio", "Studio grey"], ["ocean", "Ocean"], ["sunset", "Sunset"],
  ["forest", "Forest"], ["night", "Night"], ["blush", "Blush"],
];

/** Settings › Camera (decisions 34/35). Changes apply to a bubble on screen as soon as you Save. */
export function CameraSettings<T extends Draft>({ draft, set }: { draft: T; set: <K extends keyof T>(k: K, v: T[K]) => void }) {
  const [devices, setDevices] = useState<Device[] | null>(null);
  useEffect(() => { void invoke<Device[]>("camera_devices").then(setDevices).catch(() => setDevices([])); }, []);
  const s = set as (k: keyof Draft, v: unknown) => void;

  const pickImage = () => {
    void invoke<string | null>("settings_pick_image").then((p) => {
      if (p) { s("camera_bg_image", p); s("camera_background", "image"); }
    });
  };

  return (<>
    <div className="row">
      <label className="check">
        <input type="checkbox" checked={draft.camera_enabled} onChange={(e) => s("camera_enabled", e.target.checked)} />
        <span><span className="t">Show my camera in recordings</span>
          <span className="d">Also on the recording bar (Camera). The bubble appears when you select the area; drag it anywhere.</span></span>
      </label>
    </div>

    <div className="row">
      <label className="title" htmlFor="camdev">Camera</label>
      <select id="camdev" value={draft.camera_device} onChange={(e) => s("camera_device", e.target.value)}>
        <option value="">Default camera</option>
        {(devices ?? []).map((d) => <option key={d.id} value={d.id}>{d.name}</option>)}
      </select>
      {devices && devices.length === 0 && <div className="help">No camera found. Connect one, then reopen Settings.</div>}
    </div>

    <div className="row">
      <div className="title">Shape</div>
      <div className="cam-shapes">
        {([["circle", "Circle"], ["rounded", "Rounded"], ["square", "Square"], ["wide", "Wide 16:9"]] as const).map(([v, l]) => (
          <button key={v} className={draft.camera_shape === v ? "on" : ""} onClick={() => s("camera_shape", v)}>
            <span className={`cam-shape-ico ${v}`} />{l}
          </button>
        ))}
      </div>
    </div>

    <div className="row">
      <label className="title" htmlFor="camsize">Size — {draft.camera_size} pt</label>
      <input id="camsize" type="range" min={120} max={480} step={10} value={draft.camera_size}
             onChange={(e) => s("camera_size", Number(e.target.value))} />
    </div>

    <div className="row">
      <label className="title" htmlFor="camcorner">Starts in</label>
      <select id="camcorner" value={draft.camera_corner} onChange={(e) => s("camera_corner", e.target.value)}>
        <option value="bottom-right">Bottom right</option>
        <option value="bottom-left">Bottom left</option>
        <option value="top-right">Top right</option>
        <option value="top-left">Top left</option>
      </select>
      <div className="help">Of the area being recorded. You can drag the bubble anywhere before and during recording.</div>
    </div>

    <div className="row">
      <label className="check">
        <input type="checkbox" checked={draft.camera_mirror} onChange={(e) => s("camera_mirror", e.target.checked)} />
        <span><span className="t">Mirror</span><span className="d">Like looking in a mirror — what most people expect of their own camera.</span></span>
      </label>
      <label className="check">
        <input type="checkbox" checked={draft.camera_enhance} onChange={(e) => s("camera_enhance", e.target.checked)} />
        <span><span className="t">Enhance picture</span><span className="d">Brightens a dark room and evens out a flat picture. The camera runs at its best resolution (up to 1080p).</span></span>
      </label>
    </div>

    <div className="row">
      <label className="title" htmlFor="cambg">Background</label>
      <select id="cambg" value={draft.camera_background} onChange={(e) => s("camera_background", e.target.value)}>
        <option value="none">Keep my background</option>
        <option value="blur">Blur my background</option>
        <option value="color">Solid colour</option>
        {PRESETS.map(([id, label]) => <option key={id} value={`preset:${id}`}>{label}</option>)}
        <option value="image">My own picture…</option>
      </select>
      {draft.camera_background === "color" && (
        <div className="with-button">
          <input type="color" value={draft.camera_bg_color} onChange={(e) => s("camera_bg_color", e.target.value)} />
        </div>
      )}
      {draft.camera_background === "image" && (
        <div className="with-button">
          <input type="text" readOnly value={draft.camera_bg_image ?? ""} placeholder="No picture chosen" />
          <button onClick={pickImage}>Choose…</button>
        </div>
      )}
      <div className="help">Removes what is behind you and shows the background you choose. Works best with you clearly in frame and the room evenly lit.</div>
    </div>
  </>);
}

import { getCurrentWindow } from "@tauri-apps/api/window";
import { Overlay } from "./overlay/Overlay";
import { Settings } from "./settings/Settings";
import { Editor } from "./editor/Editor";
import { Recorder } from "./recorder/Recorder";
import { VideoWindow } from "./video/VideoWindow";
import { CameraBubble } from "./camera/CameraBubble";
import { Home } from "./home/Home";

/// Every window loads the same bundle; the label decides what it is. Overlays are "overlay-<display>",
/// editors are "editor-<n>", the recording bar is "recorder", recordings play in "video-<n>", and the
/// camera bubble is "camera", and the settings window is "settings".
export function App() {
  const label = getCurrentWindow().label;
  if (label.startsWith("overlay-")) return <Overlay />;
  if (label.startsWith("editor-")) return <Editor />;
  if (label === "recorder") return <Recorder />;
  if (label.startsWith("video-")) return <VideoWindow />;
  if (label === "camera") return <CameraBubble />;
  if (label === "home") return <Home />;
  return <Settings />;
}

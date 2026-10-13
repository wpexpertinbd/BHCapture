import { getCurrentWindow } from "@tauri-apps/api/window";
import { Overlay } from "./overlay/Overlay";
import { Settings } from "./settings/Settings";
import { Editor } from "./editor/Editor";

/// Every window loads the same bundle; the label decides what it is. Overlays are "overlay-<display>",
/// editors are "editor-<n>", and the settings window is "settings".
export function App() {
  const label = getCurrentWindow().label;
  if (label.startsWith("overlay-")) return <Overlay />;
  if (label.startsWith("editor-")) return <Editor />;
  return <Settings />;
}

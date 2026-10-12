import { getCurrentWindow } from "@tauri-apps/api/window";
import { Overlay } from "./overlay/Overlay";
import { Settings } from "./settings/Settings";

/// Every window loads the same bundle; the label decides what it is. Overlays are created one
/// per display and labelled "overlay-<displayId>".
export function App() {
  const label = getCurrentWindow().label;
  return label.startsWith("overlay-") ? <Overlay /> : <Settings />;
}

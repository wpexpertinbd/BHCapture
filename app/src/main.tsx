import React from "react";
import ReactDOM from "react-dom/client";
import { App } from "./App";
import "./overlay/overlay.css";
import "./settings/settings.css";
import "./editor/editor.css";
import "./recorder/recorder.css";
import "./video/video.css";
import "./camera/camera.css";

ReactDOM.createRoot(document.getElementById("root")!).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>
);

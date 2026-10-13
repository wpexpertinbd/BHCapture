import React from "react";
import ReactDOM from "react-dom/client";
import { App } from "./App";
import "./overlay/overlay.css";
import "./settings/settings.css";
import "./editor/editor.css";

ReactDOM.createRoot(document.getElementById("root")!).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>
);

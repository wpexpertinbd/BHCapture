import React from "react";
import ReactDOM from "react-dom/client";
import { Overlay } from "./overlay/Overlay";
import "./overlay/overlay.css";

ReactDOM.createRoot(document.getElementById("root")!).render(
  <React.StrictMode>
    <Overlay />
  </React.StrictMode>
);

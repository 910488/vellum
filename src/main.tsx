import React from "react";
import ReactDOM from "react-dom/client";
import { App } from "@/App";
import "@/i18n";
import { applyThemePreference, readThemePreference } from "@/lib/theme";

import "@/styles/tokens.css";
import "@/styles/base.css";
import "@/styles/components.css";

/* 在第一次 render 之前套用，不然選了淺色的人在深色系統上會先閃一下暗底。 */
applyThemePreference(readThemePreference());

const root = document.getElementById("root");
if (!root) throw new Error("#root not found");

ReactDOM.createRoot(root).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>,
);

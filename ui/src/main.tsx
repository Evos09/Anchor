import { StrictMode, useEffect, useState } from "react";
import { createRoot } from "react-dom/client";
import { App } from "./App";
import { get, hasToken } from "./api";
import { Workspace } from "./panels/Workspace";
import "./styles.css";

/** `anchor app` serves the desktop app; everything else the dashboard. */
function Root() {
  const [mode, setMode] = useState<string | null>(hasToken() ? null : "dashboard");
  useEffect(() => {
    if (mode !== null) return;
    get<{ mode: string }>("/api/overview").then(
      (o) => setMode(o.mode === "app" ? "app" : "dashboard"),
      // The dashboard shows the error (or the token gate).
      () => setMode("dashboard"),
    );
  }, [mode]);
  if (mode === null) return null;
  return mode === "app" ? <Workspace /> : <App />;
}

const root = document.getElementById("root");
if (root) {
  createRoot(root).render(
    <StrictMode>
      <Root />
    </StrictMode>,
  );
}

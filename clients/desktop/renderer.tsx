import { initializeAppearance } from "../shared/appearance";
import { observeDesktopAppearance } from "./appearance";
import { createRoot } from "react-dom/client";
import App from "../shared/App";
import "../shared/style.css";
import { EditorSessionsProvider } from "../shared/editors";
initializeAppearance();
const stopAppearance = observeDesktopAppearance();
import.meta.hot?.dispose(stopAppearance);
createRoot(document.getElementById("root")!).render(
  <EditorSessionsProvider>
    <App />
  </EditorSessionsProvider>,
);

import { initializeAppearance } from "../shared/appearance";
import { createRoot } from "react-dom/client";
import App from "../shared/App";
import "../shared/style.css";
import { EditorSessionsProvider } from "../shared/editors";
initializeAppearance();
createRoot(document.getElementById("root")!).render(
  <EditorSessionsProvider>
    <App />
  </EditorSessionsProvider>,
);

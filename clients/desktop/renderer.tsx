import { createRoot } from "react-dom/client";
import App from "../shared/App";
import "../shared/style.css";
import { EditorSessionsProvider } from "../shared/editors";
createRoot(document.getElementById("root")!).render(
  <EditorSessionsProvider>
    <App />
  </EditorSessionsProvider>,
);

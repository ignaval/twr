import { createRoot } from "react-dom/client";
import App from "./App";
import "./style.css";
import { EditorSessionsProvider } from "./editors";
createRoot(document.getElementById("root")!).render(
  <EditorSessionsProvider>
    <App />
  </EditorSessionsProvider>,
);

import { useEffect, useState } from "react";
import type { UpdateStatus } from "../shared/types";

const DISMISSED = "twr.update.dismissed";
function dismissedVersion() {
  try {
    return localStorage.getItem(DISMISSED) || "";
  } catch {
    return "";
  }
}

/** A quiet, dismissible notice when a newer desktop release exists. */
export function UpdateBanner() {
  const [status, setStatus] = useState<UpdateStatus>();
  const [dismissed, setDismissed] = useState(dismissedVersion);
  const [error, setError] = useState("");
  const [later, setLater] = useState(false);
  useEffect(() => {
    const app = window.twrApp;
    if (!app) return;
    void app
      .settings()
      .then((settings) => setStatus(settings.update))
      .catch(() => {});
    return app.onUpdate(setStatus);
  }, []);
  if (!status?.latest) return null;
  const ready = status.state === "downloaded";
  const available = status.state === "available";
  const downloading = status.state === "downloading";
  if (!ready && !available && !downloading) return null;
  // Only a downloaded update keeps asking; other notices stay dismissed per version.
  if (!ready && dismissed === status.latest) return null;
  if (ready && later) return null;
  const dismiss = () => {
    if (ready) return setLater(true);
    try {
      localStorage.setItem(DISMISSED, status.latest!);
    } catch {}
    setDismissed(status.latest!);
  };
  const run = (task: () => Promise<unknown>) =>
    void task().catch((e) =>
      setError(e instanceof Error ? e.message : String(e)),
    );
  return (
    <div className="update-banner" role="status" aria-live="polite">
      <div>
        <strong>
          {ready ? (
            `twr ${status.latest} is ready`
          ) : downloading ? (
            <>
              Downloading twr {status.latest}…{" "}
              <span aria-live="off">{status.progress ?? 0}%</span>
            </>
          ) : (
            `twr ${status.latest} is available`
          )}
        </strong>
        {ready && <p>It also installs the next time you quit twr.</p>}
        {!ready && !downloading && !status.automatic && status.instructions && (
          <p>{status.instructions}</p>
        )}
        {error && <p className="update-error">{error}</p>}
      </div>
      <div className="update-actions">
        {ready && (
          <button
            className="primary"
            onClick={() => run(() => window.twrApp!.installUpdate())}
          >
            Restart to update
          </button>
        )}
        {available && status.automatic && (
          <button
            className="primary"
            onClick={() => run(() => window.twrApp!.downloadUpdate())}
          >
            Download
          </button>
        )}
        {available && !status.automatic && status.download && (
          <button
            className="primary"
            onClick={() =>
              run(() => window.twrLocal.openExternal(status.download!.url))
            }
          >
            Download
          </button>
        )}
        {available &&
          !status.automatic &&
          !status.download &&
          status.releaseUrl && (
            <button
              onClick={() =>
                run(() => window.twrLocal.openExternal(status.releaseUrl!))
              }
            >
              Release notes
            </button>
          )}
        {!downloading && (
          <button onClick={dismiss} aria-label="Dismiss update notice">
            {ready ? "Later" : "Dismiss"}
          </button>
        )}
      </div>
    </div>
  );
}

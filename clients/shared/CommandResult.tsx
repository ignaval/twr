import { ExternalLink } from "lucide-react";
import { DataView, Markdown } from "./ui";
import { resultSummary } from "./results";
import type { Json } from "./types";

/** A command form's result: in words when `shared/results.ts` has a formatter for its
 * method (the data itself under Details), else as data. */
export function CommandResult({
  method,
  value,
  values,
  onOpenThread,
  onOpenLink,
  onUpdateSettings,
  onError,
}: {
  method?: string;
  value: Json;
  values: Json;
  onOpenThread: (id: string) => void;
  onOpenLink: (url: string) => void;
  onUpdateSettings: () => void;
  onError: (error: unknown) => void;
}) {
  const summary = resultSummary(method, value, values);
  if (!summary) return <DataView value={value} />;
  const thread = summary.openThread;
  const restart = summary.restart && window.twrApp;
  return (
    <div className="command-result">
      {summary.text && <p className="success-text">{summary.text}</p>}
      {summary.warning && (
        <p className="error-text" role="alert">
          {summary.warning}
        </p>
      )}
      {summary.table && (
        <div className="data-table-wrap">
          <table className="data-table">
            <thead>
              <tr>
                {summary.table.columns.map((c) => (
                  <th key={c}>{c}</th>
                ))}
              </tr>
            </thead>
            <tbody>
              {summary.table.rows.map((row, i) => (
                <tr key={i}>
                  {row.map((cell, j) => (
                    <td key={j}>{cell}</td>
                  ))}
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
      {summary.list && summary.list.length > 0 && (
        <ul className="command-result-list">
          {summary.list.map((line, i) => (
            <li key={i}>{line}</li>
          ))}
        </ul>
      )}
      {summary.body !== undefined && <Markdown text={summary.body} />}
      {(thread || summary.link || restart || summary.updateSettings) && (
        <div className="button-row">
          {thread && (
            <button type="button" onClick={() => onOpenThread(thread.id)}>
              Open {thread.name}
            </button>
          )}
          {restart && (
            <button
              type="button"
              className="primary"
              onClick={() => void window.twrApp!.installUpdate().catch(onError)}
            >
              Restart to update
            </button>
          )}
          {summary.link && (
            <button type="button" onClick={() => onOpenLink(summary.link!.url)}>
              {summary.link.label} <ExternalLink size={14} aria-hidden="true" />
            </button>
          )}
          {summary.updateSettings && (
            <button type="button" onClick={onUpdateSettings}>
              Update settings
            </button>
          )}
        </div>
      )}
      <details className="command-result-details">
        <summary>Details</summary>
        <DataView value={value} />
      </details>
    </div>
  );
}

import type { DeleteBrief } from "./types";

/** A delete card in short: its question, a line per thread with what it loses (the threads
 * that lose nothing together on one line), and what goes for good; the hub's whole summary
 * (every path, size and branch) behind Details. */
export function DeleteBriefView({
  brief,
  summary,
  id,
}: {
  brief: DeleteBrief;
  summary: string;
  id: string;
}) {
  const clean = brief.threads.filter((thread) => thread.clean);
  const grouped = clean.length > 1;
  const lines = grouped
    ? brief.threads.filter((thread) => !thread.clean)
    : brief.threads;
  return (
    <>
      <div className="delete-brief" id={id}>
        <p className="delete-brief-title">{brief.title}</p>
        {brief.note && <p className="delete-brief-note">{brief.note}</p>}
        <ul>
          {lines.map((thread, i) => (
            <li
              key={thread.name + ":" + i}
              data-refused={thread.refused || undefined}
            >
              <b>{thread.name}</b>
              {thread.project && (
                <span className="delete-brief-project">{thread.project}</span>
              )}
              <span className="visually-hidden">: </span>
              <span className="delete-brief-risk">{thread.risk}</span>
            </li>
          ))}
          {grouped && (
            <li className="delete-brief-clean">
              <b>{clean.map((thread) => thread.name).join(", ")}</b>
              <span className="visually-hidden">: </span>
              <span className="delete-brief-risk">{clean[0].risk}</span>
            </li>
          )}
        </ul>
        {brief.total && <p className="delete-brief-total">{brief.total}</p>}
      </div>
      <details className="delete-details">
        <summary>Details</summary>
        <pre className="attention-summary">{summary}</pre>
      </details>
    </>
  );
}

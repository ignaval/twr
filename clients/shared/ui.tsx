import { useHostAPI } from "./host";
import { memo, useId, useRef, type ReactNode } from "react";
import { focusableWithin, useModal } from "./a11y";
import { X, LoaderCircle, FolderOpen, Copy, Check } from "lucide-react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import type { Field } from "./commands";
export function Spinner() {
  return (
    <LoaderCircle size={16} className="spin" role="img" aria-label="Loading" />
  );
}
export function Dialog({
  title,
  subtitle,
  children,
  onClose,
  wide = false,
}: {
  title: string;
  subtitle?: string;
  children: ReactNode;
  onClose: () => void;
  wide?: boolean;
}) {
  const root = useRef<HTMLDivElement>(null);
  const titleId = useId();
  const subtitleId = useId();
  useModal(root, {
    onClose,
    container: (node) => node.parentElement,
    initialFocus: (node) => {
      const nodes = focusableWithin(node);
      return (
        nodes.find((n) =>
          n.matches("input:not([type=checkbox]), textarea, select"),
        ) || nodes.find((n) => n.matches("button"))
      );
    },
  });
  return (
    <div
      className="scrim"
      onMouseDown={(event) => {
        if (event.target === event.currentTarget) onClose();
      }}
    >
      <div
        className={`dialog ${wide ? "wide" : ""}`}
        role="dialog"
        aria-modal="true"
        aria-labelledby={titleId}
        aria-describedby={subtitle ? subtitleId : undefined}
        ref={root}
      >
        <header>
          <div>
            <h2 id={titleId}>{title}</h2>
            {subtitle && <p id={subtitleId}>{subtitle}</p>}
          </div>
          <button
            className="icon-button"
            onClick={onClose}
            aria-label="Close dialog"
            title="Close"
          >
            <X size={18} />
          </button>
        </header>
        {children}
      </div>
    </div>
  );
}
export function FieldInput({
  field,
  value,
  onChange,
}: {
  field: Field;
  value: any;
  onChange: (v: any) => void;
}) {
  const hostApi = useHostAPI();
  const id = `field-${field.key}`;
  return (
    <label
      className={`field ${field.type === "boolean" ? "check-field" : ""}`}
      htmlFor={id}
    >
      <span>
        {field.label}
        {field.required && (
          <span className="required" aria-hidden="true">
            {" "}
            *
          </span>
        )}
      </span>
      {field.type === "boolean" ? (
        <input
          id={id}
          type="checkbox"
          checked={!!value}
          onChange={(e) => onChange(e.target.checked)}
        />
      ) : field.type === "select" ? (
        <select
          id={id}
          aria-required={field.required || undefined}
          value={value ?? ""}
          onChange={(e) => onChange(e.target.value)}
        >
          <option value="">
            {field.required ? "Choose…" : "Keep default"}
          </option>
          {field.options?.map((o) => (
            <option key={o}>{o}</option>
          ))}
        </select>
      ) : field.type === "textarea" ? (
        <textarea
          id={id}
          aria-required={field.required || undefined}
          rows={5}
          value={value ?? ""}
          placeholder={field.placeholder}
          onChange={(e) => onChange(e.target.value)}
        />
      ) : (
        <div className="input-with-button">
          <input
            id={id}
            aria-required={field.required || undefined}
            type={field.type === "number" ? "number" : "text"}
            value={value ?? ""}
            placeholder={field.placeholder}
            onChange={(e) => onChange(e.target.value)}
            autoComplete="off"
            list={field.options ? `suggest-${field.key}` : undefined}
          />
          {field.options && (
            <datalist id={`suggest-${field.key}`}>
              {field.options.map((option) => (
                <option key={option} value={option} />
              ))}
            </datalist>
          )}
          {field.type === "directory" && (
            <button
              type="button"
              aria-label="Choose folder"
              title="Choose folder"
              onClick={() =>
                void hostApi.pickDirectory().then((path) => {
                  if (path) onChange(path);
                })
              }
            >
              <FolderOpen size={16} />
            </button>
          )}
        </div>
      )}
      {field.description && <small>{field.description}</small>}
    </label>
  );
}
export const Markdown = memo(function Markdown({ text }: { text: string }) {
  const hostApi = useHostAPI();
  return (
    <div className="markdown">
      <ReactMarkdown
        remarkPlugins={[remarkGfm]}
        components={{
          a: ({ href, children }) => (
            <a
              href={href}
              onClick={(e) => {
                e.preventDefault();
                if (href) void hostApi.openExternal(href).catch(() => {});
              }}
            >
              {children}
            </a>
          ),
          img: ({ alt }) => (
            <span className="muted">[Image: {alt || "attachment"}]</span>
          ),
          pre: ({ children }) => (
            <div className="code-block">
              <pre>{children}</pre>
              <button
                className="copy-code"
                aria-label="Copy code"
                onClick={(e) =>
                  void hostApi.copyText(
                    e.currentTarget.parentElement?.querySelector("pre")
                      ?.textContent || "",
                  )
                }
              >
                <Copy size={14} />
              </button>
            </div>
          ),
        }}
      >
        {text}
      </ReactMarkdown>
    </div>
  );
});
export function DataView({ value }: { value: any }) {
  if (value === null || value === undefined)
    return <p className="muted">No data reported.</p>;
  if (typeof value === "string")
    return <pre className="plain-output">{value}</pre>;
  if (typeof value !== "object") return <span>{String(value)}</span>;
  if (Array.isArray(value)) {
    if (!value.length) return <p className="muted">Nothing here yet.</p>;
    if (value.every((v) => v && typeof v === "object" && !Array.isArray(v))) {
      const keys = [...new Set(value.flatMap((v) => Object.keys(v)))]
        .filter((k) =>
          value.some((v) =>
            ["string", "number", "boolean"].includes(typeof v[k]),
          ),
        )
        .slice(0, 8);
      if (keys.length)
        return (
          <div className="data-table-wrap">
            <table className="data-table">
              <thead>
                <tr>
                  {keys.map((k) => (
                    <th key={k}>{k.replaceAll("_", " ")}</th>
                  ))}
                </tr>
              </thead>
              <tbody>
                {value.map((row, i) => (
                  <tr key={row.id || row.name || i}>
                    {keys.map((k) => (
                      <td key={k}>
                        {typeof row[k] === "object"
                          ? "—"
                          : String(row[k] ?? "—")}
                      </td>
                    ))}
                  </tr>
                ))}
              </tbody>
            </table>
            <details>
              <summary>Full details</summary>
              <pre>{JSON.stringify(value, null, 2)}</pre>
            </details>
          </div>
        );
    }
    return (
      <div className="data-list">
        {value.map((v, i) => (
          <DataView key={i} value={v} />
        ))}
      </div>
    );
  }
  return (
    <div className="record">
      {Object.entries(value).map(([key, item]) => (
        <div key={key} className="record-field">
          <span className="record-label">{key.replaceAll("_", " ")}</span>
          {item && typeof item === "object" ? (
            <details open={Array.isArray(item) && item.length < 8}>
              <summary>
                {Array.isArray(item) ? `${item.length} items` : "Details"}
              </summary>
              <DataView value={item} />
            </details>
          ) : (
            <span className="record-value">{String(item ?? "—")}</span>
          )}
        </div>
      ))}
    </div>
  );
}
export function Empty({
  title,
  description,
  children,
}: {
  title: string;
  description: string;
  children?: ReactNode;
}) {
  return (
    <div className="empty">
      <div className="empty-mark">
        twr<span>_</span>
      </div>
      <h2>{title}</h2>
      <p>{description}</p>
      {children}
    </div>
  );
}
export function formatNumber(number: number) {
  return new Intl.NumberFormat(undefined, {
    notation: "compact",
    maximumFractionDigits: 1,
  }).format(number);
}
export function ResourceTable({
  rows,
  columns,
  actions,
  empty = "Nothing here yet.",
}: {
  rows: any[];
  columns: { key: string; label: string; render?: (row: any) => ReactNode }[];
  actions?: (row: any) => ReactNode;
  empty?: string;
}) {
  if (!rows.length) return <p className="muted table-empty">{empty}</p>;
  return (
    <div className="data-table-wrap">
      <table className="data-table resource-table">
        <thead>
          <tr>
            {columns.map((c) => (
              <th key={c.key}>{c.label}</th>
            ))}
            {actions && <th>Actions</th>}
          </tr>
        </thead>
        <tbody>
          {rows.map((row, i) => (
            <tr key={row.item || row.job || row.id || row.name || i}>
              {columns.map((c) => (
                <td key={c.key}>
                  {c.render ? c.render(row) : String(row[c.key] ?? "—")}
                </td>
              ))}
              {actions && (
                <td>
                  <div className="row-actions">{actions(row)}</div>
                </td>
              )}
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

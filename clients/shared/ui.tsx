import { useHostAPI } from "./host";
import {
  isValidElement,
  memo,
  useCallback,
  useEffect,
  useId,
  useRef,
  useState,
  type ReactNode,
} from "react";
import { announce, focusableWithin, useModal } from "./a11y";
import { errorMessage } from "./errors";
import { externalLink, linkConfirmation } from "./links";
import { X, LoaderCircle, FolderOpen, Copy, Check, List } from "lucide-react";
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
      ) : (field.type === "model" || field.type === "effort") &&
        field.choices?.length ? (
        <ModelSelect
          id={id}
          value={value ?? ""}
          onChange={onChange}
          choices={field.choices}
          keep={field.keep}
          custom={field.type === "model"}
          required={field.required}
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
const CUSTOM = "\u0000custom";
/** A model (or effort) picker: the runtime's default first (or, with `keep`, keeping the
 * current one, `default` clearing it), the catalog's choices, then (`custom`) a custom id,
 * typed in a text box that a button turns back into the list. A value the list doesn't have
 * shows as a custom id. */
export function ModelSelect({
  id,
  value,
  onChange,
  choices,
  keep = false,
  custom = true,
  required = false,
  label,
}: {
  id?: string;
  value: string;
  onChange: (v: string) => void;
  choices: { value: string; description?: string }[];
  keep?: boolean;
  custom?: boolean;
  required?: boolean;
  label?: string;
}) {
  const listed =
    value === "" ||
    (keep && value === "default") ||
    choices.some((c) => c.value === value);
  const [typing, setTyping] = useState(!listed);
  if (custom && (typing || !listed))
    return (
      <div className="input-with-button">
        <input
          id={id}
          aria-label={label}
          aria-required={required || undefined}
          type="text"
          value={value}
          placeholder="Model id, as the runtime names it"
          onChange={(e) => onChange(e.target.value)}
          autoComplete="off"
          spellCheck={false}
        />
        <button
          type="button"
          aria-label="Choose from the list"
          title="Choose from the list"
          onClick={() => {
            setTyping(false);
            onChange("");
          }}
        >
          <List size={16} />
        </button>
      </div>
    );
  return (
    <select
      id={id}
      aria-label={label}
      aria-required={required || undefined}
      value={listed ? value : ""}
      onChange={(e) => {
        if (e.target.value === CUSTOM) {
          setTyping(true);
          onChange("");
        } else onChange(e.target.value);
      }}
    >
      <option value="">{keep ? "Keep current" : "Runtime default"}</option>
      {keep && <option value="default">Runtime default</option>}
      {choices.map((c) => (
        <option key={c.value} value={c.value}>
          {c.description ? `${c.value} — ${c.description}` : c.value}
        </option>
      ))}
      {custom && <option value={CUSTOM}>Custom model id…</option>}
    </select>
  );
}
/** A copy button that says it copied: its icon turns into a check for a moment and a
 * screen reader hears "Copied". A failure goes to `onError` (or is announced). */
export function CopyButton({
  text,
  label,
  className = "icon-button",
  size = 14,
  onError,
}: {
  text: () => string;
  label: string;
  className?: string;
  size?: number;
  onError?: (error: unknown) => void;
}) {
  const hostApi = useHostAPI();
  const [copied, setCopied] = useState(false);
  useEffect(() => {
    if (!copied) return;
    const timer = setTimeout(() => setCopied(false), 1500);
    return () => clearTimeout(timer);
  }, [copied]);
  return (
    <button
      type="button"
      className={className}
      aria-label={label}
      title={copied ? "Copied" : label}
      onClick={() =>
        void hostApi
          .copyText(text())
          .then(() => {
            setCopied(true);
            announce("Copied");
          })
          .catch((error) => {
            if (onError) onError(error);
            else
              announce("Could not copy: " + errorMessage(error), "assertive");
          })
      }
    >
      {copied ? (
        <Check size={size} aria-hidden="true" />
      ) : (
        <Copy size={size} aria-hidden="true" />
      )}
    </button>
  );
}
/** Opens a link from an agent's text or a terminal: at once when it is what it looks
 * like, after a confirmation showing the full address when not (shared/links.ts). */
type OpenLink = (href: string, text: string, terminal?: boolean) => void;
/** The mounted `LinkConfirm` (the app has one), which shows the confirmation. */
let confirmLinks: OpenLink | undefined;
export function useOpenLink(): OpenLink {
  const hostApi = useHostAPI();
  return (href, text, terminal) => {
    if (confirmLinks) return confirmLinks(href, text, terminal);
    const url = externalLink(href);
    if (!url) return;
    const reason = linkConfirmation({ href, text, terminal });
    if (reason && !window.confirm(reason + "\n\n" + url.href)) return;
    void hostApi.openExternal(url.href).catch(() => {});
  };
}
/** Asks before a link that needs it (`useOpenLink`) opens, showing its full address. */
export function LinkConfirm({
  onError,
}: {
  onError: (error: unknown) => void;
}) {
  const hostApi = useHostAPI();
  const [pending, setPending] = useState<{
    href: string;
    reason: string;
    openable: boolean;
  }>();
  const open = useCallback<OpenLink>(
    (href, text, terminal = false) => {
      const url = externalLink(href);
      if (!url) {
        setPending({
          href,
          reason:
            "Only web and email links open from here. Copy this address to use it.",
          openable: false,
        });
        return;
      }
      const reason = linkConfirmation({ href, text, terminal });
      if (reason) setPending({ href: url.href, reason, openable: true });
      else void hostApi.openExternal(url.href).catch(onError);
    },
    [hostApi, onError],
  );
  useEffect(() => {
    confirmLinks = open;
    return () => {
      if (confirmLinks === open) confirmLinks = undefined;
    };
  }, [open]);
  const close = () => setPending(undefined);
  return (
    <>
      {pending && (
        <Dialog
          title={pending.openable ? "Open this link?" : "Link not opened"}
          subtitle={pending.reason}
          onClose={close}
        >
          <div className="link-confirm">
            <p>{pending.openable ? "It opens:" : "Its address:"}</p>
            <code className="link-destination">{pending.href}</code>
          </div>
          <footer className="dialog-footer">
            <span />
            <div>
              <button type="button" onClick={close}>
                Cancel
              </button>
              {pending.openable ? (
                <button
                  type="button"
                  className="primary"
                  onClick={() => {
                    close();
                    void hostApi.openExternal(pending.href).catch(onError);
                  }}
                >
                  Open link
                </button>
              ) : (
                <CopyButton
                  className="copy-address"
                  text={() => pending.href}
                  label="Copy address"
                  onError={onError}
                />
              )}
            </div>
          </footer>
        </Dialog>
      )}
    </>
  );
}
function textOf(node: ReactNode): string {
  if (node === null || node === undefined || typeof node === "boolean")
    return "";
  if (typeof node === "string" || typeof node === "number") return String(node);
  if (Array.isArray(node)) return node.map(textOf).join("");
  if (isValidElement<{ children?: ReactNode }>(node))
    return textOf(node.props.children);
  return "";
}
/** A Markdown link: a web or email link opens (through `useOpenLink`) and names its
 * address on hover; anything else (a path like `src/App.tsx:42`) is code to copy. */
function MarkdownLink({
  href,
  children,
}: {
  href?: string;
  children?: ReactNode;
}) {
  const open = useOpenLink();
  const url = externalLink(href);
  if (!url)
    return (
      <span className="link-text">
        <code title={href || undefined}>{children}</code>
        <CopyButton
          className="icon-button copy-link"
          size={12}
          text={() => href || textOf(children)}
          label={"Copy " + (href || textOf(children))}
        />
      </span>
    );
  return (
    <a
      href={url.href}
      title={url.href}
      onClick={(event) => {
        event.preventDefault();
        open(url.href, textOf(children));
      }}
    >
      {children}
    </a>
  );
}
const markdownComponents = {
  a: ({ href, children }: { href?: string; children?: ReactNode }) => (
    <MarkdownLink href={href}>{children}</MarkdownLink>
  ),
  img: ({ alt }: { alt?: string }) => (
    <span className="muted">[Image: {alt || "attachment"}]</span>
  ),
  pre: ({ children }: { children?: ReactNode }) => (
    <div className="code-block">
      <pre>{children}</pre>
      <CopyButton
        className="copy-code"
        label="Copy code"
        text={() => textOf(children)}
      />
    </div>
  ),
};
export const Markdown = memo(function Markdown({ text }: { text: string }) {
  return (
    <div className="markdown">
      <ReactMarkdown
        remarkPlugins={[remarkGfm]}
        components={markdownComponents}
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

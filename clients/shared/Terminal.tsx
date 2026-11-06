import { useHostAPI } from "./host";
import { useEffect, useRef, useState } from "react";
import { Terminal as XTerm } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import { SearchAddon } from "@xterm/addon-search";
import { WebLinksAddon } from "@xterm/addon-web-links";
import "@xterm/xterm/css/xterm.css";
import type { TerminalSession } from "./types";
export function Terminal({
  session,
  visible,
}: {
  session: TerminalSession;
  visible: boolean;
}) {
  const hostApi = useHostAPI();
  const node = useRef<HTMLDivElement>(null);
  const terminal = useRef<XTerm>(null);
  const [query, setQuery] = useState("");
  const control = useRef(false);
  const [controlPressed, setControlPressed] = useState(false);
  const search = useRef<SearchAddon>(null);
  const resize = useRef<() => void>(() => {});
  useEffect(() => {
    const term = new XTerm({
      fontFamily:
        document.documentElement.dataset.platform === "darwin"
          ? '"SF Mono", monospace'
          : "monospace",
      fontSize: 13,
      lineHeight: 1.25,
      scrollback: 10000,
      cursorBlink: true,
      allowProposedApi: false,
      linkHandler: {
        activate: (_event, uri) =>
          void hostApi.openExternal(uri).catch(() => {}),
      },
      theme: {
        background: "#0b0d0f",
        foreground: "#e6e9ec",
        cursor: "#62c7d4",
        selectionBackground: "#29434a",
      },
    });
    const addon = new FitAddon();
    const finder = new SearchAddon();
    term.loadAddon(addon);
    term.loadAddon(finder);
    term.loadAddon(
      new WebLinksAddon(
        (_event, url) => void hostApi.openExternal(url).catch(() => {}),
      ),
    );
    term.open(node.current!);
    const colorContext = document.createElement("canvas").getContext("2d");
    let previousTheme = "";
    const updateTheme = () => {
      const root = document.documentElement;
      const colors = getComputedStyle(root);
      const read = (name: string) => colors.getPropertyValue(name).trim();
      // xterm accepts concrete CSS colors; Canvas resolves color-mix for us when present.
      const resolve = (value: string) => {
        if (!colorContext) return value;
        colorContext.fillStyle = value;
        return colorContext.fillStyle as string;
      };
      const theme = {
        background: resolve(read("--terminal-bg")),
        foreground: resolve(read("--terminal-text")),
        cursor: resolve(read("--terminal-accent")),
        selectionBackground: resolve(read("--terminal-selection")),
      };
      const signature = JSON.stringify(theme);
      if (signature === previousTheme) return;
      previousTheme = signature;
      term.options.theme = theme;
    };
    updateTheme();
    const themeObserver = new MutationObserver(updateTheme);
    themeObserver.observe(document.documentElement, { attributes: true });
    const scheme = matchMedia("(prefers-color-scheme: light)");
    scheme.addEventListener("change", updateTheme);
    terminal.current = term;
    search.current = finder;
    let ready = false;
    let sequence = 0;
    const queued: { data: string; sequence: number }[] = [];
    const unsubscribe = hostApi.onEvent((event) => {
      if (event.type === "resync" && event.scope !== "control") {
        snapshot(true);
        return;
      }
      if (event.id !== session.id) return;
      if (event.type === "terminal-data") {
        const chunk = { data: event.data || "", sequence: event.sequence || 0 };
        if (ready) {
          if (chunk.sequence > sequence + 1) {
            queued.push(chunk);
            snapshot(true);
            return;
          }
          if (chunk.sequence > sequence) {
            sequence = chunk.sequence;
            term.write(chunk.data, () =>
              hostApi.terminalAck(session.id, chunk.data.length),
            );
          }
        } else queued.push(chunk);
      }
      if (event.type === "terminal-exit")
        term.write(
          `\r\n\x1b[90mProcess exited (${event.exitCode}).\x1b[0m\r\n`,
        );
    });
    let snapshotLoading = false;
    function snapshot(reset = false) {
      if (snapshotLoading) return;
      ready = false;
      snapshotLoading = true;
      if (reset) term.reset();
      void hostApi
        .terminalRead(session.id)
        .then((snapshot) => {
          if (terminal.current !== term) return;
          term.write(snapshot.data);
          if (snapshot.exited)
            term.write("\r\n\x1b[90mProcess exited.\x1b[0m\r\n");
          sequence = snapshot.sequence;
          ready = true;
          for (const chunk of queued)
            if (chunk.sequence > sequence) {
              term.write(chunk.data, () =>
                hostApi.terminalAck(session.id, chunk.data.length),
              );
              sequence = chunk.sequence;
            }
          queued.length = 0;
        })
        .catch(() => {
          if (terminal.current !== term) return;
          ready = true;
          term.write(
            "\r\n\x1b[90mCould not restore terminal history; showing live output.\x1b[0m\r\n",
          );
          for (const chunk of queued.splice(0)) {
            if (chunk.sequence <= sequence) continue;
            sequence = chunk.sequence;
            term.write(chunk.data, () =>
              hostApi.terminalAck(session.id, chunk.data.length),
            );
          }
        })
        .finally(() => {
          snapshotLoading = false;
        });
    }
    snapshot();
    const input = term.onData((data) => {
      if (control.current && data.length === 1) {
        const code = data.toUpperCase().charCodeAt(0);
        if (code >= 64 && code <= 95) data = String.fromCharCode(code - 64);
        control.current = false;
        setControlPressed(false);
      }
      hostApi.terminalWrite(session.id, data);
    });
    let previousSize = "";
    const fitAndResize = () => {
      if (!node.current?.offsetWidth) return;
      addon.fit();
      const size = `${term.cols}x${term.rows}`;
      if (size === previousSize) return;
      previousSize = size;
      hostApi.terminalResize(session.id, term.cols, term.rows);
    };
    resize.current = fitAndResize;
    const observer = new ResizeObserver(fitAndResize);
    observer.observe(node.current!);
    const key = term.attachCustomKeyEventHandler((event) => {
      if (
        (event.ctrlKey || event.metaKey) &&
        event.shiftKey &&
        event.key.toLowerCase() === "c"
      ) {
        void hostApi.copyText(term.getSelection());
        return false;
      }
      return true;
    });
    return () => {
      unsubscribe();
      input.dispose();
      observer.disconnect();
      themeObserver.disconnect();
      scheme.removeEventListener("change", updateTheme);
      hostApi.terminalFocus(false);
      terminal.current = null;
      resize.current = () => {};
      term.dispose();
    };
  }, [session.id]);
  useEffect(() => {
    if (visible) {
      requestAnimationFrame(() => {
        resize.current();
        terminal.current?.focus();
      });
    }
  }, [visible, session.id]);
  return (
    <section
      className="terminal-session"
      data-session-id={session.id}
      style={{ display: visible ? "flex" : "none" }}
      aria-label={`${session.title}${session.exited ? " (exited)" : ""}`}
    >
      <div className="terminal-search">
        <span>
          {session.workspace} / {session.title}
        </span>
        <input
          aria-label="Search terminal"
          value={query}
          onChange={(e) => {
            setQuery(e.target.value);
            search.current?.findNext(e.target.value);
          }}
          placeholder="Find in terminal…"
          onKeyDown={(e) => {
            if (e.key === "Enter") search.current?.findNext(query);
          }}
        />
      </div>
      <div
        className="terminal-surface"
        ref={node}
        onFocus={() => hostApi.terminalFocus(true)}
        onBlur={() => hostApi.terminalFocus(false)}
      />
      {document.documentElement.dataset.mobile === "true" && (
        <div className="terminal-accessory" aria-label="Terminal keys">
          <button
            onPointerDown={(e) => e.preventDefault()}
            onClick={() => {
              control.current = !control.current;
              setControlPressed(control.current);
              terminal.current?.focus();
            }}
            aria-pressed={controlPressed}
          >
            Ctrl
          </button>
          {[
            ["Esc", "\u001b"],
            ["Tab", "\t"],
            ["↑", "\u001b[A"],
            ["↓", "\u001b[B"],
            ["←", "\u001b[D"],
            ["→", "\u001b[C"],
          ].map(([label, value]) => (
            <button
              key={label}
              onPointerDown={(e) => e.preventDefault()}
              onClick={() => {
                const data =
                  terminal.current?.modes.applicationCursorKeysMode &&
                  value.startsWith("\u001b[")
                    ? value.replace("\u001b[", "\u001bO")
                    : value;
                hostApi.terminalWrite(session.id, data);
                terminal.current?.focus();
              }}
            >
              {label}
            </button>
          ))}
        </div>
      )}
    </section>
  );
}

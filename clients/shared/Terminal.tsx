import { useHostAPI } from "./host";
import { useEffect, useRef, useState } from "react";
import { Terminal as XTerm } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import { SearchAddon } from "@xterm/addon-search";
import { WebLinksAddon } from "@xterm/addon-web-links";
import "@xterm/xterm/css/xterm.css";
import type { TerminalSession } from "./types";
import { terminalReaderEvent, terminalScreenReader } from "./a11y";
import { useOpenLink } from "./ui";
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
  const open = useOpenLink();
  const openLink = useRef(open);
  openLink.current = open;
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
      // Renders output into an accessible buffer for VoiceOver, Orca and TalkBack.
      screenReaderMode: terminalScreenReader(),
      allowProposedApi: false,
      // A program sets an OSC 8 hyperlink's text and its address separately: when they
      // differ, the address is shown for confirmation before it opens.
      linkHandler: {
        activate: (_event, uri, range) => {
          const buffer = term.buffer.active;
          let text = "";
          for (let y = range.start.y; y <= range.end.y; y++)
            text +=
              buffer
                .getLine(y - 1)
                ?.translateToString(
                  true,
                  y === range.start.y ? range.start.x - 1 : 0,
                  y === range.end.y ? range.end.x : undefined,
                ) || "";
          openLink.current(uri, text, true);
        },
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
      new WebLinksAddon((_event, url) => openLink.current(url, url)),
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
    const applyTypography = () => {
      const size =
        parseFloat(
          getComputedStyle(document.documentElement).getPropertyValue(
            "--code-size",
          ),
        ) || 13;
      if (term.options.fontSize !== size) {
        term.options.fontSize = size;
        resize.current();
      }
    };
    let typographyTimer: ReturnType<typeof setTimeout> | undefined;
    const updateTypography = () => {
      clearTimeout(typographyTimer);
      typographyTimer = setTimeout(applyTypography, 100);
    };
    updateTheme();
    applyTypography();
    const updateReader = () => {
      term.options.screenReaderMode = terminalScreenReader();
    };
    window.addEventListener(terminalReaderEvent, updateReader);
    window.addEventListener("twr:typography", updateTypography);
    const themeObserver = new MutationObserver(updateTheme);
    themeObserver.observe(document.documentElement, { attributes: true });
    const scheme = matchMedia("(prefers-color-scheme: light)");
    scheme.addEventListener("change", updateTheme);
    terminal.current = term;
    search.current = finder;
    let ready = false;
    let sequence = 0;
    let pendingExit: number | undefined;
    let exitShown = false;
    function showExit(exited = false) {
      if (exitShown || (!exited && pendingExit === undefined)) return;
      const suffix = pendingExit === undefined ? "." : ` (${pendingExit}).`;
      term.write(`\r\n\x1b[90mProcess exited${suffix}\x1b[0m\r\n`);
      exitShown = true;
    }
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
      if (event.type === "terminal-exit") {
        pendingExit = event.exitCode ?? 0;
        if (ready) showExit();
      }
    });
    let snapshotLoading = false;
    function snapshot(reset = false) {
      if (snapshotLoading) return;
      ready = false;
      snapshotLoading = true;
      if (reset) {
        term.reset();
        exitShown = false;
      }
      void hostApi
        .terminalRead(session.id)
        .then((snapshot) => {
          if (terminal.current !== term) return;
          term.write(snapshot.data);
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
          showExit(snapshot.exited);
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
          showExit(!!session.exited);
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
      window.removeEventListener("twr:typography", updateTypography);
      window.removeEventListener(terminalReaderEvent, updateReader);
      clearTimeout(typographyTimer);
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
        <span aria-hidden="true">
          {session.workspace} / {session.title}
        </span>
        <input
          aria-label={`Search terminal ${session.title}`}
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
        role="group"
        aria-label={`Terminal ${session.title}${session.exited ? " (exited)" : ""}`}
        ref={node}
        onFocus={() => hostApi.terminalFocus(true)}
        onBlur={() => hostApi.terminalFocus(false)}
      />
      {document.documentElement.dataset.mobile === "true" && (
        <div
          className="terminal-accessory"
          role="group"
          aria-label="Terminal keys"
        >
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

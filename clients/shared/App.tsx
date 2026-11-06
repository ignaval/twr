import { errorMessage } from "./errors";
import { savedAppearance } from "./appearance";
import { useHostAPI } from "./host";
import {
  useCallback,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  lazy,
  Suspense,
  type CSSProperties,
  type FormEvent,
  type ReactNode,
} from "react";
import { useVirtualizer } from "@tanstack/react-virtual";
import {
  Asterisk,
  Wrench,
  Database,
  ListTodo,
  ArrowUp,
  ArrowLeft,
  ArrowDown,
  ArrowUpRight,
  Plus,
  PanelLeftClose,
  PanelLeftOpen,
  LayoutGrid,
  MessageSquare,
  Bell,
  Search,
  Settings2,
  TerminalSquare,
  X,
  ChevronDown,
  ChevronRight,
  MoreHorizontal,
  GitBranch,
  Play,
  Square,
  Pin,
  Server,
  KeyRound,
  Activity,
  FileText,
  Trash2,
  Volume2,
  Command as CommandIcon,
  CheckCircle2,
  Circle,
  Copy,
  ExternalLink,
  Download,
  Paperclip,
  Eye,
  EyeOff,
  Filter,
  AlertCircle,
} from "lucide-react";
import {
  commands as applicationCommands,
  parameters,
  commandArguments,
  type Command,
} from "./commands";
import type {
  Attention,
  DesktopEvent,
  HubEvent,
  Json,
  TerminalSession,
  Thread,
  ViewState,
} from "./types";
import { DataView, Dialog, Empty, FieldInput, Markdown, Spinner } from "./ui";
import { AssistanceNotice, useAssistance, type Summary } from "./assistance";
import { blocksOf, mergeEvents, foldRoutineBlocks } from "./transcript";
import { Manager, JobLog, type Section } from "./Managers";
import { useEditorSessions } from "./editors";
import { localWorkingDirectory } from "./paths";
import { approvalShortcut, canRespond } from "./attention";
const Terminal = lazy(() =>
  import("./Terminal").then((module) => ({ default: module.Terminal })),
);

const initial: ViewState = {
  threads: [],
  attention: [],
  decisions: [],
  decisions_seen: 0,
  head: 0,
};
const nav: { id: Section; label: string; icon: typeof Activity }[] = [
  { id: "accounts", label: "Accounts", icon: KeyRound },
  { id: "boxes", label: "Boxes", icon: Server },
  { id: "jobs", label: "Jobs", icon: ListTodo },
  { id: "usage", label: "Usage", icon: Activity },
  { id: "memory", label: "Context & memory", icon: FileText },
  { id: "speech", label: "Read aloud", icon: Volume2 },
  { id: "trash", label: "Trash", icon: Trash2 },
  { id: "maintenance", label: "Hub & storage", icon: Database },
  { id: "settings", label: "Settings", icon: Settings2 },
];
const statusIcon = (status: string) =>
  status === "running" || status === "background" ? (
    <span className="working-dot" />
  ) : status === "waiting" ? (
    <AlertCircle size={12} />
  ) : status === "done" ? (
    <CheckCircle2 size={12} />
  ) : (
    <Circle size={10} />
  );

function operationAction(command: Command, again: boolean) {
  const labels: Record<string, string> = {
    new: "Create thread",
    delete: "Delete thread",
    restore: "Restore",
    rename: "Rename",
    mode: "Change mode",
    model: "Change model",
    park: "Park thread",
    resume: "Resume thread",
    stop: "Stop thread",
  };
  const label = labels[command.id] || command.title.replace(/…$/, "");
  return again ? `${label} again` : label;
}
function MessageBody({
  text,
  mono = false,
  clamp = true,
}: {
  text: string;
  mono?: boolean;
  clamp?: boolean;
}) {
  const [expanded, setExpanded] = useState(false);
  const lines = text.split("\n").length;
  const long = clamp && (lines > 16 || text.length > 1600);
  return (
    <>
      <div className={`message-body ${long && !expanded ? "clamped" : ""}`}>
        {mono ? <pre>{text}</pre> : <Markdown text={text} />}
      </div>
      {long && (
        <button
          className="message-expand"
          aria-expanded={expanded}
          onClick={() => setExpanded((v) => !v)}
        >
          {expanded
            ? "Show less"
            : lines > 16
              ? `Show ${lines - 12} more lines`
              : "Show full message"}
        </button>
      )}
    </>
  );
}

function applyOmarchyTheme(theme: Record<string, string> | null | undefined) {
  const root = document.documentElement;
  root.dataset.omarchy = theme ? "true" : "false";
  if (!theme) return;
  for (const [key, value] of Object.entries({
    bg: theme.background,
    fg: theme.foreground,
    accent: theme.accent,
    warning: theme.yellow || theme.color3,
    danger: theme.red || theme.color1,
    selection: theme.selection || theme.selection_background,
  }))
    root.style.setProperty("--omarchy-" + key, value || "");
  const rgb = (value?: string) => {
    if (!value || !/^#[0-9a-f]{6}$/i.test(value)) return undefined;
    return [1, 3, 5].map((i) => parseInt(value.slice(i, i + 2), 16));
  };
  const background = rgb(theme.background);
  const scheme =
    theme.mode === "light" || theme.mode === "dark"
      ? theme.mode
      : background &&
          (background[0] * 299 + background[1] * 587 + background[2] * 114) /
            1000 >
            140
        ? "light"
        : "dark";
  root.style.setProperty("--omarchy-scheme", scheme);
  const accent = rgb(theme.accent);
  if (accent) {
    const luminance = accent
      .map((channel) => channel / 255)
      .map((channel) =>
        channel <= 0.04045
          ? channel / 12.92
          : ((channel + 0.055) / 1.055) ** 2.4,
      )
      .reduce((sum, value, i) => sum + value * [0.2126, 0.7152, 0.0722][i], 0);
    root.style.setProperty(
      "--omarchy-accent-fg",
      luminance > 0.179 ? "#06191c" : "#ffffff",
    );
  }
}

export default function App({
  deviceScopes,
  onReady,
  headerAccessory,
  onDismissKeyboard,
}: {
  deviceScopes?: string[];
  onReady?: () => void;
  headerAccessory?: ReactNode;
  onDismissKeyboard?: () => void;
} = {}) {
  const hostApi = useHostAPI();
  const [allowedMethods, setAllowedMethods] = useState<string[]>();
  const permitted = (scope: string) =>
    !deviceScopes || deviceScopes.includes(scope);
  const commands = useMemo(
    () =>
      applicationCommands.filter(
        (command) =>
          !deviceScopes ||
          (!command.method
            ? permitted("terminal")
            : !!allowedMethods?.includes(command.method)),
      ),
    [deviceScopes, allowedMethods],
  );
  const visibleNav = nav.filter(
    (n) =>
      !(
        ["accounts", "boxes", "settings", "maintenance"].includes(n.id) &&
        !permitted("admin")
      ),
  );

  const [view, setView] = useState(initial);
  const [workspace, setWorkspace] = useState("default");
  const [homePath, setHomePath] = useState("");
  const [workspaces, setWorkspaces] = useState<Json[]>([]);
  const [paths, setPaths] = useState<Json>();
  const [info, setInfo] = useState<Json>();
  const [connected, setConnected] = useState(false);
  const [connecting, setConnecting] = useState(true);
  const [connectionError, setConnectionError] = useState("");
  const [error, setError] = useState("");
  const [section, setSection] = useState<Section>("chat");
  const [sidebar, setSidebar] = useState(() => window.innerWidth > 800);
  const [hovering, setHovering] = useState(false);
  const [focus, setFocus] = useState(false);
  const [parked, setParked] = useState(false);
  const [pins, setPins] = useState<string[]>(() =>
    JSON.parse(localStorage.getItem("twr.pins") || "[]"),
  );
  const [selected, setSelected] = useState<string>();
  const [destination, setDestination] = useState<string>();
  const observedDestination = useRef<string | undefined>(undefined);
  const mobileDraftKey = useRef(
    `twr.mobile.drafts.${document.documentElement.dataset.hub || ""}`,
  ).current;
  const [drafts, setDrafts] = useState<Record<string, string>>(() => {
    if (document.documentElement.dataset.mobile !== "true") return {};
    try {
      const value = JSON.parse(localStorage.getItem(mobileDraftKey) || "{}");
      return value && typeof value === "object" && !Array.isArray(value)
        ? Object.fromEntries(
            Object.entries(value).filter(
              (entry): entry is [string, string] =>
                typeof entry[1] === "string",
            ),
          )
        : {};
    } catch {
      return {};
    }
  });
  useEffect(() => {
    if (document.documentElement.dataset.mobile === "true") {
      try {
        localStorage.setItem(mobileDraftKey, JSON.stringify(drafts));
      } catch {}
    }
  }, [drafts, mobileDraftKey]);
  const [sending, setSending] = useState(false);
  const slashPending = useRef(false);
  const [queued, setQueued] = useState<Json[]>([]);
  const [queueInfo, setQueueInfo] = useState<Json>({});
  const [shellCwd, setShellCwd] = useState<string>();
  const [shellJobs, setShellJobs] = useState<Json[]>([]);
  const [slashHidden, setSlashHidden] = useState(false);
  const [mentionIndex, setMentionIndex] = useState(0);
  const [mentionHidden, setMentionHidden] = useState(false);
  const [palette, setPalette] = useState(false);
  const [query, setQuery] = useState("");
  const [paletteIndex, setPaletteIndex] = useState(0);
  const [cliCommands, setCliCommands] = useState<Command[]>([]);
  const [slashCommands, setSlashCommands] = useState<
    { id: string; title: string }[]
  >([]);
  useEffect(() => {
    if (connected)
      void hostApi
        .rpc("desktop.slash_commands")
        .then(setSlashCommands)
        .catch(() => {});
  }, [connected]);
  const [operation, setOperation] = useState<Command>();
  const [values, setValues] = useState<Record<string, any>>({});
  const [operationBusy, setOperationBusy] = useState(false);
  const [operationError, setOperationError] = useState("");
  const [result, setResult] = useState<Json>();
  const [refresh, setRefresh] = useState(0);
  const [transcriptRefresh, setTranscriptRefresh] = useState(0);
  const [queueRefresh, setQueueRefresh] = useState(0);
  const [terminals, setTerminals] = useState<TerminalSession[]>([]);
  const [activeTerminal, setActiveTerminal] = useState<string>();
  const [terminalVisible, setTerminalVisible] = useState(true);
  const [confirmation, setConfirmation] = useState<{
    title: string;
    detail: string;
    run: () => void;
  }>();
  const [workspaceModal, setWorkspaceModal] = useState(false);
  const [newWorkspace, setNewWorkspace] = useState("");
  const [showTools, setShowTools] = useState(false);
  const [showThinking, setShowThinking] = useState(false);
  const [events, setEvents] = useState<HubEvent[]>([]);
  const [transcriptLoading, setTranscriptLoading] = useState(false);
  const [transcriptError, setTranscriptError] = useState("");
  const [earliestTurn, setEarliestTurn] = useState<number>();
  const [earlierBusy, setEarlierBusy] = useState(false);
  const connectGeneration = useRef(0);
  const transcriptGeneration = useRef(0);
  const activeThread = useRef<string | undefined>(undefined);
  const activeSubscription = useRef("");
  const pendingEvents = useRef<HubEvent[]>([]);
  const eventTimer = useRef<ReturnType<typeof setTimeout> | undefined>(
    undefined,
  );
  const transcriptRetry = useRef<ReturnType<typeof setTimeout> | undefined>(
    undefined,
  );
  const connectionRef = useRef(false);
  const input = useRef<HTMLTextAreaElement>(null);
  const fail = useCallback((e: unknown) => setError(errorMessage(e)), []);
  const rpc = useCallback(
    (method: string, params: Json = {}) => hostApi.rpc(method, params),
    [],
  );
  const orchestrator = view.threads.find((t) => t.is_orchestrator);
  const target = destination
    ? view.threads.find((t) => t.id === destination)
    : orchestrator;
  const draftKey = workspace + ":" + (destination || "orchestrator");
  const draft = drafts[draftKey] || "";
  const selectedThread = view.threads.find((t) => t.id === selected);
  const [visibleRoutines, setVisibleRoutines] = useState<string[]>([]);
  const presentedEvents = useMemo(() => {
    const resolved = new Set(view.went_on || []);
    return events.map((event) =>
      event.stall && resolved.has(event.stall)
        ? { ...event, routine: true }
        : event,
    );
  }, [events, view.went_on]);
  const assistance = useAssistance({
    workspace,
    connected: connected && permitted("operate"),
    visible: section === "chat",
    target,
    events: presentedEvents,
    draft,
    refresh,
    visibleRoutines,
    threads: view.threads,
  });
  const [acceptedSuggestion, setAcceptedSuggestion] = useState<{
    key: string;
    text: string;
  }>();
  function acceptSuggestion() {
    if (!assistance.suggestion) return;
    setDrafts((old) => ({ ...old, [draftKey]: assistance.suggestion }));
    setAcceptedSuggestion({ key: draftKey, text: assistance.suggestion });
    input.current?.focus();
  }
  async function setAssistance(key: "suggest" | "summaries", value: boolean) {
    try {
      await hostApi.configSet(["tui"], key, value);
      setRefresh((n) => n + 1);
    } catch (error) {
      fail(error);
    }
  }
  const openOperation = useCallback(
    (command: Command, initialValues: Json = {}) => {
      setPalette(false);
      setOperation(command);
      setValues({
        _cwd: initialValues._cwd || localWorkingDirectory(target, homePath),
        ...Object.fromEntries(
          command.fields.map((f) => [
            f.key,
            (f.type === "words" && Array.isArray(initialValues[f.key])
              ? initialValues[f.key]
                  .map((word: string) => JSON.stringify(word))
                  .join(" ")
              : initialValues[f.key]) ??
              (command.id === "delete" && f.key === "threads"
                ? initialValues.thread
                : undefined) ??
              f.initial ??
              (f.type === "boolean" ? false : ""),
          ]),
        ),
      });
      setOperationError("");
      setResult(undefined);
    },
    [target?.worktree, target?.repo, target?.box_name, homePath],
  );
  const action = useCallback(
    (id: string, initialValues: Json = {}) => {
      const command = commands.find((c) => c.id === id);
      if (command) openOperation(command, initialValues);
    },
    [openOperation, commands],
  );
  const actionRef = useRef(action);
  actionRef.current = action;
  const cliAction = useCallback(
    (path: string, initialValues: Json = {}) => {
      const command = cliCommands.find((c) => c.cli?.join(" ") === path);
      if (command) openOperation(command, initialValues);
      else
        void hostApi
          .catalog()
          .then((list) => {
            setCliCommands(list);
            const found = list.find((c) => c.cli?.join(" ") === path);
            if (found) openOperation(found, initialValues);
            else fail("This workflow is not available in the bundled CLI.");
          })
          .catch(fail);
    },
    [cliCommands, openOperation, fail],
  );
  const refreshInfo = useCallback(async () => {
    const value = await rpc("hub.info");
    setInfo(value);
  }, [rpc]);
  const refreshWorkspaces = useCallback(async () => {
    const value = await hostApi.cli(["ws", "list", "--json"]);
    if (value.code !== 0) throw new Error(value.stderr || value.stdout);
    const parsed = JSON.parse(value.stdout);
    setWorkspaces(Array.isArray(parsed) ? parsed : parsed.workspaces || []);
  }, []);
  const openTerminal = useCallback(
    async (
      request: Parameters<typeof hostApi.terminalOpen>[0],
      throwOnFailure = false,
    ) => {
      try {
        const session = await hostApi.terminalOpen(request);
        setTerminals((old) => [...old, session]);
        setActiveTerminal(session.id);
        setTerminalVisible(true);
        return session;
      } catch (e) {
        if (throwOnFailure) throw e;
        fail(e);
      }
    },
    [fail],
  );
  const connect = useCallback(
    async (name: string) => {
      const generation = ++connectGeneration.current;
      transcriptGeneration.current++;
      activeThread.current = undefined;
      connectionRef.current = false;
      setWorkspace(name);
      setView(initial);
      setConnected(false);
      setConnecting(true);
      setConnectionError("");
      setDestination(undefined);
      setSelected(undefined);
      setEvents([]);
      setInfo(undefined);
      setQueued([]);
      setPaths(undefined);
      setShellCwd(undefined);
      setShellJobs([]);
      setSection("chat");
      if (window.innerWidth <= 800) setSidebar(false);
      try {
        const value = await hostApi.connect(name);
        if (generation !== connectGeneration.current) return;
        setPaths(value);
        await refreshInfo();
        await refreshWorkspaces();
        setRefresh((n) => n + 1);
      } catch (e) {
        if (generation === connectGeneration.current)
          setConnectionError(e instanceof Error ? e.message : String(e));
      } finally {
        if (generation === connectGeneration.current) setConnecting(false);
      }
    },
    [refreshInfo, refreshWorkspaces],
  );
  useEffect(() => {
    const open = (event: Event) => {
      const next = (event as CustomEvent<{ workspace?: string }>).detail
        ?.workspace;
      const show = () => {
        setSection("attention");
        setSidebar(false);
      };
      if (next && next !== workspace) void connect(next).then(show);
      else show();
    };
    window.addEventListener("twr:open-attention", open);
    return () => window.removeEventListener("twr:open-attention", open);
  }, [connect, workspace]);
  useEffect(() => {
    let refreshingTerminals = false;
    const api = hostApi;
    const refreshTerminals = () => {
      if (refreshingTerminals) return;
      refreshingTerminals = true;
      void api
        .bootstrap()
        .then((boot) => {
          const sessions = boot.terminals || [];
          setTerminals(sessions);
          setActiveTerminal((id) =>
            sessions.some((session) => session.id === id)
              ? id
              : sessions.at(-1)?.id,
          );
        })
        .catch(() => {})
        .finally(() => {
          refreshingTerminals = false;
        });
    };
    const unsubscribe = hostApi.onEvent((event) => {
      if (event.type === "resync" && event.scope !== "control")
        refreshTerminals();
      if (event.type === "resync" && event.scope !== "terminal")
        setTranscriptRefresh((n) => n + 1);
      if (event.type === "state" && event.state) setView(event.state);
      if (event.type === "connection") {
        const restored = event.connected && !connectionRef.current;
        connectionRef.current = !!event.connected;
        setConnected(!!event.connected);
        setConnectionError(event.error || "");
        if (restored) {
          refreshTerminals();
          setRefresh((n) => n + 1);
          setTranscriptRefresh((n) => n + 1);
        }
      }
      if (event.type === "theme") applyOmarchyTheme(event.theme);
      if (event.type === "error")
        fail(event.error || "An unexpected desktop error occurred");
      if (
        event.type === "thread" &&
        event.subscription === activeSubscription.current &&
        event.event &&
        event.event.thread === activeThread.current
      ) {
        pendingEvents.current.push(event.event);
        if (!eventTimer.current) {
          const subscription = event.subscription;
          eventTimer.current = setTimeout(() => {
            eventTimer.current = undefined;
            const batch = pendingEvents.current.splice(0);
            if (subscription === activeSubscription.current)
              setEvents((old) => mergeEvents(old, batch));
          }, 40);
        }
      }
      if (
        event.type === "transcript-error" &&
        event.subscription === activeSubscription.current &&
        event.thread === activeThread.current
      ) {
        setTranscriptError(event.error || "Transcript disconnected");
        if (!transcriptRetry.current)
          transcriptRetry.current = setTimeout(() => {
            transcriptRetry.current = undefined;
            setTranscriptRefresh((n) => n + 1);
          }, 2000);
      }
      if (event.type === "config-error") {
        setConnectionError(event.error || "Configuration needs repair");
        setSection("settings");
      }
      if (event.type === "terminal-exit")
        setTerminals((old) =>
          old.map((t) => (t.id === event.id ? { ...t, exited: true } : t)),
        );
      if (event.type === "terminal-removed")
        setTerminals((old) => old.filter((t) => t.id !== event.id));
      if (event.type === "menu-new") actionRef.current("new");
      if (event.type === "menu-palette") {
        setPalette(true);
        setQuery("");
      }
    });
    void hostApi
      .bootstrap()
      .then((boot) => {
        setAllowedMethods(boot.allowed_methods);
        document.documentElement.dataset.platform = boot.platform;
        setTerminals(boot.terminals || []);
        setActiveTerminal(boot.terminals?.at(-1)?.id);
        document.documentElement.dataset.theme = savedAppearance();
        applyOmarchyTheme(boot.theme);
        setHomePath(boot.home);
        return connect(boot.workspace).then(() => onReady?.());
      })
      .catch(fail);
    void hostApi.catalog().then(setCliCommands).catch(fail);
    return () => {
      unsubscribe();
      if (eventTimer.current) clearTimeout(eventTimer.current);
      eventTimer.current = undefined;
      pendingEvents.current = [];
    };
  }, []);
  useEffect(() => {
    const media = window.matchMedia("(max-width:800px)");
    const change = () => {
      if (media.matches) setSidebar(false);
    };
    media.addEventListener("change", change);
    return () => media.removeEventListener("change", change);
  }, []);
  useEffect(() => {
    if (!connected) return;
    const timer = setInterval(() => {
      void refreshInfo().catch(() => {});
      void refreshWorkspaces().catch(() => {});
    }, 10000);
    return () => clearInterval(timer);
  }, [connected, refreshInfo, refreshWorkspaces]);
  useEffect(() => {
    const id = target?.id;
    activeThread.current = id;
    const generation = ++transcriptGeneration.current;
    setEvents([]);
    setTranscriptError("");
    setEarliestTurn(undefined);
    activeSubscription.current = "";
    if (eventTimer.current) clearTimeout(eventTimer.current);
    eventTimer.current = undefined;
    pendingEvents.current = [];
    if (!id || !connected) {
      setTranscriptLoading(false);
      return;
    }
    setTranscriptLoading(true);
    void (async () => {
      let page: Json;
      try {
        page = await rpc("thread.transcript", { thread: id, limit: 500 });
      } catch (error) {
        // A newly created worker has no turn yet. Subscribe from zero rather than
        // leaving its first message without a live transcript connection.
        if (!/turn not found/i.test(String(error))) throw error;
        page = await rpc("thread.transcript", {
          thread: id,
          since_seq: 0,
          limit: 500,
        });
      }
      const all: HubEvent[] = [...page.events];
      let firstTurn = page.turn as number | undefined;
      while (page.more) {
        page = await rpc("thread.transcript", {
          thread: id,
          turn: page.turn,
          since_seq: page.next,
          limit: 500,
        });
        if (generation !== transcriptGeneration.current) return;
        all.push(...page.events);
      }
      const cursor = page.next || 0;
      // Give short conversations useful context immediately. Older turns remain
      // paged, and the virtual list asks for more when the user reaches the top.
      for (
        let n = 0;
        n < 3 &&
        firstTurn &&
        firstTurn > 1 &&
        blocksOf(all).reduce((sum, b) => sum + b.text.length, 0) < 3000;
        n++
      ) {
        let older = await rpc("thread.transcript", {
          thread: id,
          turn: firstTurn - 1,
          limit: 500,
        });
        const batch: HubEvent[] = [...older.events];
        while (older.more) {
          older = await rpc("thread.transcript", {
            thread: id,
            turn: firstTurn - 1,
            since_seq: older.next,
            limit: 500,
          });
          batch.push(...older.events);
        }
        all.unshift(...batch);
        firstTurn--;
        if (generation !== transcriptGeneration.current) return;
      }
      if (generation !== transcriptGeneration.current) return;
      setEarliestTurn(firstTurn);
      setEvents(all);
      const subscription = String(generation);
      activeSubscription.current = subscription;
      await rpc("desktop.subscribe", {
        thread: id,
        since: cursor,
        subscription,
      });
    })()
      .catch((e) => {
        if (generation === transcriptGeneration.current)
          setTranscriptError(String(e));
      })
      .finally(() => {
        if (generation === transcriptGeneration.current)
          setTranscriptLoading(false);
      });
  }, [target?.id, connected, transcriptRefresh, rpc]);
  useEffect(() => {
    if (!target || !connected) {
      setQueued([]);
      return;
    }
    let live = true;
    const load = () =>
      void rpc("thread.queue", { thread: target.id })
        .then((value) => {
          if (live) {
            setQueued(
              Array.isArray(value)
                ? value
                : value.items || value.messages || value.queue || [],
            );
            setQueueInfo(value);
          }
        })
        .catch(() => {});
    load();
    const timer = setInterval(load, 2500);
    return () => {
      live = false;
      clearInterval(timer);
    };
  }, [target?.id, connected, queueRefresh, rpc]);
  useEffect(() => {
    if (destination && view.threads.some((t) => t.id === destination)) {
      observedDestination.current = destination;
      return;
    }
    if (
      destination &&
      observedDestination.current === destination &&
      !view.threads.some((t) => t.id === destination) &&
      connected &&
      !connecting
    )
      setDestination(undefined);
  }, [destination, view.threads, connected, connecting]);
  useEffect(() => {
    const keyboard = (event: KeyboardEvent) => {
      if (event.defaultPrevented || document.querySelector("[role=dialog]"))
        return;
      const mac = ["darwin", "ios"].includes(
        document.documentElement.dataset.platform || "",
      );
      const menu = document.querySelector<HTMLDetailsElement>(
        ".conversation-menu[open]",
      );
      if (event.key === "Escape" && menu) {
        event.preventDefault();
        menu.removeAttribute("open");
        menu.querySelector<HTMLElement>("summary")?.focus();
        return;
      }
      const mod = mac ? event.metaKey : event.ctrlKey;
      const inTerminal = (event.target as HTMLElement)?.closest(
        ".terminal-surface",
      );
      if (inTerminal) return;
      if (mod && event.key.toLowerCase() === "n") {
        event.preventDefault();
        actionRef.current("new");
        return;
      }
      if (mod && event.key.toLowerCase() === "k") {
        event.preventDefault();
        setQuery("");
        setPalette(true);
        return;
      }
      if (mod && event.key.toLowerCase() === "b") {
        event.preventDefault();
        setSidebar((v) => !v);
      }
      if (mod && event.key.toLowerCase() === "g") {
        event.preventDefault();
        setSection((v) => (v === "grid" ? "chat" : "grid"));
      }
      if (
        mod &&
        (!mac || event.shiftKey) &&
        event.key.toLowerCase() === "f" &&
        !(event.target instanceof HTMLInputElement) &&
        !(event.target instanceof HTMLTextAreaElement)
      ) {
        event.preventDefault();
        setFocus((v) => !v);
      }
      if (mod && event.key === "," && permitted("admin")) {
        event.preventDefault();
        setSection("settings");
      }
      if (event.key === "Escape") {
        if (event.target instanceof HTMLTextAreaElement && event.target.value)
          return;
        if (event.target instanceof HTMLInputElement) return;
        if (selected) setSelected(undefined);
        else if (destination) setDestination(undefined);
        input.current?.focus();
      }
    };
    const closeMenu = (event: PointerEvent) => {
      if (
        event.target instanceof Element &&
        event.target.closest(".conversation-menu")
      )
        return;
      document
        .querySelector(".conversation-menu[open]")
        ?.removeAttribute("open");
    };
    window.addEventListener("keydown", keyboard);
    window.addEventListener("pointerdown", closeMenu);
    return () => {
      window.removeEventListener("keydown", keyboard);
      window.removeEventListener("pointerdown", closeMenu);
    };
  }, [selected, destination]);
  useEffect(() => {
    if (connected)
      void rpc("desktop.preferences", {
        pins,
        held: !!selected || hovering,
      }).catch(() => {});
  }, [pins, selected, hovering, connected, rpc]);
  const execute = async () => {
    if (!operation) return;
    const run = async () => {
      setOperationBusy(true);
      setOperationError("");
      try {
        if (operation.cli) {
          if (!values._cwd)
            throw new Error("Choose a working directory for this command.");
          const opened = await openTerminal(
            {
              args: commandArguments(operation, values),
              cwd: values._cwd || undefined,
              title: operation.title,
            },
            true,
          );
          if (!opened)
            throw new Error(
              "Could not open the terminal. Your form has been kept.",
            );
          setOperation(undefined);
        } else {
          const params = parameters(operation, values);
          if (operation.method === "thread.new") {
            if (!values._cwd)
              throw new Error(
                "Choose a working directory to resolve project paths.",
              );
            params.desktop_cwd = values._cwd;
          }
          const value = await rpc(operation.method!, params);
          setResult(value);
          setRefresh((n) => n + 1);
          if (operation.id === "new") {
            setOperation(undefined);
            setDestination(value.id);
            setSelected(undefined);
            setSection("chat");
          } else if (operation.id === "delete") {
            setOperation(undefined);
            setSection("attention");
          }
        }
      } catch (e) {
        setOperationError(errorMessage(e));
      } finally {
        setOperationBusy(false);
      }
    };
    if (operation.confirm)
      setConfirmation({
        title: operation.title,
        detail: operation.confirm,
        run: () => void run(),
      });
    else await run();
  };
  const send = async (event?: FormEvent) => {
    event?.preventDefault();
    if (!permitted("operate") || !draft.trim() || sending || !connected) return;
    if (draft.startsWith("/") && !draft.startsWith("//")) {
      await submitSlash(draft);
      return;
    }
    if (!target) {
      fail(
        "Choose an orchestrator or open a worker thread before sending a message.",
      );
      return;
    }
    setSending(true);
    const key = draftKey;
    const text = draft.startsWith("//") ? draft.slice(1) : draft;
    try {
      let response: Json;
      if (text.startsWith("!") && !text.startsWith("!!!")) {
        const share = text.startsWith("!!");
        const command = text.slice(share ? 2 : 1).trim();
        if (!command)
          throw new Error(
            "Enter a shell command after !, or use !! to share its result.",
          );
        if (/^cd(?:\s|$)/.test(command)) {
          if (destination)
            throw new Error(
              "This thread runs in its worktree. Use its embedded terminal to change directories.",
            );
          response = await rpc("shell.cd", {
            cwd: shellCwd,
            dir: command.slice(2).trim() || "~",
          });
          setShellCwd(response.cwd);
        } else {
          response = await rpc("shell.run", {
            command,
            share,
            ...(destination
              ? { thread: destination }
              : shellCwd
                ? { cwd: shellCwd }
                : {}),
          });
          setShellJobs((old) =>
            [...old, { job: response.job, command }].slice(-5),
          );
        }
      } else
        response = destination
          ? await rpc("thread.send", { thread: destination, prompt: text })
          : await rpc("say", {
              message: text,
              seen_gseq: view.head,
              to: "orchestrator",
              suggested:
                acceptedSuggestion?.key === key &&
                acceptedSuggestion.text === text,
            });
      if (response.action === "pending") {
        setSection("attention");
        throw new Error(
          "Message not sent. Review the waiting requests and answer the exact item shown.",
        );
      }
      setDrafts((old) => ({ ...old, [key]: "" }));
      setQueueRefresh((n) => n + 1);
    } catch (e) {
      fail(e);
    } finally {
      setSending(false);
      input.current?.focus();
    }
  };
  async function submitSlash(text: string) {
    if (slashPending.current) return;
    slashPending.current = true;
    setSending(true);
    try {
      const rawName = text.slice(1).trim().split(/\s/)[0];
      const extraRoutes: Record<string, Section> = {
        boxes: "boxes",
        jobs: "jobs",
        usage: "usage",
        memory: "memory",
        maintenance: "maintenance",
      };
      if (extraRoutes[rawName]) {
        setSection(extraRoutes[rawName]);
        setDrafts((old) => ({ ...old, [draftKey]: "" }));
        return;
      }
      const parsed = await rpc("desktop.parse_slash", { line: text });
      const args = parsed.args;
      const chosen = destination || selected;
      const named = (name: string | undefined) =>
        view.threads.find((thread) =>
          [thread.id, thread.name, ...(thread.aliases || [])].includes(name),
        );
      const thread = (name?: string) => name || chosen;
      const toggle = (
        value: boolean | null,
        update: (fn: (old: boolean) => boolean) => void,
      ) => update((old) => value ?? !old);
      switch (parsed.kind) {
        case "Back":
          setDestination(undefined);
          setSection("chat");
          break;
        case "Unsend": {
          const message = queued.findLast((message) => message.mine);
          const last = events.findLast(
            (event) =>
              event.kind === "turn.started" &&
              event.data.from === "user" &&
              event.data.ext?.twr?.ref != null,
          );
          const ref = message?.ref ?? last?.data.ext?.twr?.ref;
          if (ref == null)
            throw new Error(
              "No message can be taken back in this conversation.",
            );
          setDrafts((old) => ({ ...old, [draftKey]: "" }));
          await takeBack({ ref });
          return;
        }
        case "Open": {
          const target = named(args || chosen);
          if (!target)
            throw new Error("Select a thread or name it after /open.");
          setDestination(target.id);
          setSelected(undefined);
          setSection("chat");
          break;
        }
        case "Ws": {
          if (!args) setWorkspaceModal(true);
          else if (
            workspaces.some(
              (item) => (typeof item === "string" ? item : item.name) === args,
            )
          )
            await connect(args);
          else {
            setNewWorkspace(args);
            setWorkspaceModal(true);
          }
          break;
        }
        case "WsNew": {
          if (!args) setWorkspaceModal(true);
          else {
            const made = await hostApi.cli(["ws", "new", args]);
            if (made.code) throw new Error(made.stderr || made.stdout);
            await refreshWorkspaces();
            await connect(args);
          }
          break;
        }
        case "AccountAdd":
          cliAction("account add", { runtime: args.runtime, name: args.name });
          break;
        case "Account": {
          let name = args.account,
            target = args.thread;
          if (destination && target && !name && !named(target)) {
            name = target;
            target = destination;
          }
          if (!name)
            openOperation(
              {
                id: "account-show",
                title: "Current thread account",
                description:
                  "Read the thread’s current account without changing it.",
                group: "Accounts",
                method: "thread.get",
                fields: [{ key: "thread", label: "Thread", required: true }],
              },
              { thread: thread(target) },
            );
          else
            action("model", {
              thread: thread(target),
              account: name,
              outside_pin: args.outside_pin,
            });
          break;
        }
        case "Model": {
          const values = { ...args, thread: thread(args.thread) };
          if (!args.model && !args.effort && !args.runtime)
            openOperation(
              {
                id: "model-show",
                title: "Current model, runtime and account",
                description:
                  "Read the thread’s current settings without changing them.",
                group: "Threads",
                method: "thread.get",
                fields: [{ key: "thread", label: "Thread", required: true }],
              },
              values,
            );
          else action("model", values);
          break;
        }
        case "Mode":
          action("mode", { ...args, thread: thread(args.thread) });
          break;
        case "Rename":
          action("rename", { ...args, thread: thread(args.thread) });
          break;
        case "New":
          action("new", args);
          break;
        case "Delete":
          action("delete", { threads: args });
          break;
        case "Orch":
          action("orch", args);
          break;
        case "OrchPick":
          action("orch");
          break;
        case "Clear":
          action("clear");
          break;
        case "Park":
        case "Resume":
        case "Trim":
        case "Stop":
        case "CompactContext":
          action(
            (
              {
                Park: "park",
                Resume: "resume",
                Trim: "trim",
                Stop: "stop",
                CompactContext: "compact",
              } as Record<string, string>
            )[parsed.kind],
            { thread: thread(args) },
          );
          break;
        case "Attach": {
          const target = thread(args);
          if (!target) throw new Error("Choose a thread to attach.");
          await openTerminal({
            args: ["attach", target],
            title: "Attach " + target,
          });
          break;
        }
        case "Pin": {
          const target = named(thread(args));
          if (!target) throw new Error("Choose a thread to pin.");
          togglePin(target.id);
          break;
        }
        case "Parked":
          toggle(args, setParked);
          break;
        case "Focus":
          toggle(args, setFocus);
          break;
        case "Sidebar":
        case "Spaces":
          toggle(args, setSidebar);
          break;
        case "Compact":
          setSidebar((old) => (args === null ? !old : !args));
          break;
        case "Pane":
          setSection(
            args === "Needs"
              ? "attention"
              : args === "Threads"
                ? "grid"
                : "chat",
          );
          break;
        case "Grid":
          setSection((old) => (old === "grid" ? "chat" : "grid"));
          break;
        case "Decisions":
          setSection((old) => (old === "decisions" ? "chat" : "decisions"));
          break;
        case "Accounts":
          setSection("accounts");
          break;
        case "Settings":
          setSection("settings");
          break;
        case "Suggest":
        case "Summaries": {
          const key = parsed.kind === "Suggest" ? "suggest" : "summaries";
          await setAssistance(key, args ?? !assistance.settings?.[key]);
          break;
        }
        case "Trash": {
          if (args.kind === "List") setSection("trash");
          else if (args.kind === "Restore")
            action("restore", { item: args.args.item, as: args.args.as_name });
          else action("purge", { ...args.args, dry_run: true });
          break;
        }
        case "Read": {
          const cmd = args.cmd,
            speaker = args.on || undefined;
          if (cmd.kind === "Status") setSection("speech");
          else if (cmd.kind === "Last")
            action("read", {
              last: cmd.args.n,
              thread: cmd.args.orch ? orchestrator?.id : destination,
              speaker,
            });
          else if (cmd.kind === "This") {
            const event = events.findLast(
              (event) => event.kind === "agent.text" && event.data.final,
            );
            if (!target || !event)
              throw new Error(
                "There is no completed message in this conversation to read.",
              );
            action("read", {
              thread: target.id,
              seq: event.seq,
              last: "",
              speaker,
            });
          } else
            action("speech-control", {
              op: (
                {
                  On: "live",
                  Off: "live",
                  Stop: "stop",
                  Pause: "pause",
                  Resume: "resume",
                  Skip: "skip",
                  Voice: "voice",
                  Rate: "rate",
                } as Record<string, string>
              )[cmd.kind],
              speaker,
              live: cmd.kind === "On",
              voice: cmd.kind === "Voice" ? cmd.args : "",
              rate: cmd.kind === "Rate" ? cmd.args : "",
            });
          break;
        }
        case "Quit":
          await hostApi.quit();
          break;
        case "Help":
          setQuery("");
          setPalette(true);
          break;
        case "Mouse":
          throw new Error(
            "Desktop text selection uses the operating system. Terminal mouse capture is controlled inside each terminal.",
          );
        default:
          throw new Error("No desktop action for this command.");
      }
      setDrafts((old) => ({ ...old, [draftKey]: "" }));
    } catch (error) {
      fail(error);
    } finally {
      slashPending.current = false;
      setSending(false);
    }
  }
  async function exportConversation() {
    if (!target) return;
    try {
      let page = await rpc("thread.transcript", {
        thread: target.id,
        since_seq: 0,
        limit: 500,
      });
      const all: HubEvent[] = [...page.events];
      while (page.more) {
        page = await rpc("thread.transcript", {
          thread: target.id,
          since_seq: page.next,
          limit: 500,
        });
        all.push(...page.events);
      }
      await hostApi.exportText(
        `${target.name}-conversation.md`,
        blocksOf(all)
          .map((b) => `## ${b.kind} · ${b.ts}\n\n${b.text}\n${b.detail || ""}`)
          .join("\n\n"),
      );
    } catch (error) {
      fail(error);
    }
  }
  async function takeBack(message: Json) {
    try {
      const response = await rpc("message.withdraw", {
        thread: target?.id,
        ref: message.ref,
      });
      if (!response.withdrawn)
        throw new Error(
          response.message || "This message can no longer be taken back.",
        );
      setDrafts((old) => ({
        ...old,
        [draftKey]:
          (old[draftKey] ? old[draftKey] + "\n\n" : "") + response.prompt,
      }));
      setQueueRefresh((n) => n + 1);
      input.current?.focus();
    } catch (error) {
      fail(error);
    }
  }
  async function sendNow(message: Json) {
    try {
      const response = await rpc("message.send_now", {
        thread: target?.id,
        ref: message.ref,
      });
      if (!response.sent_now)
        throw new Error(
          response.message ||
            response.why ||
            response.reason ||
            "The message is still queued.",
        );
      setQueueRefresh((n) => n + 1);
    } catch (error) {
      fail(error);
    }
  }
  const togglePin = (id: string) =>
    setPins((old) => {
      const next = old.includes(id)
        ? old.filter((p) => p !== id)
        : [...old, id];
      localStorage.setItem("twr.pins", JSON.stringify(next));
      return next;
    });
  const workers = view.threads.filter((t) => !t.is_orchestrator);
  const visible = workers.filter(
    (t) =>
      (parked || t.status !== "parked") &&
      (!focus ||
        ["running", "background", "waiting", "error"].includes(t.status) ||
        view.attention.some((a) => a.thread === t.id)),
  );
  const attentionFor = (id: string) =>
    view.attention.filter((a) => a.thread === id);
  const respond = async (item: Attention, answer: string, message?: string) => {
    if (!permitted("operate")) return;
    try {
      await rpc("attention.respond", {
        attn_id: item.attn_id,
        revision: item.revision,
        answer,
        ...(message ? { message } : {}),
      });
    } catch (e) {
      fail(e);
    }
  };
  const earlier = async () => {
    if (!earliestTurn || earliestTurn <= 1 || !target || earlierBusy) return;
    const generation = transcriptGeneration.current;
    setEarlierBusy(true);
    try {
      let page = await rpc("thread.transcript", {
        thread: target.id,
        turn: earliestTurn - 1,
        limit: 500,
      });
      const more: HubEvent[] = [...page.events];
      while (page.more) {
        page = await rpc("thread.transcript", {
          thread: target.id,
          turn: earliestTurn - 1,
          since_seq: page.next,
          limit: 500,
        });
        more.push(...page.events);
      }
      if (generation !== transcriptGeneration.current) return;
      setEvents((old) => mergeEvents(old, more));
      setEarliestTurn(earliestTurn - 1);
    } catch (e) {
      fail(e);
    } finally {
      setEarlierBusy(false);
    }
  };
  function chooseMention(thread: Thread) {
    setDrafts((old) => ({
      ...old,
      [draftKey]: draft.replace(/@[\w-]*$/, "@" + thread.name + " "),
    }));
    setMentionHidden(true);
    input.current?.focus();
  }
  const mentions = draft.match(/(?:^|\s)@([\w-]*)$/)
    ? workers
        .filter((t) => t.name.startsWith(draft.match(/@([\w-]*)$/)?.[1] || ""))
        .slice(0, 6)
    : [];
  const slashMatches =
    draft.startsWith("/") && !draft.startsWith("//")
      ? (slashCommands.length ? slashCommands : commands)
          .filter((c) => c.id.startsWith(draft.slice(1).split(/\s/)[0]))
          .slice(0, 6)
      : [];
  useEffect(() => {
    setPaletteIndex(0);
  }, [query]);
  useEffect(() => {
    document
      .querySelector<HTMLElement>("[data-palette-active=true]")
      ?.scrollIntoView({ block: "nearest" });
  }, [paletteIndex]);
  const allCommands = useMemo(
    () => [...commands, ...(permitted("terminal") ? cliCommands : [])],
    [cliCommands, commands],
  );
  const matching = allCommands.filter((c) =>
    (c.title + " " + c.group + " " + c.id)
      .toLowerCase()
      .includes(query.toLowerCase()),
  );
  const rootStyle = target
    ? ({
        "--destination": destination ? target.accent : "var(--accent)",
      } as CSSProperties)
    : {};
  return (
    <div className={`app ${sidebar ? "" : "sidebar-hidden"}`} style={rootStyle}>
      <button
        className="sidebar-backdrop"
        aria-label="Close navigation"
        onClick={() => setSidebar(false)}
        style={{ display: sidebar ? undefined : "none" }}
      />
      <aside
        className="sidebar"
        onClick={(event) => {
          if (
            window.innerWidth <= 800 &&
            (event.target as HTMLElement).closest(
              ".primary-nav button,.thread-card,.sidebar-secondary button,.sidebar-footer button",
            )
          )
            setSidebar(false);
        }}
      >
        <div className="brand drag-region">
          <span className="wordmark">
            twr<span>_</span>
          </span>
          <span className="brand-caption">Hub client</span>
          <button
            className="icon-button no-drag"
            aria-label="Hide sidebar"
            onClick={() => setSidebar(false)}
          >
            <PanelLeftClose size={16} />
          </button>
        </div>
        <div className="workspace-picker">
          <label htmlFor="workspace-select">Workspace</label>
          <div>
            <select
              id="workspace-select"
              value={workspace}
              disabled={connecting}
              onChange={(e) => void connect(e.target.value)}
            >
              {[
                ...new Set([
                  workspace,
                  ...workspaces.map((w) =>
                    typeof w === "string" ? w : w.name,
                  ),
                ]),
              ]
                .filter(Boolean)
                .map((w) => (
                  <option key={w} value={w}>
                    {w}
                  </option>
                ))}
            </select>
            <button
              className="icon-button"
              aria-label="Manage workspaces"
              onClick={() => setWorkspaceModal(true)}
            >
              <Plus size={16} />
            </button>
          </div>
        </div>
        <nav className="primary-nav">
          <button
            className={section === "chat" && !destination ? "active" : ""}
            onClick={() => {
              setSection("chat");
              setDestination(undefined);
            }}
          >
            <MessageSquare size={17} />
            Orchestrator
          </button>
          <button
            className={section === "attention" ? "active" : ""}
            onClick={() => setSection("attention")}
          >
            <Bell size={17} />
            Needs you
            {view.attention.length > 0 && (
              <span className="count attention-count">
                {view.attention.length}
              </span>
            )}
          </button>
          <button
            className={section === "grid" ? "active" : ""}
            onClick={() => setSection("grid")}
          >
            <LayoutGrid size={17} />
            Thread grid<span className="count">{workers.length}</span>
          </button>
        </nav>
        <div className="threads-heading">
          <span>Threads</span>
          <div>
            <button
              className={`icon-button ${focus ? "on" : ""}`}
              title="Only active threads"
              aria-label="Toggle focus filter"
              onClick={() => setFocus((v) => !v)}
            >
              <Filter size={13} />
            </button>
            <button
              className="icon-button"
              aria-label="New thread"
              disabled={!permitted("operate")}
              onClick={() => action("new")}
            >
              <Plus size={16} />
            </button>
          </div>
        </div>
        <div
          className="thread-list"
          onMouseEnter={() => setHovering(true)}
          onMouseLeave={() => setHovering(false)}
        >
          {visible.map((thread) => (
            <ThreadCard
              key={thread.id}
              thread={thread}
              items={attentionFor(thread.id)}
              dived={destination === thread.id}
              selected={selected === thread.id}
              pinned={pins.includes(thread.id)}
              onSelect={() => setSelected(thread.id)}
              onOpen={() => {
                setDestination(thread.id);
                setSection("chat");
                setSelected(undefined);
              }}
            />
          ))}
          {!visible.length && (
            <p className="sidebar-empty">
              {workers.length
                ? "No threads match your filters."
                : "Threads appear here as your agents start working."}
            </p>
          )}
        </div>
        {workers.some((t) => t.status === "parked") && (
          <button
            className="parked-toggle"
            onClick={() => setParked((v) => !v)}
          >
            {parked
              ? "Hide parked"
              : `${workers.filter((t) => t.status === "parked").length} parked threads hidden`}
          </button>
        )}
        <details className="sidebar-secondary manage-nav">
          <summary>Manage</summary>
          <nav>
            {visibleNav
              .filter((n) => n.id !== "settings")
              .map((n) => (
                <button key={n.id} onClick={() => setSection(n.id)}>
                  <n.icon size={15} />
                  {n.label}
                </button>
              ))}
            <button onClick={() => setSection("decisions")}>
              <CheckCircle2 size={15} />
              Decisions
            </button>
          </nav>
        </details>
        <div className="sidebar-footer">
          <button
            onClick={() => {
              setQuery("");
              setPalette(true);
            }}
          >
            <Search size={15} />
            Commands
            <span className="key-hint" aria-hidden="true">
              {document.documentElement.dataset.platform === "darwin"
                ? "⌘K"
                : "Ctrl K"}
            </span>
          </button>
          <button
            disabled={!permitted("admin")}
            onClick={() => setSection("settings")}
          >
            <Settings2 size={15} />
            Settings
          </button>
          <div className="connection-status">
            <span className={`status-dot ${connected ? "online" : ""}`} />
            {connected
              ? "Hub connected"
              : connecting
                ? "Connecting…"
                : "Disconnected"}
            <span>v{paths?.version || "0.3.0"}</span>
          </div>
        </div>
      </aside>
      <main className="main-shell">
        <header className="topbar drag-region">
          <div className="topbar-left no-drag">
            {!sidebar && (
              <button
                className="icon-button"
                aria-label="Show sidebar"
                onClick={() => setSidebar(true)}
              >
                <PanelLeftOpen size={18} />
              </button>
            )}
            {destination && section === "chat" && (
              <button
                className="icon-button mobile-back-button"
                aria-label="Return to orchestrator"
                onClick={() => setDestination(undefined)}
              >
                <ArrowLeft size={18} />
              </button>
            )}
            <span className="workspace-breadcrumb">{workspace}</span>
            <ChevronRight size={13} />
            <span className="topbar-title">
              {destination
                ? target?.name
                : section === "chat"
                  ? "Orchestrator"
                  : section === "grid"
                    ? "Threads"
                    : section === "attention"
                      ? "Needs you"
                      : nav.find((n) => n.id === section)?.label || "Decisions"}
              {section === "chat" && target && (
                <small className="mobile-runtime">
                  {target.runtime} ·{" "}
                  {target.model_actual || target.model || "default model"}
                </small>
              )}
            </span>
            {connected && (
              <span
                className="live-indicator"
                title="Hub connected"
                aria-label="Live connection"
              >
                <span />
              </span>
            )}
          </div>
          {headerAccessory && (
            <div className="topbar-accessory no-drag">{headerAccessory}</div>
          )}
          <div className="topbar-actions no-drag">
            <button
              className="icon-button"
              aria-label="Open shell"
              disabled={!permitted("terminal")}
              title={
                !permitted("terminal")
                  ? "This device has no terminal access"
                  : "Open shell"
              }
              onClick={() =>
                void openTerminal({
                  shell: true,
                  cwd: localWorkingDirectory(target, homePath) || undefined,
                })
              }
            >
              <TerminalSquare size={17} />
            </button>
            <button
              className="icon-button"
              aria-label="Open command palette"
              onClick={() => {
                setPalette(true);
                setQuery("");
              }}
            >
              <CommandIcon size={17} />
            </button>
            {section === "chat" && (
              <details className="conversation-menu">
                <summary aria-label="Thread actions">
                  <MoreHorizontal size={18} />
                </summary>
                <div
                  onClick={(event) =>
                    (
                      event.currentTarget.parentElement as HTMLDetailsElement
                    ).removeAttribute("open")
                  }
                >
                  <button onClick={() => setSidebar(true)}>
                    <PanelLeftOpen size={16} />
                    Threads and navigation
                  </button>
                  <button
                    disabled={!permitted("operate")}
                    onClick={() => action("new")}
                  >
                    <Plus size={16} />
                    New thread
                  </button>
                  <button
                    aria-pressed={showTools}
                    onClick={() => setShowTools((v) => !v)}
                  >
                    <Wrench size={16} />
                    Tool calls
                  </button>
                  <button
                    aria-pressed={showThinking}
                    onClick={() => setShowThinking((v) => !v)}
                  >
                    <Eye size={16} />
                    Reasoning
                  </button>
                  {target && (
                    <button onClick={() => void exportConversation()}>
                      <Download size={16} />
                      Export conversation
                    </button>
                  )}
                  <button
                    onClick={() =>
                      action(
                        destination ? "model" : "orch",
                        destination ? { thread: destination } : {},
                      )
                    }
                  >
                    <Settings2 size={16} />
                    {destination ? "Thread settings" : "Orchestrator settings"}
                  </button>
                  <button
                    onClick={() => {
                      setPalette(true);
                      setQuery("");
                    }}
                  >
                    <Search size={16} />
                    Commands
                  </button>
                </div>
              </details>
            )}
            <button
              className="topbar-new-thread"
              disabled={!permitted("operate")}
              onClick={() => action("new")}
            >
              <Plus size={14} />
              New thread
            </button>
          </div>
        </header>
        {paths?.versionMismatch && (
          <div className="connection-banner warning">
            <AlertCircle size={15} />
            <span>
              The app bundles twr {paths.version}; this hub still runs{" "}
              {paths.hubVersion}. Stop and reconnect when you’re ready to use
              the new engine.
            </span>
            <button onClick={() => action("hub-stop")}>Stop hub…</button>
          </div>
        )}
        {(connectionError || connecting) && (
          <div
            className={`connection-banner ${connectionError ? "warning" : ""}`}
          >
            {connecting ? <Spinner /> : <AlertCircle size={15} />}
            <span>
              {connecting ? "Connecting to your workspace…" : connectionError}
            </span>
            {!connecting && (
              <button onClick={() => void connect(workspace)}>Reconnect</button>
            )}
          </div>
        )}
        <div className="work-area">
          <div className="content-column">
            {section === "chat" && (
              <>
                <header className="conversation-heading">
                  <div>
                    {destination && (
                      <button
                        className="icon-button"
                        aria-label="Back to orchestrator"
                        onClick={() => setDestination(undefined)}
                      >
                        <ArrowLeft size={17} />
                      </button>
                    )}
                    <span
                      className={`destination-symbol ${destination ? "worker" : ""}`}
                    >
                      {destination ? (
                        <ArrowUpRight size={18} />
                      ) : (
                        <Asterisk size={20} />
                      )}
                    </span>
                    <div>
                      <h1>{destination ? target?.name : "Orchestrator"}</h1>
                      <p>
                        {target
                          ? `${target.runtime}${target.account ? " · " + target.account : ""} · ${target.model_actual || target.model || "default model"}${target.effort ? " · " + target.effort : ""}`
                          : "One conversation for all your coding agents."}
                      </p>
                    </div>
                  </div>
                  <div className="conversation-actions">
                    <button
                      className={`icon-button ${showTools ? "on" : ""}`}
                      aria-label="Toggle tool calls"
                      title="Show tool calls"
                      onClick={() => setShowTools((v) => !v)}
                    >
                      <Wrench size={16} />
                    </button>
                    <button
                      className={`icon-button ${showThinking ? "on" : ""}`}
                      aria-label="Toggle reasoning"
                      title="Show reasoning"
                      onClick={() => setShowThinking((v) => !v)}
                    >
                      <Eye size={16} />
                    </button>
                    {target && (
                      <button
                        className="icon-button"
                        aria-label="Export conversation"
                        onClick={() => void exportConversation()}
                      >
                        <Download size={16} />
                      </button>
                    )}
                    <button
                      className="icon-button"
                      aria-label="Orchestrator settings"
                      onClick={() =>
                        action(
                          destination ? "model" : "orch",
                          destination ? { thread: destination } : {},
                        )
                      }
                    >
                      <MoreHorizontal size={18} />
                    </button>
                  </div>
                </header>
                <div className="conversation-attention">
                  {view.attention
                    .filter(
                      (a) => a.thread === "hub" || a.thread === target?.id,
                    )
                    .map((item) => (
                      <AttentionCard
                        key={item.attn_id + ":" + item.revision}
                        item={item}
                        thread={view.threads.find((t) => t.id === item.thread)}
                        readOnly={!permitted("operate")}
                        onRespond={respond}
                        rpc={rpc}
                        fail={fail}
                      />
                    ))}
                </div>
                <Conversation
                  key={target?.id || "orchestrator"}
                  events={presentedEvents}
                  showTools={showTools}
                  showThinking={showThinking}
                  loading={transcriptLoading}
                  error={transcriptError}
                  earlier={
                    earliestTurn && earliestTurn > 1 ? earlier : undefined
                  }
                  earlierBusy={earlierBusy}
                  target={target}
                  onSetup={() => action("orch")}
                  connected={connected}
                  summaries={assistance.summaries}
                  onError={fail}
                  onVisibleRoutines={setVisibleRoutines}
                />
                {!destination && workers.some((t) => t.last_report) && (
                  <details className="reports">
                    <summary>
                      Latest thread reports{" "}
                      <span>{workers.filter((t) => t.last_report).length}</span>
                    </summary>
                    {workers
                      .filter((t) => t.last_report)
                      .map((t) => (
                        <button key={t.id} onClick={() => setSelected(t.id)}>
                          <span style={{ color: t.accent }}>{t.name}</span>
                          <span>{t.last_report.summary}</span>
                          <ArrowUpRight size={14} />
                        </button>
                      ))}
                  </details>
                )}
                {queued.length > 0 && (
                  <div className="queue">
                    <div className="eyebrow">
                      QUEUED MESSAGES · {queueInfo.total || queued.length}
                      {queueInfo.held ? " · Held after a failure" : ""}
                    </div>
                    {queued.map((message, i) => (
                      <div
                        className="queued-message"
                        key={message.ref || message.id || i}
                      >
                        <span>
                          {message.prompt ||
                            message.message ||
                            JSON.stringify(message)}
                        </span>
                        <button
                          disabled={!message.mine}
                          onClick={() => void takeBack(message)}
                        >
                          Take back
                        </button>
                        <button
                          disabled={i !== 0}
                          onClick={() => {
                            setConfirmation({
                              title: "Send this message now?",
                              detail:
                                "The running turn will be interrupted so this message can start.",
                              run: () => void sendNow(message),
                            });
                          }}
                        >
                          Send now
                        </button>
                      </div>
                    ))}
                  </div>
                )}
                <div className="shell-runs">
                  {shellJobs.map((job) => (
                    <div key={job.job}>
                      <TerminalSquare size={12} />
                      <span>{job.command}</span>
                      <button
                        onClick={() => action("job-log", { job: job.job })}
                      >
                        Output
                      </button>
                      <button
                        onClick={() => action("job-cancel", { job: job.job })}
                      >
                        Stop…
                      </button>
                      <button
                        aria-label="Hide shell output"
                        onClick={() =>
                          setShellJobs((old) =>
                            old.filter((j) => j.job !== job.job),
                          )
                        }
                      >
                        <X size={12} />
                      </button>
                    </div>
                  ))}
                </div>
                {assistance.needsDisclosure && (
                  <AssistanceNotice
                    settings={assistance.settings}
                    acknowledge={assistance.acknowledge}
                    disable={() => {
                      void Promise.all([
                        hostApi.configSet(["tui"], "suggest", false),
                        hostApi.configSet(["tui"], "summaries", false),
                      ])
                        .then(() => setRefresh((n) => n + 1))
                        .catch(fail);
                    }}
                  />
                )}
                {assistance.notice && (
                  <div className="assistance-status" role="status">
                    {assistance.notice}
                    <button onClick={assistance.retry}>Retry assistance</button>
                    <button
                      onClick={assistance.clearNotice}
                      aria-label="Dismiss assistance notice"
                    >
                      <X size={12} />
                    </button>
                  </div>
                )}
                {!draft && assistance.suggestion && (
                  <button
                    className="suggested-message"
                    onClick={acceptSuggestion}
                  >
                    <span>{assistance.suggestion}</span>
                    <kbd>Tab to use</kbd>
                  </button>
                )}
                <form
                  className={`composer ${draft.startsWith("!") && !draft.startsWith("!!!") ? "shell-mode" : ""}`}
                  onSubmit={(event) => void send(event)}
                >
                  <div className="composer-label">
                    <span className="destination-chip">
                      <span className="destination-dot" />
                      To:{" "}
                      <b>
                        {draft.startsWith("!") && !draft.startsWith("!!!")
                          ? "Shell · " +
                            (destination
                              ? target?.name
                              : shellCwd || "workspace folder") +
                            (draft.startsWith("!!") ? " · share result" : "")
                          : destination
                            ? target?.name
                            : "Orchestrator"}
                      </b>
                      {destination && (
                        <button
                          type="button"
                          aria-label="Return to orchestrator"
                          onClick={() => setDestination(undefined)}
                        >
                          <X size={12} />
                        </button>
                      )}
                    </span>

                    <span>
                      {!connected
                        ? "Disconnected"
                        : target
                          ? target.status_label || target.status
                          : "Choose an orchestrator"}
                    </span>
                    {onDismissKeyboard && (
                      <button
                        type="button"
                        className="keyboard-dismiss"
                        aria-label="Dismiss keyboard"
                        onClick={onDismissKeyboard}
                      >
                        <ChevronDown size={18} />
                      </button>
                    )}
                  </div>
                  <textarea
                    ref={input}
                    aria-label={
                      destination
                        ? `Message ${target?.name} directly`
                        : "Message orchestrator"
                    }
                    value={draft}
                    onChange={(e) => {
                      setAcceptedSuggestion(undefined);
                      setDrafts((old) => ({
                        ...old,
                        [draftKey]: e.target.value,
                      }));
                      setSlashHidden(false);
                      setMentionHidden(false);
                      setMentionIndex(0);
                    }}
                    placeholder={
                      destination
                        ? `Message ${target?.name} directly…`
                        : "Ask your orchestrator to build something…"
                    }
                    rows={Math.min(
                      6,
                      Math.max(
                        document.documentElement.dataset.mobile === "true"
                          ? 1
                          : 2,
                        draft.split("\n").length,
                      ),
                    )}
                    disabled={!connected || !permitted("operate")}
                    onKeyDown={(e) => {
                      if (e.nativeEvent.isComposing || e.keyCode === 229)
                        return;
                      if (e.key === "Tab" && !draft && assistance.suggestion) {
                        e.preventDefault();
                        acceptSuggestion();
                        return;
                      }
                      if (mentions.length && !mentionHidden) {
                        if (e.key === "ArrowDown" || e.key === "ArrowUp") {
                          e.preventDefault();
                          setMentionIndex(
                            (i) =>
                              (i +
                                (e.key === "ArrowDown"
                                  ? 1
                                  : mentions.length - 1)) %
                              mentions.length,
                          );
                          return;
                        }
                        if (
                          (e.key === "Enter" && !e.shiftKey) ||
                          e.key === "Tab"
                        ) {
                          e.preventDefault();
                          chooseMention(
                            mentions[mentionIndex % mentions.length],
                          );
                          return;
                        }
                        if (e.key === "Escape") {
                          e.preventDefault();
                          e.stopPropagation();
                          setMentionHidden(true);
                          return;
                        }
                      }
                      if (
                        e.key === "Escape" &&
                        slashMatches.length &&
                        !slashHidden
                      ) {
                        e.preventDefault();
                        e.stopPropagation();
                        setSlashHidden(true);
                        return;
                      }
                      if (
                        e.key === "Enter" &&
                        !e.shiftKey &&
                        document.documentElement.dataset.mobile !== "true" &&
                        !e.nativeEvent.isComposing
                      ) {
                        e.preventDefault();
                        void send();
                      }
                    }}
                  />
                  <div className="composer-bottom">
                    {hostApi.uploadFile && permitted("operate") && (
                      <label
                        className="attachment-button"
                        title="Upload files to this thread"
                      >
                        <Paperclip size={18} />
                        <span className="sr-only">Upload files</span>
                        <input
                          type="file"
                          aria-label="Upload files"
                          multiple
                          disabled={!connected || !target || sending}
                          onChange={async (event) => {
                            const files = Array.from(event.target.files || []);
                            event.target.value = "";
                            if (!target) return;
                            const thread = target.id;
                            const key = draftKey;
                            setSending(true);
                            try {
                              for (const file of files) {
                                const uploaded = await hostApi.uploadFile!(
                                  file,
                                  thread,
                                );
                                setDrafts((old) => ({
                                  ...old,
                                  [key]:
                                    `${old[key] || ""}\nUploaded file: ${file.name}\n${uploaded.path}\n`.trimStart(),
                                }));
                              }
                            } catch (error) {
                              fail(error);
                            } finally {
                              setSending(false);
                            }
                          }}
                        />
                      </label>
                    )}
                    <span>
                      <kbd>/</kbd> commands <span>·</span> <kbd>@</kbd>{" "}
                      reference a thread <span>·</span> Shift Enter for a new
                      line
                      {assistance.stats && (
                        <small
                          title={`${assistance.stats.calls} calls · ${assistance.stats.failures} failed`}
                        >
                          {" "}
                          · {assistance.stats.source} $
                          {Number(assistance.stats.cost_usd || 0).toFixed(4)}{" "}
                          USD
                        </small>
                      )}
                    </span>
                    <button
                      className="send-button"
                      type="submit"
                      disabled={
                        !permitted("operate") ||
                        !draft.trim() ||
                        !connected ||
                        (!target && !draft.startsWith("/")) ||
                        sending
                      }
                      aria-label="Send message"
                    >
                      {sending ? <Spinner /> : <ArrowUp size={18} />}
                    </button>
                  </div>
                  {mentions.length > 0 && !mentionHidden && (
                    <div className="mentions">
                      {mentions.map((t, i) => (
                        <button
                          className={i === mentionIndex ? "active" : ""}
                          type="button"
                          key={t.id}
                          onClick={() => chooseMention(t)}
                        >
                          {" "}
                          @{t.name}
                        </button>
                      ))}
                    </div>
                  )}
                  {slashMatches.length > 0 && !slashHidden && (
                    <div className="mentions slash-suggestions">
                      {slashMatches.map((command) => (
                        <button
                          type="button"
                          key={command.id}
                          onClick={() =>
                            submitSlash(
                              "/" +
                                command.id +
                                draft.slice(1).replace(/^\S+/, ""),
                            )
                          }
                        >
                          <b>/{command.id}</b>
                          <span>{command.title}</span>
                        </button>
                      ))}
                    </div>
                  )}
                </form>
              </>
            )}
            {section === "grid" && (
              <div className="manager">
                <header className="section-heading">
                  <div>
                    <span className="eyebrow">Your agents</span>
                    <h1>Threads</h1>
                    <p>
                      Live work, clear outcomes, and the things waiting on you.
                    </p>
                  </div>
                  <button
                    className="primary grid-new-thread"
                    disabled={!permitted("operate")}
                    onClick={() => action("new")}
                  >
                    <Plus size={16} />
                    New thread
                  </button>
                </header>
                <div className="manager-actions">
                  <button
                    className={focus ? "active" : ""}
                    onClick={() => setFocus((v) => !v)}
                  >
                    <Filter size={14} />
                    Focus {focus ? "on" : "off"}
                  </button>
                  <button onClick={() => setParked((v) => !v)}>
                    {parked ? "Hide" : "Show"} parked
                  </button>
                </div>
                <div className="thread-grid">
                  {visible.map((t) => (
                    <ThreadCard
                      key={t.id}
                      thread={t}
                      items={attentionFor(t.id)}
                      dived={destination === t.id}
                      selected={selected === t.id}
                      pinned={pins.includes(t.id)}
                      onSelect={() => setSelected(t.id)}
                      onOpen={() => {
                        setDestination(t.id);
                        setSection("chat");
                        setSelected(undefined);
                      }}
                      large
                    />
                  ))}
                </div>
                {!visible.length && (
                  <Empty
                    title="A little room for parallel work"
                    description="Start a thread on a repository, or ask your orchestrator to split up the work."
                  />
                )}
              </div>
            )}
            {section === "attention" && (
              <div className="manager">
                <header className="section-heading">
                  <div>
                    <span className="eyebrow">Your attention</span>
                    <h1>
                      Needs you{" "}
                      <span className="heading-count">
                        {view.attention.length}
                      </span>
                    </h1>
                    <p>
                      Review the request, then decide. Your agents will take it
                      from there.
                    </p>
                  </div>
                </header>
                {view.attention.map((item) => (
                  <AttentionCard
                    key={item.attn_id + ":" + item.revision}
                    item={item}
                    thread={view.threads.find((t) => t.id === item.thread)}
                    readOnly={!permitted("operate")}
                    onRespond={respond}
                    rpc={rpc}
                    fail={fail}
                  />
                ))}
                {!view.attention.length && (
                  <Empty
                    title="You’re all caught up"
                    description="Questions, permissions, and completed work will appear here."
                  />
                )}
              </div>
            )}
            {section === "decisions" && (
              <div className="manager">
                <header className="section-heading">
                  <div>
                    <h1>Decision history</h1>
                    <p>{view.decisions_seen} recorded decisions</p>
                  </div>
                </header>
                <DataView
                  value={view.decisions.map(([gseq, event]) => ({
                    sequence: gseq,
                    ...event.data,
                  }))}
                />
                <button onClick={() => action("control")}>
                  Read older control history
                </button>
              </div>
            )}
            {!["chat", "grid", "attention", "decisions"].includes(section) && (
              <Manager
                key={workspace + ":" + section}
                section={section}
                workspace={workspace}
                refreshKey={refresh}
                rpc={rpc}
                action={action}
                cliAction={cliAction}
                fail={fail}
              />
            )}
          </div>
          {selectedThread && (
            <aside className="inspector">
              <header>
                <span className="eyebrow">Thread details</span>
                <button
                  className="icon-button"
                  aria-label="Close thread details"
                  onClick={() => setSelected(undefined)}
                >
                  <X size={17} />
                </button>
              </header>
              <div className="inspector-body">
                <span className="thread-runtime">
                  {selectedThread.runtime} · {selectedThread.box_name}
                </span>
                <h2 style={{ color: selectedThread.accent }}>
                  {selectedThread.name}
                </h2>
                <div
                  className={`thread-status status-${selectedThread.status}`}
                >
                  {statusIcon(selectedThread.status)}
                  {selectedThread.status_label || selectedThread.status}
                </div>
                <button
                  className="primary full-width"
                  onClick={() => {
                    setDestination(selectedThread.id);
                    setSection("chat");
                    setSelected(undefined);
                  }}
                >
                  <MessageSquare size={15} />
                  Talk to this thread
                </button>
                <button
                  className="full-width"
                  onClick={() =>
                    void openTerminal({
                      args: ["attach", selectedThread.id],
                      title: selectedThread.name,
                    })
                  }
                >
                  <TerminalSquare size={15} />
                  Attach agent terminal
                </button>
                <div className="inspector-meta">
                  <span>Project</span>
                  <b>{selectedThread.project || "—"}</b>
                  <span>Model</span>
                  <b>
                    {selectedThread.model_actual ||
                      selectedThread.model ||
                      "Default"}
                  </b>
                  <span>Permission mode</span>
                  <b>{selectedThread.mode}</b>
                  <span>Worktree</span>
                  <button
                    className="text-button path"
                    onClick={() =>
                      void hostApi
                        .reveal(
                          selectedThread.worktree || selectedThread.repo || "",
                        )
                        .catch(fail)
                    }
                  >
                    {selectedThread.worktree || selectedThread.repo || "—"}
                  </button>
                </div>
                {attentionFor(selectedThread.id).map((item) => (
                  <AttentionCard
                    key={item.attn_id + ":" + item.revision}
                    item={item}
                    thread={selectedThread}
                    readOnly={!permitted("operate")}
                    onRespond={respond}
                    rpc={rpc}
                    fail={fail}
                  />
                ))}
                {selectedThread.last_report && (
                  <section className="inspector-report">
                    <h3>Latest report</h3>
                    <Markdown text={selectedThread.last_report.summary || ""} />
                  </section>
                )}
                <div className="inspector-actions">
                  <button onClick={() => togglePin(selectedThread.id)}>
                    <Pin size={14} />
                    {pins.includes(selectedThread.id) ? "Unpin" : "Pin"} thread
                  </button>
                  {[
                    "mode",
                    "model",
                    "rename",
                    "compact",
                    "resume",
                    "stop",
                    "park",
                    "trim",
                    "delete",
                  ]
                    .filter((id) => commands.some((c) => c.id === id))
                    .map((id) => (
                      <button
                        className={id === "delete" ? "danger-quiet" : ""}
                        key={id}
                        onClick={() =>
                          action(id, { thread: selectedThread.id })
                        }
                      >
                        {commands.find((c) => c.id === id)?.title}
                      </button>
                    ))}
                </div>
                <details>
                  <summary>Full thread state</summary>
                  <DataView value={selectedThread} />
                </details>
              </div>
            </aside>
          )}
        </div>
        <nav className="compact-nav">
          <button
            onClick={() => {
              setSection("chat");
            }}
          >
            <MessageSquare size={15} />
            Chat
          </button>
          <button onClick={() => setSection("attention")}>
            <Bell size={15} />
            Needs you
            {view.attention.length > 0 && (
              <span className="count">{view.attention.length}</span>
            )}
          </button>
          <button onClick={() => setSection("grid")}>
            <LayoutGrid size={15} />
            Threads
          </button>
          <button onClick={() => setSidebar((v) => !v)}>
            <MoreHorizontal size={16} />
            More
          </button>
        </nav>

        {terminals.length > 0 && (
          <section
            className={`terminal-dock ${terminalVisible ? "" : "collapsed"}`}
          >
            <div className="terminal-tabs">
              <button
                className="icon-button"
                aria-label={
                  terminalVisible ? "Collapse terminals" : "Expand terminals"
                }
                onClick={() => setTerminalVisible((v) => !v)}
              >
                <ChevronDown size={15} />
              </button>
              {terminals.map((t) => (
                <div
                  className={`terminal-tab ${t.id === activeTerminal ? "active" : ""}`}
                  key={t.id}
                >
                  <button
                    onClick={() => {
                      setActiveTerminal(t.id);
                      setTerminalVisible(true);
                    }}
                  >
                    {t.exited ? (
                      <Square size={10} />
                    ) : (
                      <TerminalSquare size={13} />
                    )}
                    <span>{t.title}</span>
                  </button>
                  <button
                    aria-label={`Close ${t.title}`}
                    onClick={() =>
                      setConfirmation({
                        title: "Close terminal?",
                        detail: t.exited
                          ? "Remove this terminal tab?"
                          : "Its interactive program will exit. The hub and other agents keep running.",
                        run: () => {
                          void hostApi
                            .terminalClose(t.id)
                            .then(() => {
                              setTerminals((old) =>
                                old.filter((s) => s.id !== t.id),
                              );
                              if (activeTerminal === t.id)
                                setActiveTerminal(
                                  terminals.find((s) => s.id !== t.id)?.id,
                                );
                            })
                            .catch(fail);
                        },
                      })
                    }
                  >
                    <X size={12} />
                  </button>
                </div>
              ))}
              <button
                className="icon-button"
                aria-label="New shell terminal"
                onClick={() => void openTerminal({ shell: true })}
              >
                <Plus size={15} />
              </button>
            </div>
            <div
              className="terminal-content"
              style={{ display: terminalVisible ? "block" : "none" }}
            >
              <Suspense
                fallback={
                  <div className="transcript-loading">
                    <Spinner />
                    Opening terminal…
                  </div>
                }
              >
                {terminals.map((t) => (
                  <Terminal
                    key={t.id}
                    session={t}
                    visible={terminalVisible && activeTerminal === t.id}
                  />
                ))}
              </Suspense>
            </div>
          </section>
        )}
      </main>
      {error && (
        <div role="alert" className="toast">
          <AlertCircle size={17} />
          <span>{error}</span>
          <button aria-label="Dismiss error" onClick={() => setError("")}>
            <X size={15} />
          </button>
        </div>
      )}
      {palette && (
        <Dialog
          title="Commands"
          subtitle="Every workflow, one place. CLI actions run in an embedded terminal."
          onClose={() => setPalette(false)}
        >
          <div className="palette-search">
            <Search size={18} />
            <input
              autoFocus
              aria-label="Search commands"
              value={query}
              onChange={(e) => setQuery(e.target.value)}
              placeholder="Search commands, settings, or CLI actions…"
              onKeyDown={(e) => {
                if (e.key === "ArrowDown" || e.key === "ArrowUp") {
                  e.preventDefault();
                  setPaletteIndex((i) =>
                    matching.length
                      ? (i +
                          (e.key === "ArrowDown" ? 1 : matching.length - 1)) %
                        matching.length
                      : 0,
                  );
                }
                if (e.key === "Enter" && matching[paletteIndex])
                  openOperation(matching[paletteIndex], {
                    thread: selected || destination || "",
                  });
              }}
            />
          </div>
          <div className="palette-list">
            {matching.map((command, index) => (
              <button
                data-cli={command.cli ? "true" : undefined}
                data-palette-active={index === paletteIndex}
                className={index === paletteIndex ? "active" : ""}
                key={command.id}
                onClick={() =>
                  openOperation(command, {
                    thread: selected || destination || "",
                  })
                }
              >
                <span>
                  <b>{command.title}</b>
                  <small>{command.description.slice(0, 130)}</small>
                </span>
                <span className="command-group">{command.group}</span>
                <ChevronRight size={15} />
              </button>
            ))}
            {!matching.length && <p className="muted">No matching commands.</p>}
            {!cliCommands.length && (
              <button
                onClick={() =>
                  void hostApi.catalog().then(setCliCommands).catch(fail)
                }
              >
                Reload CLI workflows
              </button>
            )}
          </div>
          <footer className="dialog-footer">
            <span>
              {cliCommands.length} CLI workflows · {commands.length} application
              actions
            </span>
          </footer>
        </Dialog>
      )}
      {operation && (
        <Dialog
          title={operation.title}
          subtitle={operation.description}
          onClose={() => {
            if (!operationBusy) setOperation(undefined);
          }}
          wide={operation.fields.some((f) => f.type === "textarea")}
        >
          {operationError && (
            <div className="operation-error error-banner" role="alert">
              {operationError}
            </div>
          )}
          <form
            onSubmit={(e) => {
              e.preventDefault();
              void execute();
            }}
          >
            <div className="operation-fields">
              {operation.fields.map((field) => (
                <FieldInput
                  key={field.key}
                  field={
                    field.key === "runtime"
                      ? {
                          ...field,
                          options: [
                            ...new Set([
                              "claude-code",
                              "codex",
                              "grok-build",
                              ...(info?.agents || []).map(
                                (a: Json) => a.runtime,
                              ),
                            ]),
                          ],
                        }
                      : field.key === "mode" && !permitted("admin")
                        ? {
                            ...field,
                            options: ["manual", "plan", "read-only", "dontAsk"],
                          }
                        : field
                  }
                  value={values[field.key]}
                  onChange={(value) =>
                    setValues((old) => ({ ...old, [field.key]: value }))
                  }
                />
              ))}
              {(operation.cli || operation.method === "thread.new") && (
                <FieldInput
                  field={{
                    key: "_cwd",
                    label: "Working directory",
                    type: "directory",
                    required: true,
                    description:
                      "Relative paths in this command are resolved from this folder.",
                  }}
                  value={values._cwd}
                  onChange={(value) =>
                    setValues((old) => ({ ...old, _cwd: value }))
                  }
                />
              )}
              {operation.cli && (
                <p className="form-note">
                  Runs in this workspace’s embedded terminal. Any interactive
                  confirmation or login stays there.
                </p>
              )}

              {result !== undefined && (
                <div className="operation-result">
                  {operation.id === "job-log" ? (
                    <JobLog job={values.job} rpc={rpc} />
                  ) : operation.id === "app-update" ? (
                    <>
                      <DataView value={result} />
                      <button
                        onClick={() =>
                          void hostApi
                            .openExternal(result.release_page)
                            .catch(fail)
                        }
                      >
                        Open release downloads <ExternalLink size={14} />
                      </button>
                    </>
                  ) : operation.id === "context-read" ? (
                    <ContextEditor
                      key={values.project + ":" + result.hash}
                      value={result}
                      project={values.project}
                      rpc={rpc}
                      fail={fail}
                    />
                  ) : (
                    <DataView value={result} />
                  )}
                </div>
              )}
            </div>
            <footer className="dialog-footer">
              <span>In {workspace}</span>
              <div>
                <button
                  type="button"
                  onClick={() => setOperation(undefined)}
                  disabled={operationBusy}
                >
                  {result !== undefined ? "Close" : "Cancel"}
                </button>
                <button
                  className="primary"
                  type="submit"
                  disabled={operationBusy}
                >
                  {operationBusy ? (
                    <Spinner />
                  ) : operation.cli ? (
                    <TerminalSquare size={15} />
                  ) : operation.id === "new" ? (
                    <Plus size={15} />
                  ) : (
                    <ArrowUpRight size={15} />
                  )}{" "}
                  {operation.cli
                    ? "Open terminal"
                    : operationAction(operation, result !== undefined)}
                </button>
              </div>
            </footer>
          </form>
        </Dialog>
      )}
      {confirmation && (
        <Dialog
          title={confirmation.title}
          subtitle={confirmation.detail}
          onClose={() => setConfirmation(undefined)}
        >
          <footer className="dialog-footer">
            <span />
            <div>
              <button onClick={() => setConfirmation(undefined)}>Cancel</button>
              <button
                className="primary"
                onClick={() => {
                  const run = confirmation.run;
                  setConfirmation(undefined);
                  run();
                }}
              >
                Continue
              </button>
            </div>
          </footer>
        </Dialog>
      )}
      {workspaceModal && (
        <Dialog
          title="Workspaces"
          subtitle="Each workspace has its own hub, conversation, and threads."
          onClose={() => setWorkspaceModal(false)}
        >
          <div className="operation-fields">
            {workspaces.map((w) => (
              <div className="workspace-row" key={w.name}>
                <button
                  onClick={() => {
                    setWorkspaceModal(false);
                    void connect(w.name);
                  }}
                >
                  {w.name}
                  <span>{w.state || w.status}</span>
                </button>
                <button
                  className="icon-button danger-quiet"
                  aria-label={`Remove workspace ${w.name}`}
                  onClick={() => {
                    setWorkspaceModal(false);
                    cliAction("ws rm", { name: w.name });
                  }}
                >
                  <Trash2 size={14} />
                </button>
              </div>
            ))}
            <form
              onSubmit={(e) => {
                e.preventDefault();
                void hostApi
                  .cli(["ws", "new", newWorkspace])
                  .then((r) => {
                    if (r.code) throw new Error(r.stderr || r.stdout);
                    setWorkspaceModal(false);
                    setNewWorkspace("");
                    return connect(newWorkspace);
                  })
                  .catch(fail);
              }}
            >
              <label className="field">
                New workspace name
                <input
                  value={newWorkspace}
                  onChange={(e) => setNewWorkspace(e.target.value)}
                  pattern="[a-z0-9][a-z0-9-]{0,31}"
                  maxLength={32}
                  required
                  placeholder="e.g. client-project"
                />
              </label>
              <button className="primary">
                <Plus size={15} />
                Create workspace
              </button>
            </form>
          </div>
        </Dialog>
      )}
      {document.documentElement.dataset.mobile === "true" && (
        <nav className="mobile-tabs" aria-label="Main navigation">
          <button
            aria-current={section === "chat" ? "page" : undefined}
            onClick={() => {
              setSection("chat");
              setSidebar(false);
            }}
          >
            <MessageSquare size={19} />
            Chat
          </button>
          <button
            aria-current={section === "attention" ? "page" : undefined}
            onClick={() => {
              setSection("attention");
              setSidebar(false);
            }}
          >
            <Bell size={19} />
            Needs you
            {view.attention.length ? ` · ${view.attention.length}` : ""}
          </button>
          <button
            aria-current={section === "grid" ? "page" : undefined}
            onClick={() => {
              setSection("grid");
              setSidebar(false);
            }}
          >
            <LayoutGrid size={19} />
            Threads
          </button>
          <button aria-expanded={sidebar} onClick={() => setSidebar((v) => !v)}>
            <MoreHorizontal size={19} />
            More
          </button>
        </nav>
      )}
    </div>
  );
}

function ThreadCard({
  thread,
  items,
  selected,
  dived = false,
  pinned,
  onSelect,
  onOpen,
  large = false,
}: {
  thread: Thread;
  items: Attention[];
  selected: boolean;
  dived?: boolean;
  pinned: boolean;
  onSelect: () => void;
  onOpen: () => void;
  large?: boolean;
}) {
  const git = thread.git || thread.git_snapshot;
  const report = thread.last_report;
  return (
    <button
      className={`thread-card ${selected ? "selected" : ""} ${dived ? "dived" : ""} ${large ? "large" : ""}`}
      aria-current={dived ? "location" : undefined}
      style={{ "--thread-color": thread.accent } as CSSProperties}
      onClick={onSelect}
      onDoubleClick={onOpen}
      onKeyDown={(e) => {
        if (e.key === "Enter" && selected) {
          e.preventDefault();
          onOpen();
        }
      }}
    >
      <div className="thread-card-title">
        <span className="thread-color-dot" />
        <b>{thread.name}</b>
        {dived && (
          <ArrowUpRight
            size={13}
            className="dive-marker"
            role="img"
            aria-label="Current destination"
          />
        )}
        {pinned && <Pin size={11} />}
        <span className={`thread-status status-${thread.status}`}>
          {statusIcon(thread.status)}
          {thread.status_label || thread.status}
        </span>
      </div>

      {items[0] ? (
        <span className="thread-card-request">
          <AlertCircle size={12} />
          {items[0].summary}
        </span>
      ) : report && ["done", "idle", "parked"].includes(thread.status) ? (
        <span className="thread-card-summary">{report.summary}</span>
      ) : (
        <span className="thread-card-summary">
          {thread.activity ||
            (thread.background_tasks &&
              Object.values(thread.background_tasks)
                .map((t: any) => t.description)
                .join(" · ")) ||
            thread.project ||
            "Ready for a task"}
        </span>
      )}
      <span className="thread-card-runtime">
        {thread.runtime}
        {thread.box_name !== "local" ? " @" + thread.box_name : ""} ·{" "}
        {thread.model_actual || thread.model || "default model"}
        {git && (
          <>
            {" "}
            · <GitBranch size={11} />
            {git.branch || "detached"}
            {git.dirty_files ? " *" : ""}
            {git.ahead ? ` +${git.ahead}` : ""}
            {git.behind ? ` −${git.behind}` : ""}
          </>
        )}
      </span>
    </button>
  );
}
function AttentionCard({
  item,
  readOnly = false,
  thread,
  onRespond,
  rpc,
  fail,
}: {
  item: Attention;
  readOnly?: boolean;
  thread?: Thread;
  onRespond: (
    item: Attention,
    answer: string,
    message?: string,
  ) => Promise<void>;
  rpc: (method: string, params?: Json) => Promise<Json>;
  fail: (e: unknown) => void;
}) {
  const [answer, setAnswer] = useState("");
  const [message, setMessage] = useState("");
  const [busy, setBusy] = useState(false);
  const [requestError, setRequestError] = useState("");
  const options = item.options || [];
  const respond = async (value: string) => {
    if (readOnly || !canRespond(item, busy)) return;
    setBusy(true);
    setRequestError("");
    try {
      await rpc("attention.respond", {
        attn_id: item.attn_id,
        revision: item.revision,
        answer: value,
        ...(message ? { message } : {}),
      });
    } catch (e) {
      setRequestError(errorMessage(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <article
      className={`attention-card attention-${item.reason}`}
      tabIndex={0}
      onKeyDown={(event) => {
        if (
          event.target !== event.currentTarget ||
          readOnly ||
          busy ||
          item.incomplete ||
          !item.revision ||
          event.ctrlKey ||
          event.metaKey ||
          event.altKey
        )
          return;
        const answer = approvalShortcut(item, busy, event);
        if (answer !== undefined) {
          event.preventDefault();
          void respond(answer);
        }
      }}
    >
      <header>
        <span className="attention-kind">
          <AlertCircle size={14} />
          {item.reason === "permission"
            ? "Permission requested"
            : item.reason === "question"
              ? "Question for you"
              : item.reason}
        </span>
        <span>{thread?.name || "twr"}</span>
      </header>
      <div className="attention-body">
        {item.incomplete && (
          <p className="error-text">
            The full request could not be loaded. Answering is disabled until it
            can be reviewed.
            <button
              type="button"
              onClick={() =>
                void rpc("desktop.retry_attention", {
                  attn_id: item.attn_id,
                }).catch(fail)
              }
            >
              Retry loading request
            </button>
          </p>
        )}
        <pre className="attention-summary">{item.summary}</pre>
        {item.context?.task && (
          <details>
            <summary>Task</summary>
            <pre>{item.context.task}</pre>
          </details>
        )}
        {item.context?.why && <p>{item.context.why}</p>}
        {item.context?.cwd && (
          <code className="request-cwd">{item.context.cwd}</code>
        )}
        {item.context?.input && (
          <pre className="full-request">{item.context.input}</pre>
        )}
        {item.context?.said && (
          <details>
            <summary>What the agent said</summary>
            <Markdown text={item.context.said} />
          </details>
        )}
        {item.note && <p className="attention-note">{item.note}</p>}
      </div>
      {readOnly && <p className="muted">This device has read-only access.</p>}
      <div className="attention-response">
        <input
          aria-label="Optional message with your decision"
          placeholder="Add a note (optional)…"
          value={message}
          onChange={(e) => setMessage(e.target.value)}
          disabled={readOnly || busy}
        />
        <div className="attention-options">
          {options.map((option, i) => (
            <button
              disabled={readOnly || busy || !item.revision || item.incomplete}
              key={option + ":" + i}
              onClick={() => void respond(option)}
            >
              <kbd>{i + 1}</kbd>
              {option === "allow"
                ? "Allow"
                : option === "deny"
                  ? "Deny"
                  : option}
            </button>
          ))}
          {!options.length &&
            ["question", "permission"].includes(item.reason) && (
              <form
                onSubmit={(e) => {
                  e.preventDefault();
                  if (answer.trim()) void respond(answer);
                }}
              >
                <input
                  aria-label="Your answer"
                  value={answer}
                  onChange={(e) => setAnswer(e.target.value)}
                  placeholder="Your answer…"
                  disabled={readOnly || busy}
                />
                <button
                  disabled={
                    readOnly ||
                    busy ||
                    !answer.trim() ||
                    !item.revision ||
                    item.incomplete
                  }
                >
                  Answer
                </button>
              </form>
            )}
          {!["permission", "question"].includes(item.reason) && (
            <button
              disabled={readOnly || busy || !item.revision || item.incomplete}
              onClick={() => {
                setBusy(true);
                void rpc("attention.dismiss", {
                  attn_id: item.attn_id,
                  revision: item.revision,
                })
                  .catch(fail)
                  .finally(() => setBusy(false));
              }}
            >
              Dismiss
            </button>
          )}
          {busy && <Spinner />}
        </div>
        {requestError && (
          <p role="alert" className="error-text">
            {requestError} Review the current request before trying again.
          </p>
        )}
      </div>
    </article>
  );
}
function Conversation({
  events,
  showTools,
  showThinking,
  loading,
  error,
  earlier,
  earlierBusy,
  target,
  onSetup,
  connected,
  summaries,
  onVisibleRoutines,
  onError,
}: {
  onVisibleRoutines: (threads: string[]) => void;
  onError: (error: unknown) => void;
  summaries: Summary[];
  events: HubEvent[];
  showTools: boolean;
  showThinking: boolean;
  loading: boolean;
  error: string;
  earlier?: () => void;
  earlierBusy: boolean;
  target?: Thread;
  onSetup: () => void;
  connected: boolean;
}) {
  const hostApi = useHostAPI();
  const parent = useRef<HTMLDivElement>(null);
  const anchor = useRef<{ height: number; top: number } | undefined>(undefined);
  const follow = useRef(true);
  const [following, setFollowing] = useState(true);
  const blocks = useMemo(
    () =>
      foldRoutineBlocks(blocksOf(events), events, summaries).filter(
        (b) =>
          (showTools || b.kind !== "tool") &&
          (showThinking || b.kind !== "thinking"),
      ),
    [events, showTools, showThinking, summaries],
  );
  const virtual = useVirtualizer({
    count: blocks.length,
    getScrollElement: () => parent.current,
    estimateSize: () => 120,
    overscan: 8,
    getItemKey: (index) => blocks[index].id,
  });
  const totalSize = virtual.getTotalSize();
  useLayoutEffect(() => {
    const element = parent.current;
    if (!element) return;
    const observer = new ResizeObserver(() => {
      if (follow.current) element.scrollTop = element.scrollHeight;
    });
    observer.observe(element);
    return () => observer.disconnect();
  }, []);

  const visibleRoutineKey = [
    ...new Set(
      virtual
        .getVirtualItems()
        .map((row) => blocks[row.index])
        .filter((block) => block.children && block.about)
        .map((block) => block.about!),
    ),
  ]
    .sort()
    .join(",");
  useEffect(() => {
    onVisibleRoutines(visibleRoutineKey ? visibleRoutineKey.split(",") : []);
  }, [visibleRoutineKey, onVisibleRoutines]);
  const last = blocks.at(-1);
  function loadEarlier() {
    if (!earlier || earlierBusy || !parent.current) return;
    anchor.current = {
      height: parent.current.scrollHeight,
      top: parent.current.scrollTop,
    };
    follow.current = false;
    setFollowing(false);
    earlier();
  }
  useLayoutEffect(() => {
    if (anchor.current && parent.current) {
      const saved = anchor.current;
      anchor.current = undefined;
      parent.current.scrollTop =
        saved.top + Math.max(0, parent.current.scrollHeight - saved.height);
    }
  }, [blocks[0]?.id]);
  useEffect(() => {
    if (follow.current && blocks.length)
      requestAnimationFrame(() => {
        if (parent.current)
          parent.current.scrollTop = parent.current.scrollHeight;
      });
  }, [blocks.length, last?.text, totalSize]);
  return (
    <div
      className="conversation-scroll"
      ref={parent}
      onScroll={() => {
        const p = parent.current!;
        follow.current = p.scrollHeight - p.scrollTop - p.clientHeight < 100;
        setFollowing(follow.current);
        if (p.scrollTop < 40 && !loading && !earlierBusy && earlier)
          loadEarlier();
      }}
    >
      {earlier && (
        <button
          className="load-earlier"
          onClick={loadEarlier}
          disabled={earlierBusy}
        >
          {earlierBusy ? <Spinner /> : null}Load earlier turn
        </button>
      )}
      {error && <div className="error-banner">{error}</div>}
      {loading && (
        <div className="transcript-loading">
          <Spinner />
          Loading conversation…
        </div>
      )}
      {!loading && !blocks.length && (
        <Empty
          title={
            target
              ? target.is_orchestrator
                ? "What are we building?"
                : target.name
              : "One conversation. All your agents."
          }
          description={
            target
              ? target.is_orchestrator
                ? "Describe the outcome you want. Your orchestrator can coordinate the work, and bring decisions back to you."
                : "Send a task to this thread. Replies and requests will appear here."
              : "Choose the agent that will coordinate your workspace. Your existing subscriptions do the work."
          }
        >
          {!target && (
            <button className="primary" disabled={!connected} onClick={onSetup}>
              Choose your orchestrator
              <ArrowUpRight size={16} />
            </button>
          )}
          <div className="welcome-points">
            <span>
              <MessageSquare size={15} />
              Talk naturally
            </span>
            <span>
              <GitBranch size={15} />
              Work in parallel
            </span>
            <span>
              <Bell size={15} />
              Decide when needed
            </span>
          </div>
        </Empty>
      )}
      <div
        className="transcript-virtual"
        style={{ height: totalSize, position: "relative" }}
      >
        {virtual.getVirtualItems().map((row) => {
          const block = blocks[row.index];
          return (
            <article
              key={block.id}
              data-index={row.index}
              ref={virtual.measureElement}
              className={`transcript-block block-${block.kind}`}
              style={{
                position: "absolute",
                top: 0,
                left: 0,
                width: "100%",
                transform: `translateY(${row.start}px)`,
              }}
            >
              <div className="message-meta">
                <span className="message-avatar">
                  {block.kind === "user" ? (
                    <span aria-hidden="true">›</span>
                  ) : block.kind === "tool" ? (
                    <Wrench size={15} />
                  ) : block.kind === "thinking" ? (
                    <Circle size={14} />
                  ) : (
                    <Asterisk size={16} />
                  )}
                </span>
                <b>
                  {block.kind === "user"
                    ? "You"
                    : block.kind === "tool"
                      ? block.text
                      : block.kind === "thinking"
                        ? "Reasoning"
                        : block.kind === "notice"
                          ? "twr"
                          : target?.name || "Agent"}
                </b>
                <time>
                  {new Date(block.ts).toLocaleTimeString([], {
                    hour: "2-digit",
                    minute: "2-digit",
                  })}
                </time>
                {block.pending && <Spinner />}
                {block.kind === "assistant" && !block.pending && target && (
                  <button
                    className="icon-button"
                    aria-label="Read message aloud"
                    title="Read this message aloud"
                    onClick={() =>
                      void hostApi
                        .rpc("speech.request", {
                          thread: target.id,
                          seq: block.seq,
                        })
                        .catch(onError)
                    }
                  >
                    <Volume2 size={13} />
                  </button>
                )}
                <button
                  className="icon-button copy-message"
                  aria-label="Copy message"
                  onClick={() =>
                    void hostApi.copyText(block.detail || block.text)
                  }
                >
                  <Copy size={13} />
                </button>
              </div>
              {block.children ? (
                <details className="routine-updates">
                  <summary>
                    {block.text}
                    <span>
                      {" "}
                      · {block.children.length} update
                      {block.children.length === 1 ? "" : "s"}
                    </span>
                  </summary>
                  {block.children.map((entry) => (
                    <div key={entry.id}>
                      <time>{new Date(entry.ts).toLocaleTimeString()}</time>
                      <Markdown text={entry.text} />
                    </div>
                  ))}
                </details>
              ) : block.kind === "tool" ? (
                <details>
                  <summary>
                    {block.detail?.split("\n")[0] || "Tool call"}
                  </summary>
                  <MessageBody text={block.detail || ""} mono />
                </details>
              ) : (
                <MessageBody
                  text={block.text}
                  clamp={block.kind === "user" || block.kind === "notice"}
                />
              )}
            </article>
          );
        })}
      </div>
      {!following && (
        <button
          className="jump-latest"
          onClick={() => {
            follow.current = true;
            setFollowing(true);
            parent.current?.scrollTo({
              top: parent.current.scrollHeight,
              behavior: "smooth",
            });
          }}
        >
          <ArrowDown size={14} />
          Jump to latest
        </button>
      )}
    </div>
  );
}
function ContextEditor({
  value,
  project,
  rpc,
  fail,
}: {
  value: Json;
  project: string;
  rpc: (method: string, params?: Json) => Promise<Json>;
  fail: (e: unknown) => void;
}) {
  const editors = useEditorSessions();
  const key = "context:" + project;
  const existing = editors.get(key);
  const [text, setText] = useState(existing?.text ?? value.content ?? "");
  const [hash, setHash] = useState(existing?.hash ?? value.hash);
  const [busy, setBusy] = useState(false);
  const [saved, setSaved] = useState(false);
  return (
    <div className="editor-form">
      <label className="field">
        Standing context
        <textarea
          aria-label="Standing context"
          rows={14}
          disabled={value.truncated || value.utf8 === false}
          value={text}
          onChange={(e) => {
            setText(e.target.value);
            setSaved(false);
            editors.set(key, { text: e.target.value, hash });
          }}
        />
      </label>
      <button
        type="button"
        className="primary"
        disabled={busy || value.truncated || value.utf8 === false}
        onClick={() => {
          setBusy(true);
          void rpc("context.set", { project, content: text, expect_hash: hash })
            .then(() => rpc("context.read", { project }))
            .then((v) => {
              editors.clear(key);
              setHash(v.hash);
              setSaved(true);
            })
            .catch(fail)
            .finally(() => setBusy(false));
        }}
      >
        {busy ? <Spinner /> : null}Save context
      </button>
      {(value.truncated || value.utf8 === false) && (
        <p className="error-text">
          This file was truncated or is not UTF-8. Open it in your editor; the
          desktop will not overwrite a partial copy.
        </p>
      )}
      {saved && <p className="success-text">Context saved.</p>}
    </div>
  );
}

import {
  appendDraft,
  messageDraft,
  offlineJournal,
  type OfflineJournal,
} from "./offline";
import { TerminalSnapshots } from "./terminalSnapshots";
import { Outbox } from "./Outbox";
import { UploadController } from "./uploads";
import { AttachmentManager } from "./AttachmentManager";
import { MessageBody } from "./MessageBody";
import { ConversationSearch, NeedsYouInbox } from "./InboxSearch";
import { PaneResize } from "./LayoutOptions";
import { WORKSPACE_REFRESH_MS, workspaceEntries } from "./workspaceTree";
import { errorMessage } from "./errors";
import { savedAppearance } from "./appearance";
import { useHostAPI } from "./host";
import {
  useCallback,
  useEffect,
  useId,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  lazy,
  Suspense,
  type CSSProperties,
  type FormEvent,
  type ReactNode,
  type RefObject,
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
  Check,
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
  FolderPlus,
} from "lucide-react";
import {
  commands as applicationCommands,
  parameters,
  commandArguments,
  type Command,
} from "./commands";
import type {
  Attention,
  InboxSource,
  DesktopEvent,
  HubEvent,
  Json,
  TerminalSession,
  Thread,
  ViewState,
} from "./types";
import { DataView, Dialog, Empty, FieldInput, Markdown, Spinner } from "./ui";
import { effortChoices, formRuntime, modelChoices } from "./models";
import {
  AssistanceNotice,
  takesSuggestion,
  useAssistance,
  type Summary,
} from "./assistance";
import { VoiceControl } from "./VoiceControl";
import { announcements, voiceHint } from "./voice";
import { blocksOf, mergeEvents, foldRoutineBlocks } from "./transcript";
import { Manager, JobLog, type Section } from "./Managers";
import { useEditorSessions } from "./editors";
import { localWorkingDirectory } from "./paths";
import { approvalShortcut, canRespond } from "./attention";
import { armed, armsIn, hidden, mayAnswer, shown, type Arming } from "./arming";
import {
  announce,
  attentionAnnouncement,
  focusableWithin,
  handleRovingKey,
  prepareAnnouncer,
  scrollBehavior,
  turnAnnouncement,
  useModal,
} from "./a11y";
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
        ["accounts", "boxes", "maintenance"].includes(n.id) &&
        !permitted("admin")
      ),
  );

  const [view, setView] = useState(initial);
  const [workspace, setWorkspace] = useState("default");
  const [hostId, setHostId] = useState("local");
  const [inboxSources, setInboxSources] = useState<InboxSource[]>([]);
  const [inboxRefreshing, setInboxRefreshing] = useState(false);
  const [searchOpen, setSearchOpen] = useState(false);
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
  const [narrow, setNarrow] = useState(
    () => window.matchMedia("(max-width:800px)").matches,
  );
  const sidebarRef = useRef<HTMLElement>(null);
  // On narrow layouts the sidebar is an overlay sheet ("More"): treat it as modal.
  const sidebarModal = narrow && sidebar;
  useModal(sidebarRef, {
    active: sidebarModal,
    onClose: () => setSidebar(false),
    inertBackground: false,
    initialFocus: (root) =>
      focusableWithin(root).find((node) =>
        node.matches('[aria-current="page"]'),
      ),
  });
  const [hovering, setHovering] = useState(false);
  const [focus, setFocus] = useState(false);
  const [parked, setParked] = useState(false);
  const [collapsed, setCollapsed] = useState<string[]>(() => {
    try {
      const saved = JSON.parse(
        localStorage.getItem("twr.sidebar.collapsed") || "[]",
      );
      return Array.isArray(saved)
        ? saved.filter((v): v is string => typeof v === "string").slice(0, 256)
        : [];
    } catch {
      return [];
    }
  });
  const pendingOpen = useRef<{
    workspace: string;
    thread?: string;
    talk: boolean;
    then?: () => void;
  }>(undefined);
  const [pins, setPins] = useState<string[]>(() =>
    JSON.parse(localStorage.getItem("twr.pins") || "[]"),
  );
  const [selected, setSelected] = useState<string>();
  const [destination, setDestination] = useState<string>();
  const observedDestination = useRef<string | undefined>(undefined);
  const [drafts, setDrafts] = useState<Record<string, string>>({});
  const currentDrafts = useRef(drafts);
  currentDrafts.current = drafts;
  const draftSaveTimer = useRef<ReturnType<typeof setTimeout> | undefined>(
    undefined,
  );
  const pendingEnqueue = useRef<
    { key: string; text: string; saved: boolean } | undefined
  >(undefined);
  const [journal, setJournal] = useState<OfflineJournal>();
  const [offlineNotice, setOfflineNotice] = useState("");
  useEffect(() => {
    const reset = (event: Event) => {
      if ((event as CustomEvent).detail?.hostId === hostId) {
        setDrafts({});
        setJournal(offlineJournal(hostApi, hostId));
        setOfflineNotice("");
      }
    };
    const ready = (event: Event) => {
      if ((event as CustomEvent).detail?.hostId !== hostId) return;
      const saved = offlineJournal(hostApi, hostId);
      void saved
        .load()
        .then(() => {
          setDrafts((current) => ({ ...saved.snapshot.drafts, ...current }));
          setJournal(saved);
          setOfflineNotice("");
        })
        .catch((error) => setOfflineNotice(errorMessage(error)));
    };
    window.addEventListener("twr:offline-ready", ready);
    window.addEventListener("twr:offline-reset", reset);
    return () => {
      window.removeEventListener("twr:offline-reset", reset);
      window.removeEventListener("twr:offline-ready", ready);
    };
  }, [hostId]);
  const liveView = useRef(false);
  const hasLiveSnapshot = useRef(false);
  const [uploads] = useState(
    () =>
      new UploadController(hostApi, (attachment) => {
        const key = attachment.workspace + ":" + attachment.thread;
        const orchKey = attachment.workspace + ":orchestrator";
        setDrafts((old) => {
          const targetKey =
            attachment.thread ===
            currentOrchestrator.current[attachment.workspace]
              ? orchKey
              : key;
          return {
            ...old,
            [targetKey]:
              `${old[targetKey] || ""}\nUploaded file: ${attachment.file.name}\n${attachment.path}\n`.trimStart(),
          };
        });
      }),
  );
  const currentOrchestrator = useRef<Record<string, string | undefined>>({});
  useEffect(() => () => uploads.dispose(), [uploads]);
  useEffect(() => uploads.setWorkspace(workspace), [uploads, workspace]);
  useEffect(() => {
    if (!journal) return;
    const pending = pendingEnqueue.current;
    if (pending?.saved && drafts[pending.key] !== pending.text)
      pendingEnqueue.current = undefined;
    const epoch = journal.epoch;
    let timer: ReturnType<typeof setTimeout>;
    const persist = () => {
      if (pendingEnqueue.current) {
        timer = setTimeout(persist, 100);
        draftSaveTimer.current = timer;
        return;
      }
      void journal
        .drafts(currentDrafts.current, epoch)
        .then(() => setOfflineNotice(""))
        .catch((error) => setOfflineNotice(errorMessage(error)));
    };
    timer = setTimeout(persist, 350);
    draftSaveTimer.current = timer;
    return () => {
      clearTimeout(timer);
      if (draftSaveTimer.current === timer) draftSaveTimer.current = undefined;
    };
  }, [journal, drafts]);
  const [sending, setSending] = useState(false);
  const slashPending = useRef(false);
  const [queued, setQueued] = useState<Json[]>([]);
  const [queueInfo, setQueueInfo] = useState<Json>({});
  const [shellCwd, setShellCwd] = useState<string>();
  const [shellJobs, setShellJobs] = useState<Json[]>([]);
  const [slashHidden, setSlashHidden] = useState(false);
  const [mentionIndex, setMentionIndex] = useState(0);
  const [mentionHidden, setMentionHidden] = useState(false);
  const [slashIndex, setSlashIndex] = useState(-1);
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
  const terminalSnapshots = useRef(new TerminalSnapshots());
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
  /** The conversation whose transcript on screen was loaded live from the hub. */
  const liveTranscript = useRef<string | undefined>(undefined);
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
  currentOrchestrator.current[workspace] = orchestrator?.id;
  const target = destination
    ? view.threads.find((t) => t.id === destination)
    : orchestrator;
  const draftKey = workspace + ":" + (destination || "orchestrator");
  const draft = drafts[draftKey] || "";
  const selectedThread = view.threads.find((t) => t.id === selected);
  // The desktop tray/menu bar and Dock badge mirror what needs the user.
  useEffect(() => {
    window.twrApp?.setAttention({
      count: view.attention.length,
      items: view.attention.slice(0, 5).map((item) => ({
        thread: item.thread,
        title: (
          view.threads.find((t) => t.id === item.thread)?.name || item.thread
        ).slice(0, 200),
        summary: (item.summary || item.reason || "").slice(0, 300),
      })),
    });
  }, [view.attention, view.threads]);
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
  const infoRef = useRef(info);
  infoRef.current = info;
  const action = useCallback(
    (id: string, initialValues: Json = {}) => {
      const command = commands.find((c) => c.id === id);
      // Choosing the first orchestrator starts from the one the hub would start: its
      // runtime, and the runtime's default model unless one is picked.
      const o = infoRef.current?.orchestrator;
      if (id === "orch" && !initialValues.runtime && o && !o.thread) {
        const runtime = o.configured?.runtime || o.runtime;
        if (runtime) initialValues = { ...initialValues, runtime };
      }
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
  // What the model pickers offer (`orchestrator.show` `models`), once per hub: a list that
  // changes only with twr. An older hub has none, and its model fields stay free text.
  const [modelCatalog, setModelCatalog] = useState<Json>();
  useEffect(() => {
    setModelCatalog(undefined);
    if (!connected) return;
    let live = true;
    void rpc("orchestrator.show")
      .then((show) => {
        if (live) setModelCatalog(show?.models);
      })
      .catch(() => {});
    return () => {
      live = false;
    };
  }, [connected, workspace, rpc]);
  const refreshWorkspaces = useCallback(async () => {
    const value = await hostApi.cli(["ws", "list", "--json"]);
    if (value.code !== 0) throw new Error(value.stderr || value.stdout);
    const parsed = JSON.parse(value.stdout);
    setWorkspaces(Array.isArray(parsed) ? parsed : parsed.workspaces || []);
  }, []);
  const inboxGeneration = useRef(0);
  // `quiet`: the sidebar's background reads, which neither show progress nor report errors.
  const refreshInboxSources = useCallback(
    async (quiet = false) => {
      if (!hostApi.inboxSources) return;
      const generation = ++inboxGeneration.current;
      if (!quiet) setInboxRefreshing(true);
      try {
        const sources = await hostApi.inboxSources();
        if (generation === inboxGeneration.current)
          setInboxSources(
            sources.map((source) => ({
              ...source,
              updatedAt:
                source.updatedAt ||
                (!source.cached && !source.error && source.running !== false
                  ? Date.now()
                  : undefined),
            })),
          );
      } catch (error) {
        if (generation === inboxGeneration.current) {
          setInboxSources((old) =>
            old.map((source) => ({ ...source, cached: true })),
          );
          if (!quiet) fail(error);
        }
      } finally {
        if (generation === inboxGeneration.current) setInboxRefreshing(false);
      }
    },
    [hostApi, fail],
  );
  useEffect(() => {
    if (section === "attention" && connected) void refreshInboxSources();
  }, [section, connected, workspace, refreshInboxSources]);
  // The sidebar lists every workspace's threads, as the TUI's does: while it shows, the other
  // workspaces are re-read now and then over short-lived read-only connections.
  const sidebarReads = sidebar && connected && workspaces.length > 1;
  useEffect(() => {
    if (!sidebarReads) return;
    const read = () => {
      if (document.visibilityState !== "hidden") void refreshInboxSources(true);
    };
    read();
    const timer = setInterval(read, WORKSPACE_REFRESH_MS);
    document.addEventListener("visibilitychange", read);
    return () => {
      clearInterval(timer);
      document.removeEventListener("visibilitychange", read);
    };
  }, [sidebarReads, workspace, refreshInboxSources]);
  const openTerminal = useCallback(
    async (
      request: Parameters<typeof hostApi.terminalOpen>[0],
      throwOnFailure = false,
    ) => {
      try {
        terminalSnapshots.current.changed();
        const session = await hostApi.terminalOpen(request);
        terminalSnapshots.current.changed();
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
      setSearchOpen(false);
      inboxGeneration.current++;
      setInboxRefreshing(false);
      setInboxSources((old) =>
        old.map((source) => ({ ...source, cached: true })),
      );
      liveView.current = false;
      hasLiveSnapshot.current = false;
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
    let terminalsDirty = false;
    let stopped = false;
    const api = hostApi;
    const refreshTerminals = () => {
      if (stopped) return;
      if (refreshingTerminals) {
        terminalsDirty = true;
        return;
      }
      refreshingTerminals = true;
      const revision = terminalSnapshots.current.begin();
      void api
        .bootstrap()
        .then((boot) => {
          const sessions = terminalSnapshots.current.apply(
            revision,
            boot.terminals || [],
          );
          if (stopped) return;
          if (!sessions) {
            terminalsDirty = true;
            return;
          }
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
          if (terminalsDirty && !stopped) {
            terminalsDirty = false;
            refreshTerminals();
          }
        });
    };
    const unsubscribe = hostApi.onEvent((event) => {
      if (event.type === "resync" && event.scope !== "control")
        refreshTerminals();
      if (event.type === "resync" && event.scope !== "terminal")
        setTranscriptRefresh((n) => n + 1);
      if (event.type === "state" && event.state) {
        liveView.current = true;
        hasLiveSnapshot.current = true;
        setView(event.state);
        if (connectionRef.current) setConnected(true);
      }
      if (event.type === "connection") {
        const restored = event.connected && !connectionRef.current;
        connectionRef.current = !!event.connected;
        if (!event.connected) liveView.current = false;
        setConnected(!!event.connected && liveView.current);
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
      if (event.type === "terminal-exit") {
        terminalSnapshots.current.changed();
        setTerminals((old) =>
          old.map((t) => (t.id === event.id ? { ...t, exited: true } : t)),
        );
      }
      if (event.type === "terminal-removed") {
        if (event.id) terminalSnapshots.current.removed(event.id);
        setTerminals((old) => old.filter((t) => t.id !== event.id));
      }
      if (event.type === "menu-new") actionRef.current("new");
      if (event.type === "menu-settings") {
        setSection("settings");
        setSidebar(false);
      }
      if (event.type === "menu-attention") {
        setSection("attention");
        setSidebar(false);
        if (event.thread) setSelected(event.thread);
      }
      if (event.type === "menu-palette") {
        setPalette(true);
        setQuery("");
      }
    });
    const initialTerminalRevision = terminalSnapshots.current.begin();
    void hostApi
      .bootstrap()
      .then((boot) => {
        if (stopped) return;
        const id = boot.hostId || "local";
        setHostId(id);
        const saved = offlineJournal(hostApi, id);
        void saved
          .load()
          .then(async () => {
            // Migrate previous mobile message drafts into device-encrypted storage.
            const oldKey = `twr.mobile.drafts.${document.documentElement.dataset.hub || ""}`;
            let legacy: Record<string, string> = {};
            try {
              legacy = JSON.parse(localStorage.getItem(oldKey) || "{}");
            } catch {}
            const previous = Object.fromEntries(
              Object.entries(legacy || {}).filter(
                (entry): entry is [string, string] =>
                  typeof entry[1] === "string",
              ),
            );
            const restored = { ...previous, ...saved.snapshot.drafts };
            if (Object.keys(previous).length) {
              await saved.drafts(restored);
              localStorage.removeItem(oldKey);
            }
            setDrafts((current) => ({ ...restored, ...current }));
            setJournal(saved);
            setOfflineNotice(saved.historyError);
          })
          .catch((error) => setOfflineNotice(errorMessage(error)));
        setAllowedMethods(boot.allowed_methods);
        document.documentElement.dataset.platform = boot.platform;
        const sessions = terminalSnapshots.current.apply(
          initialTerminalRevision,
          boot.terminals || [],
        );
        if (sessions) {
          setTerminals(sessions);
          setActiveTerminal(sessions.at(-1)?.id);
        } else refreshTerminals();
        document.documentElement.dataset.theme = savedAppearance();
        applyOmarchyTheme(boot.theme);
        setHomePath(boot.home);
        return connect(boot.workspace).then(() => onReady?.());
      })
      .catch(fail);
    void hostApi.catalog().then(setCliCommands).catch(fail);
    return () => {
      stopped = true;
      unsubscribe();
      if (eventTimer.current) clearTimeout(eventTimer.current);
      eventTimer.current = undefined;
      pendingEvents.current = [];
    };
  }, []);
  useEffect(() => {
    if (!journal || connected || hasLiveSnapshot.current) return;
    const cached = journal.snapshot.workspaces[workspace];
    if (cached) {
      setView(cached.view);
      setEvents(
        cached.transcripts[
          target?.id ||
            cached.view.threads.find((t) => t.is_orchestrator)?.id ||
            ""
        ] || [],
      );
    }
  }, [journal, connected, workspace, target?.id]);
  const latestCache = useRef({
    view,
    events,
    thread: activeThread.current,
    transcriptLoading,
    transcriptError,
  });
  latestCache.current = {
    view,
    events,
    thread: activeThread.current,
    transcriptLoading,
    transcriptError,
  };
  useEffect(() => {
    if (!journal || !hasLiveSnapshot.current) return;
    let saved: typeof latestCache.current | undefined;
    const save = () => {
      const latest = latestCache.current;
      if (
        saved?.view === latest.view &&
        saved.events === latest.events &&
        saved.thread === latest.thread
      )
        return;
      void journal
        .cache(
          workspace,
          latest.view,
          !latest.transcriptLoading && !latest.transcriptError
            ? latest.thread
            : undefined,
          latest.events,
        )
        .then(() => {
          saved = latest;
        })
        .catch((error) => setOfflineNotice(errorMessage(error)));
    };
    // Flush on loss and throttle streaming history instead of postponing it forever.
    save();
    if (!connected) return;
    const timer = setInterval(save, 5000);
    return () => clearInterval(timer);
  }, [journal, workspace, connected]);
  useEffect(() => {
    const media = window.matchMedia("(max-width:800px)");
    const change = () => {
      setNarrow(media.matches);
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
    const previousThread = activeThread.current;
    activeThread.current = id;
    const generation = ++transcriptGeneration.current;
    const buffered = pendingEvents.current.splice(0);
    const shownEvents = latestCache.current.events;
    // Only a change of conversation clears the transcript.
    const kept =
      connected &&
      id &&
      previousThread === id &&
      liveTranscript.current === id &&
      shownEvents.length
        ? mergeEvents(shownEvents, buffered)
        : undefined;
    // A disconnect keeps the live transcript on screen; its reconnect catches up.
    if (previousThread !== id || !hasLiveSnapshot.current)
      liveTranscript.current = undefined;
    setEvents((previous) =>
      kept
        ? mergeEvents(previous, buffered)
        : !connected && hasLiveSnapshot.current && previousThread === id
          ? mergeEvents(previous, buffered)
          : !connected && journal && id
            ? journal.snapshot.workspaces[workspace]?.transcripts[id] || []
            : [],
    );
    setTranscriptError("");
    if (!kept) setEarliestTurn(undefined);
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
      // The same conversation again (a reconnect, a resync, a retry after a transcript
      // error): keep what is on screen and catch up from its last event, so the view,
      // its older turns and its scroll position stay. Far behind, reload it instead.
      if (kept?.length) {
        const last = kept[kept.length - 1].seq;
        let page = await rpc("thread.transcript", {
          thread: id,
          since_seq: last,
          limit: 500,
        });
        const fresh: HubEvent[] = [...page.events];
        for (let pages = 1; page.more && pages < 8; pages++) {
          page = await rpc("thread.transcript", {
            thread: id,
            since_seq: page.next,
            limit: 500,
          });
          if (generation !== transcriptGeneration.current) return;
          fresh.push(...page.events);
        }
        if (generation !== transcriptGeneration.current) return;
        if (!page.more) {
          setEvents((old) => mergeEvents(old, fresh));
          liveTranscript.current = id;
          const subscription = String(generation);
          activeSubscription.current = subscription;
          await rpc("desktop.subscribe", {
            thread: id,
            since: page.next || last,
            subscription,
          });
          return;
        }
        setEarliestTurn(undefined);
      }
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
      liveTranscript.current = id;
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
      if (
        mod &&
        event.key.toLowerCase() === "f" &&
        !event.shiftKey &&
        !(event.target as HTMLElement)?.closest(".terminal-surface")
      ) {
        event.preventDefault();
        if (connectionRef.current && liveView.current) setSearchOpen(true);
        return;
      }
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
  useEffect(() => prepareAnnouncer(), []);
  // Screen-reader announcements. The transcript itself is not a live region, so
  // streaming output never floods the reader; only state changes are spoken.
  const connectionAnnounced = useRef("");
  useEffect(() => {
    if (connecting) return;
    const key = workspace + ":" + connected;
    const previous = connectionAnnounced.current;
    if (key === previous) return;
    connectionAnnounced.current = key;
    if (connected) announce(`Connected to ${workspace}`);
    else if (previous)
      announce(
        `Disconnected from ${workspace}${connectionError ? ". " + connectionError : ""}`,
      );
    else if (connectionError)
      announce(`Could not connect to ${workspace}. ${connectionError}`);
  }, [connected, connecting, workspace]);
  const knownAttention = useRef<{ workspace: string; ids: Set<string> }>(
    undefined,
  );
  useEffect(() => {
    if (!connected) return;
    const ids = new Set(view.attention.map((item) => item.attn_id));
    const known = knownAttention.current;
    knownAttention.current = { workspace, ids };
    if (!known || known.workspace !== workspace) return;
    const fresh = view.attention.filter((item) => !known.ids.has(item.attn_id));
    for (const item of fresh.slice(0, 3))
      announce(
        attentionAnnouncement({
          reason: item.reason,
          summary: item.summary,
          threadName: view.threads.find((t) => t.id === item.thread)?.name,
        }),
      );
    if (fresh.length > 3)
      announce(`${fresh.length - 3} more requests need you`);
  }, [view.attention, connected, workspace]);
  const turnBaseline = useRef<{ thread?: string; end?: string }>({});
  useEffect(() => {
    const end = events.findLast((event) => event.kind === "turn.ended");
    const id = end ? end.id || String(end.seq) : undefined;
    const baseline = turnBaseline.current;
    turnBaseline.current = { thread: target?.id, end: id };
    if (
      transcriptLoading ||
      !end ||
      baseline.thread !== target?.id ||
      baseline.end === id
    )
      return;
    announce(
      turnAnnouncement(
        destination ? target?.name || "Thread" : "Orchestrator",
        end.data?.result,
      ),
    );
  }, [events, transcriptLoading, target?.id]);
  useEffect(() => {
    if (result !== undefined && operation) announce(`${operation.title}: done`);
  }, [result]);
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
    if (!permitted("operate") || !draft.trim() || sending) return;
    if (
      !connected &&
      (!journal || draft.startsWith("/") || draft.startsWith("!"))
    )
      return;
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
      } else {
        const method = destination ? "thread.send" : "say";
        const params = destination
          ? { thread: destination, prompt: text }
          : {
              message: text,
              seen_gseq: view.head,
              to: "orchestrator",
              suggested:
                acceptedSuggestion?.key === key &&
                acceptedSuggestion.text === text,
            };
        // Live replies to a waiting request use only the current visible head.
        // They never enter the delayed message outbox or receive automatic retries.
        const liveReply =
          !destination &&
          connected &&
          liveView.current &&
          view.attention.some(
            (item) =>
              item.reason === "permission" || item.reason === "question",
          );
        if (journal && hostApi.sendMessage && !liveReply) {
          const pending = { key, text: draft, saved: false };
          pendingEnqueue.current = pending;
          const epoch = journal.epoch;
          clearTimeout(draftSaveTimer.current);
          draftSaveTimer.current = undefined;
          const message = await journal
            .enqueue(workspace, method, params, draft)
            .catch((error) => {
              if (pendingEnqueue.current === pending)
                pendingEnqueue.current = undefined;
              void journal.drafts(currentDrafts.current, epoch).catch(() => {});
              throw error;
            });
          pending.saved = true;
          setDrafts((old) => ({
            ...old,
            [key]: old[key] === draft ? "" : old[key],
          }));
          if (connected) response = await journal.deliver(message);
          else response = { saved_offline: true };
        } else response = await rpc(method, params);
      }

      for (const restored of response.restored_drafts || [])
        setDrafts((old) => ({
          ...old,
          [restored.key]: appendDraft(old[restored.key], restored.text),
        }));
      if (response.action === "pending") {
        setDrafts((old) => ({
          ...old,
          [key]: appendDraft(old[key], draft),
        }));
        setSection("attention");
        throw new Error(
          "Message not sent. Review the waiting requests and answer the exact item shown.",
        );
      }
      setDrafts((old) => ({
        ...old,
        [key]: old[key] === draft ? "" : old[key],
      }));
      if (!response.saved_offline && target)
        uploads.dismissReady(target.id, workspace, draft);
      announce(
        response.saved_offline
          ? "Message saved on this device. It sends when the hub reconnects."
          : text.startsWith("!") && !text.startsWith("!!!")
            ? "Shell command started"
            : "Message sent",
      );
      setQueueRefresh((n) => n + 1);
    } catch (e) {
      fail(e);
    } finally {
      setSending(false);
      input.current?.focus();
    }
  };
  // Voice mode (shared/voice.ts): a transcript goes to the orchestrator as it was heard,
  // marked spoken, so the hub never takes it for an answer or a mode change. Sent at once,
  // not through the offline outbox: it is said now, about what is on screen now.
  const submitVoice = async (text: string): Promise<Json> => {
    if (!orchestrator)
      throw new Error("Choose an orchestrator before talking to it.");
    const response = await rpc("say", {
      message: text,
      seen_gseq: view.head,
      to: "orchestrator",
      voice: true,
    });
    if (response.action === "pending") {
      setSection("attention");
      throw new Error(
        "Not sent: review the waiting requests and answer the exact item shown.",
      );
    }
    setQueueRefresh((n) => n + 1);
    return response;
  };
  const voiceHintWords = useMemo(() => voiceHint(view.threads), [view.threads]);
  // What this device may have read aloud lately, for voice mode's echo check: the
  // orchestrator's last messages (kept while a worker's conversation is on screen) and the
  // sentences a speaker announces.
  const orchestratorSaid = useRef<string[]>([]);
  useEffect(() => {
    if (destination || !orchestrator) return;
    orchestratorSaid.current = blocksOf(events)
      .filter((b) => b.kind === "assistant")
      .slice(-3)
      .map((b) => b.text);
  }, [events, destination, orchestrator?.id]);
  const spokenLately = () => [
    ...orchestratorSaid.current,
    ...announcements(view.threads),
  ];
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
        case "Runner":
          setSection("boxes");
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
  // While a workspace connects, its entry keeps the rows it had (a double-click's second
  // click lands on the row the first one chose).
  const entries = workspaceEntries(
    workspaces,
    workspace,
    hostId,
    inboxSources,
    connecting
      ? undefined
      : { threads: view.threads, attention: view.attention },
  );
  const toggleCollapsed = (name: string) =>
    setCollapsed((old) => {
      const next = old.includes(name)
        ? old.filter((n) => n !== name)
        : [...old, name];
      try {
        localStorage.setItem("twr.sidebar.collapsed", JSON.stringify(next));
      } catch {}
      return next;
    });
  /**
   * Opens a workspace's orchestrator (no `thread`) or one of its threads from the sidebar,
   * switching to the workspace first when it is another one. `talk` makes the thread the
   * message destination (a double-click); otherwise it is selected, opening its inspector.
   * Clicks while the switch connects update the pending request instead of switching again.
   * `then` runs once it is open (a new thread's form, the grid).
   */
  const openIn = (
    name: string,
    thread?: string,
    talk = false,
    then?: () => void,
  ) => {
    const show = (
      id: string | undefined,
      toThread: boolean,
      after?: () => void,
    ) => {
      setSection("chat");
      if (!id) {
        setDestination(undefined);
        setSelected(undefined);
      } else if (toThread) {
        setDestination(id);
        setSelected(undefined);
      } else setSelected(id);
      after?.();
    };
    // `connect` names the new workspace at once: until it finishes, clicks there update
    // what opens once it has.
    const pending = pendingOpen.current;
    if (pending?.workspace === name) {
      pendingOpen.current = {
        workspace: name,
        thread,
        talk: pending.thread === thread ? pending.talk || talk : talk,
        then,
      };
      return;
    }
    if (name === workspace) return show(thread, talk, then);
    pendingOpen.current = { workspace: name, thread, talk, then };
    const switched = connect(name);
    // A later switch (here, the workspaces dialog, `/ws`) supersedes this one.
    const generation = connectGeneration.current;
    void switched.then(() => {
      const wanted = pendingOpen.current;
      if (wanted?.workspace !== name) return;
      pendingOpen.current = undefined;
      if (generation === connectGeneration.current)
        show(wanted.thread, wanted.talk, wanted.then);
    });
  };
  // Every attention answer (conversation cards, Needs you, the inspector, mobile) goes
  // through here: only a live hub view, never a cached one, can authorize it.
  const liveAnswerCheck = useCallback(() => {
    if (!connectionRef.current || !liveView.current)
      throw new Error(
        "Not connected to the live hub. Reconnect and review the current request.",
      );
    if (deviceScopes && !deviceScopes.includes("operate"))
      throw new Error("This device has read-only access.");
  }, [deviceScopes]);
  const respond = useCallback(
    async (item: Attention, answer: string, message?: string) => {
      liveAnswerCheck();
      await rpc("attention.respond", {
        attn_id: item.attn_id,
        revision: item.revision,
        answer,
        ...(message ? { message } : {}),
      });
    },
    [liveAnswerCheck, rpc],
  );
  const dismissAttention = useCallback(
    async (item: Attention) => {
      liveAnswerCheck();
      await rpc("attention.dismiss", {
        attn_id: item.attn_id,
        revision: item.revision,
      });
    },
    [liveAnswerCheck, rpc],
  );
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
        type="button"
        className="skip-link"
        onClick={() => {
          const composer = input.current;
          if (composer && !composer.disabled && section === "chat")
            composer.focus();
          else document.getElementById("twr-main")?.focus();
        }}
      >
        Skip to {section === "chat" ? "message composer" : "main content"}
      </button>
      <button
        className="sidebar-backdrop"
        aria-label="Close navigation"
        tabIndex={-1}
        onClick={() => setSidebar(false)}
        style={{ display: sidebar ? undefined : "none" }}
      />
      <aside
        className="sidebar"
        id="twr-sidebar"
        ref={sidebarRef}
        aria-label={sidebarModal ? "Navigation" : "Sidebar"}
        role={sidebarModal ? "dialog" : undefined}
        aria-modal={sidebarModal ? true : undefined}
        onClick={(event) => {
          if (
            window.innerWidth <= 800 &&
            (event.target as HTMLElement).closest(
              ".primary-nav button,.thread-card,.workspace-orchestrator,.workspace-thread,.sidebar-secondary button,.sidebar-footer button",
            )
          )
            setSidebar(false);
        }}
      >
        <PaneResize pane="sidebar" />
        <div className="brand drag-region">
          <span className="wordmark">
            twr<span>_</span>
          </span>
          <span className="brand-caption">Hub client</span>
          <button
            className="icon-button no-drag"
            aria-label="Hide sidebar"
            aria-controls="twr-sidebar"
            aria-expanded={true}
            title="Hide sidebar"
            onClick={() => setSidebar(false)}
          >
            <PanelLeftClose size={16} />
          </button>
        </div>
        <nav className="primary-nav" aria-label="Primary">
          <button
            className={section === "attention" ? "active" : ""}
            aria-current={section === "attention" ? "page" : undefined}
            onClick={() => setSection("attention")}
          >
            <Bell size={17} />
            Needs you
            {view.attention.length > 0 && (
              <>
                {" "}
                <span className="count attention-count">
                  {view.attention.length}
                </span>
              </>
            )}
          </button>
          <button
            className={section === "grid" ? "active" : ""}
            aria-current={section === "grid" ? "page" : undefined}
            onClick={() => setSection("grid")}
          >
            <LayoutGrid size={17} />
            Thread grid <span className="count">{workers.length}</span>
          </button>
        </nav>
        <div className="threads-heading">
          <span role="heading" aria-level={2} id="twr-workspaces-heading">
            Workspaces
          </span>
          <div>
            <button
              className={`icon-button ${focus ? "on" : ""}`}
              title="Only active threads"
              aria-label="Toggle focus filter"
              aria-pressed={focus}
              onClick={() => setFocus((v) => !v)}
            >
              <Filter size={13} />
            </button>
            <button
              className="icon-button"
              aria-label="Manage workspaces"
              title="New or remove workspaces"
              aria-haspopup="dialog"
              onClick={() => setWorkspaceModal(true)}
            >
              <FolderPlus size={15} />
            </button>
          </div>
        </div>
        <div
          className="workspace-tree"
          aria-labelledby="twr-workspaces-heading"
          role="group"
        >
          {entries.map((entry) => {
            const shut = collapsed.includes(entry.name);
            const headingId = `twr-ws-${entry.name}`;
            const status =
              entry.running === false
                ? "hub not running"
                : [
                    entry.waiting ? `${entry.waiting} waiting on you` : "",
                    entry.working ? `${entry.working} working` : "",
                  ]
                    .filter(Boolean)
                    .join(", ") || (entry.stale ? "not refreshed" : "idle");
            const talkingToOrchestrator =
              entry.current && section === "chat" && !destination;
            return (
              <section
                key={entry.name}
                className={`workspace-entry ${entry.current ? "current" : ""} ${entry.running === false ? "stopped" : ""}`}
                data-workspace={entry.name}
                aria-labelledby={headingId}
              >
                <div className="workspace-entry-head">
                  <button
                    className="workspace-toggle"
                    id={headingId}
                    aria-expanded={!shut}
                    aria-current={entry.current ? "true" : undefined}
                    title={`${entry.name}: ${status}`}
                    onClick={() => toggleCollapsed(entry.name)}
                  >
                    {shut ? (
                      <ChevronRight size={14} aria-hidden="true" />
                    ) : (
                      <ChevronDown size={14} aria-hidden="true" />
                    )}
                    <span className="workspace-name">{entry.name}</span>
                    <span className="visually-hidden">, {status}</span>
                    <span className="workspace-badges" aria-hidden="true">
                      {entry.running === false ? (
                        <span className="workspace-stopped">○</span>
                      ) : (
                        <>
                          {entry.waiting > 0 && (
                            <span className="workspace-waiting">
                              ⚠{entry.waiting}
                            </span>
                          )}
                          {entry.working > 0 && (
                            <span className="workspace-working">
                              ●{entry.working}
                            </span>
                          )}
                        </>
                      )}
                    </span>
                  </button>
                  <button
                    className="icon-button"
                    aria-label={
                      entry.current
                        ? "New thread"
                        : `New thread in ${entry.name}`
                    }
                    title={`New thread in ${entry.name}`}
                    disabled={!permitted("operate")}
                    onClick={() =>
                      openIn(entry.name, undefined, false, () =>
                        actionRef.current("new"),
                      )
                    }
                  >
                    <Plus size={15} />
                  </button>
                </div>
                {!shut && (
                  <>
                    <button
                      className={`workspace-orchestrator ${talkingToOrchestrator ? "active" : ""}`}
                      aria-current={talkingToOrchestrator ? "page" : undefined}
                      onClick={() => openIn(entry.name)}
                    >
                      <MessageSquare size={15} />
                      Orchestrator
                    </button>
                    {entry.current && !connecting ? (
                      <>
                        <div
                          className="thread-list"
                          role="list"
                          aria-label={`Threads in ${entry.name}`}
                          onMouseEnter={() => setHovering(true)}
                          onMouseLeave={() => setHovering(false)}
                        >
                          {visible.map((thread) => (
                            <ThreadCard
                              listItem
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
                            <p className="sidebar-empty" role="listitem">
                              {connecting
                                ? "Connecting…"
                                : workers.length
                                  ? "No threads match your filters."
                                  : "Threads appear here as your agents start working."}
                            </p>
                          )}
                        </div>
                        {workers.some((t) => t.status === "parked") && (
                          <button
                            className="parked-toggle"
                            aria-pressed={parked}
                            onClick={() => setParked((v) => !v)}
                          >
                            {parked
                              ? "Hide parked"
                              : `${workers.filter((t) => t.status === "parked").length} parked threads hidden`}
                          </button>
                        )}
                      </>
                    ) : (
                      <ul
                        className="workspace-peek"
                        aria-label={`Threads in ${entry.name}`}
                      >
                        {entry.threads.map((thread) => (
                          <li key={thread.id}>
                            <button
                              className="workspace-thread"
                              style={
                                {
                                  "--thread-color": thread.accent,
                                } as CSSProperties
                              }
                              title={`${thread.name} in ${entry.name}: ${thread.status_label || thread.status}. Double-click to talk to it.`}
                              onClick={(event) =>
                                openIn(entry.name, thread.id, event.detail > 1)
                              }
                            >
                              {thread.accent && (
                                <span
                                  className="thread-color-dot"
                                  aria-hidden="true"
                                />
                              )}
                              <span className="workspace-thread-name">
                                {thread.name}
                              </span>
                              <span className="visually-hidden">, </span>
                              <span
                                className={`thread-status status-${thread.status}`}
                              >
                                {thread.status_label || thread.status}
                              </span>
                            </button>
                          </li>
                        ))}
                        {entry.more > 0 && (
                          <li>
                            <button
                              className="workspace-thread workspace-more"
                              onClick={() =>
                                openIn(entry.name, undefined, false, () =>
                                  setSection("grid"),
                                )
                              }
                            >
                              +{entry.more} more
                            </button>
                          </li>
                        )}
                      </ul>
                    )}
                  </>
                )}
              </section>
            );
          })}
          <button
            className="workspace-add"
            aria-haspopup="dialog"
            onClick={() => setWorkspaceModal(true)}
          >
            <Plus size={14} />
            New workspace
          </button>
        </div>
        <details className="sidebar-secondary manage-nav">
          <summary>Manage</summary>
          <nav aria-label="Manage">
            {visibleNav
              .filter((n) => n.id !== "settings")
              .map((n) => (
                <button
                  key={n.id}
                  aria-current={section === n.id ? "page" : undefined}
                  onClick={() => setSection(n.id)}
                >
                  <n.icon size={15} />
                  {n.label}
                </button>
              ))}
            <button
              aria-current={section === "decisions" ? "page" : undefined}
              onClick={() => setSection("decisions")}
            >
              <CheckCircle2 size={15} />
              Decisions
            </button>
          </nav>
        </details>
        <div className="sidebar-footer">
          <button
            aria-haspopup="dialog"
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
            aria-current={section === "settings" ? "page" : undefined}
            onClick={() => setSection("settings")}
          >
            <Settings2 size={15} />
            Settings
          </button>
          <div className="connection-status">
            <span
              className={`status-dot ${connected ? "online" : ""}`}
              aria-hidden="true"
            />
            {connected
              ? "Hub connected"
              : connecting
                ? "Connecting…"
                : "Disconnected"}
            <span>v{paths?.version || "0.3.0"}</span>
          </div>
        </div>
      </aside>
      <main className="main-shell" id="twr-main" tabIndex={-1}>
        {!connected && journal?.snapshot.workspaces[workspace] && (
          <div className="offline-banner" role="status">
            Offline · saved{" "}
            {new Date(
              journal.snapshot.workspaces[workspace].savedAt,
            ).toLocaleString()}
            . Requests need a live connection before you can answer.
          </div>
        )}
        {offlineNotice && (
          <div className="offline-banner" role="status">
            {offlineNotice}
          </div>
        )}
        <header className="topbar drag-region">
          <div className="topbar-left no-drag">
            {!sidebar && (
              <button
                className="icon-button"
                aria-label="Show sidebar"
                aria-controls="twr-sidebar"
                aria-expanded={false}
                title="Show sidebar"
                onClick={() => setSidebar(true)}
              >
                <PanelLeftOpen size={18} />
              </button>
            )}
            {destination && section === "chat" && (
              <button
                className="icon-button mobile-back-button"
                aria-label="Return to orchestrator"
                title="Return to orchestrator"
                onClick={() => setDestination(undefined)}
              >
                <ArrowLeft size={18} />
              </button>
            )}
            <span className="workspace-breadcrumb">{workspace}</span>
            <ChevronRight size={13} />
            <span className="visually-hidden">,</span>
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
                role="img"
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
              aria-label="Search conversations"
              title="Search conversations (⌘/Ctrl F)"
              disabled={!connected}
              onClick={() => setSearchOpen(true)}
            >
              <Search size={17} />
            </button>
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
              title="Commands (⌘/Ctrl K)"
              aria-haspopup="dialog"
              onClick={() => {
                setPalette(true);
                setQuery("");
              }}
            >
              <CommandIcon size={17} />
            </button>
            <details
              className="conversation-menu"
              onToggle={(event) => {
                const menu = event.currentTarget;
                // Keyboard users land on the first action; pointer users keep focus.
                if (menu.open && menu.dataset.keyboard === "true")
                  menu
                    .querySelector<HTMLElement>("div > button:not(:disabled)")
                    ?.focus();
                delete menu.dataset.keyboard;
              }}
            >
              <summary
                aria-label={
                  section === "chat" ? "Thread actions" : "More actions"
                }
                title={section === "chat" ? "Thread actions" : "More actions"}
                onKeyDown={(event) => {
                  const menu = event.currentTarget
                    .parentElement as HTMLDetailsElement;
                  if (["Enter", " "].includes(event.key))
                    menu.dataset.keyboard = "true";
                  if (event.key === "ArrowDown") {
                    event.preventDefault();
                    menu.dataset.keyboard = "true";
                    if (!menu.open) menu.open = true;
                    else
                      menu
                        .querySelector<HTMLElement>(
                          "div > button:not(:disabled)",
                        )
                        ?.focus();
                  }
                }}
              >
                <MoreHorizontal size={18} />
              </summary>
              <div
                onKeyDown={(event) =>
                  handleRovingKey(event, event.currentTarget, ":scope > button")
                }
                onClick={(event) =>
                  (
                    event.currentTarget.parentElement as HTMLDetailsElement
                  ).removeAttribute("open")
                }
              >
                <button
                  aria-controls="twr-sidebar"
                  onClick={() => setSidebar(true)}
                >
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
                  disabled={!permitted("terminal")}
                  onClick={() =>
                    void openTerminal({
                      shell: true,
                      cwd: localWorkingDirectory(target, homePath) || undefined,
                    })
                  }
                >
                  <TerminalSquare size={16} />
                  Open shell
                </button>
                {section === "chat" && (
                  <>
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
                      {destination
                        ? "Thread settings"
                        : "Orchestrator settings"}
                    </button>
                  </>
                )}
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
            <button
              className="topbar-new-thread"
              aria-label="New thread"
              title="New thread"
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
            aria-busy={connecting || undefined}
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
                        title="Back to orchestrator"
                        onClick={() => setDestination(undefined)}
                      >
                        <ArrowLeft size={17} />
                      </button>
                    )}
                    <span
                      className={`destination-symbol ${destination ? "worker" : ""}`}
                      aria-hidden="true"
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
                      aria-pressed={showTools}
                      title="Show tool calls"
                      onClick={() => setShowTools((v) => !v)}
                    >
                      <Wrench size={16} />
                    </button>
                    <button
                      className={`icon-button ${showThinking ? "on" : ""}`}
                      aria-label="Toggle reasoning"
                      aria-pressed={showThinking}
                      title="Show reasoning"
                      onClick={() => setShowThinking((v) => !v)}
                    >
                      <Eye size={16} />
                    </button>
                    {target && (
                      <button
                        className="icon-button"
                        aria-label="Export conversation"
                        title="Export conversation"
                        onClick={() => void exportConversation()}
                      >
                        <Download size={16} />
                      </button>
                    )}
                    <button
                      className="icon-button"
                      aria-label={
                        destination
                          ? "Thread settings"
                          : "Orchestrator settings"
                      }
                      title={
                        destination
                          ? "Thread settings"
                          : "Orchestrator settings"
                      }
                      aria-haspopup="dialog"
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
                <div
                  className="conversation-attention"
                  role={
                    view.attention.some(
                      (a) => a.thread === "hub" || a.thread === target?.id,
                    )
                      ? "region"
                      : undefined
                  }
                  aria-label="Requests for this conversation"
                >
                  {view.attention
                    .filter(
                      (a) => a.thread === "hub" || a.thread === target?.id,
                    )
                    .map((item) => (
                      <AttentionCard
                        key={item.attn_id + ":" + item.revision}
                        item={item}
                        thread={view.threads.find((t) => t.id === item.thread)}
                        readOnly={!connected || !permitted("operate")}
                        onRespond={respond}
                        onDismiss={dismissAttention}
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
                  loading={transcriptLoading && !presentedEvents.length}
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
                        <button
                          key={t.id}
                          aria-controls="twr-inspector"
                          onClick={() => setSelected(t.id)}
                        >
                          <span style={{ color: t.accent }}>{t.name}</span>
                          <span className="visually-hidden">: </span>
                          <span>{t.last_report.summary}</span>
                          <ArrowUpRight size={14} />
                        </button>
                      ))}
                  </details>
                )}
                <Outbox
                  journal={journal}
                  workspace={workspace}
                  connected={connected}
                  onSent={(result) => {
                    setQueueRefresh((n) => n + 1);
                    for (const restored of [
                      ...(result.restored_drafts || []),
                      ...(result.restored_draft ? [result.restored_draft] : []),
                    ])
                      setDrafts((old) => ({
                        ...old,
                        [restored.key]: appendDraft(
                          old[restored.key],
                          restored.text,
                        ),
                      }));
                  }}
                  onCopy={(message) => {
                    const key =
                      message.workspace +
                      ":" +
                      (message.method === "say"
                        ? "orchestrator"
                        : message.params.thread);
                    const text = messageDraft(message);
                    setDrafts((old) => ({
                      ...old,
                      [key]: appendDraft(old[key], text),
                    }));
                  }}
                />
                {target && (
                  <AttachmentManager
                    controller={uploads}
                    thread={target.id}
                    workspace={workspace}
                  />
                )}
                {queued.length > 0 && (
                  <section className="queue" aria-label="Queued messages">
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
                  </section>
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
                        aria-label={`Hide shell command ${job.command}`}
                        title="Hide"
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
                      title="Dismiss"
                    >
                      <X size={12} />
                    </button>
                  </div>
                )}
                {!draft &&
                  assistance.suggestion &&
                  document.documentElement.dataset.mobile === "true" && (
                    <button
                      className="suggested-message"
                      onClick={acceptSuggestion}
                    >
                      <span className="visually-hidden">Suggested reply: </span>
                      <span>{assistance.suggestion}</span>
                      <span className="visually-hidden">, tap to use</span>
                    </button>
                  )}
                <form
                  className={`composer ${draft.startsWith("!") && !draft.startsWith("!!!") ? "shell-mode" : ""}`}
                  aria-label="Message composer"
                  onSubmit={(event) => void send(event)}
                >
                  <div className="composer-label">
                    <span className="destination-chip">
                      <span className="destination-dot" aria-hidden="true" />
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
                          title="Return to orchestrator"
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
                    aria-describedby="twr-composer-hints"
                    aria-autocomplete={
                      mentions.length || slashMatches.length
                        ? "list"
                        : undefined
                    }
                    aria-controls={
                      mentions.length && !mentionHidden
                        ? "twr-mentions"
                        : slashMatches.length && !slashHidden
                          ? "twr-slash-suggestions"
                          : undefined
                    }
                    aria-activedescendant={
                      mentions.length && !mentionHidden
                        ? `twr-mention-${mentionIndex % mentions.length}`
                        : slashMatches.length && !slashHidden && slashIndex >= 0
                          ? `twr-slash-${slashIndex % slashMatches.length}`
                          : undefined
                    }
                    value={draft}
                    onChange={(e) => {
                      setAcceptedSuggestion(undefined);
                      setDrafts((old) => ({
                        ...old,
                        [draftKey]: e.target.value,
                      }));
                      setSlashHidden(false);
                      setSlashIndex(-1);
                      setMentionHidden(false);
                      setMentionIndex(0);
                    }}
                    placeholder={
                      // A suggestion shows as the empty box's placeholder, as the TUI's
                      // ghost text: Tab or → takes it, typing types over it.
                      !draft && assistance.suggestion
                        ? assistance.suggestion
                        : document.documentElement.dataset.mobile === "true"
                          ? "Message…"
                          : destination
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
                    disabled={(!connected && !journal) || !permitted("operate")}
                    onKeyDown={(e) => {
                      if (e.nativeEvent.isComposing || e.keyCode === 229)
                        return;
                      if (
                        takesSuggestion(
                          e,
                          draft,
                          assistance.suggestion,
                          assistance.suggestionShownFor(),
                        )
                      ) {
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
                        setSlashIndex(-1);
                        return;
                      }
                      if (
                        slashMatches.length &&
                        !slashHidden &&
                        !draft.includes("\n") &&
                        (e.key === "ArrowDown" || e.key === "ArrowUp")
                      ) {
                        // Arrow keys pick a suggested command; Enter then runs it.
                        e.preventDefault();
                        setSlashIndex((i) =>
                          e.key === "ArrowDown"
                            ? (i + 1) % slashMatches.length
                            : i <= 0
                              ? slashMatches.length - 1
                              : i - 1,
                        );
                        return;
                      }
                      if (
                        e.key === "Enter" &&
                        !e.shiftKey &&
                        slashMatches.length &&
                        !slashHidden &&
                        slashIndex >= 0
                      ) {
                        e.preventDefault();
                        const command =
                          slashMatches[slashIndex % slashMatches.length];
                        setSlashIndex(-1);
                        void submitSlash(
                          "/" + command.id + draft.slice(1).replace(/^\S+/, ""),
                        );
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
                          onChange={(event) => {
                            const files = Array.from(event.target.files || []);
                            event.target.value = "";
                            if (target) {
                              try {
                                uploads.add(files, target.id, workspace);
                              } catch (error) {
                                fail(error);
                              }
                            }
                          }}
                        />
                      </label>
                    )}
                    <span id="twr-composer-hints">
                      <kbd>/</kbd> commands <span>·</span> <kbd>@</kbd>{" "}
                      reference a thread <span>·</span> Shift Enter for a new
                      line
                      {!draft && assistance.suggestion && (
                        <span>
                          {" "}
                          · <kbd>Tab</kbd> or <kbd>→</kbd> takes the suggestion
                        </span>
                      )}
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
                    <div className="composer-actions">
                      <VoiceControl
                        host={hostApi}
                        rpc={rpc}
                        connected={connected}
                        allowed={permitted("operate") && Boolean(orchestrator)}
                        submit={submitVoice}
                        hint={voiceHintWords}
                        spoken={spokenLately}
                        edit={(text) => {
                          setDestination(undefined);
                          setDrafts((old) => ({
                            ...old,
                            [workspace + ":orchestrator"]: appendDraft(
                              old[workspace + ":orchestrator"],
                              text,
                            ),
                          }));
                          input.current?.focus();
                        }}
                        onError={fail}
                      />
                      <button
                        className="send-button"
                        type="submit"
                        disabled={
                          !permitted("operate") ||
                          !draft.trim() ||
                          (!connected &&
                            (!journal ||
                              draft.startsWith("/") ||
                              draft.startsWith("!"))) ||
                          (!target && !draft.startsWith("/")) ||
                          sending
                        }
                        aria-label={
                          connected ? "Send message" : "Save message for later"
                        }
                        title={
                          connected
                            ? "Send message"
                            : "Save on this device until the hub reconnects"
                        }
                      >
                        {sending ? <Spinner /> : <ArrowUp size={18} />}
                      </button>
                    </div>
                  </div>
                  {mentions.length > 0 && !mentionHidden && (
                    <div
                      className="mentions"
                      id="twr-mentions"
                      role="listbox"
                      aria-label="Threads to reference"
                    >
                      {mentions.map((t, i) => (
                        <button
                          className={
                            i === mentionIndex % mentions.length ? "active" : ""
                          }
                          type="button"
                          role="option"
                          id={`twr-mention-${i}`}
                          aria-selected={i === mentionIndex % mentions.length}
                          tabIndex={-1}
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
                    <div
                      className="mentions slash-suggestions"
                      id="twr-slash-suggestions"
                      role="listbox"
                      aria-label="Commands"
                    >
                      {slashMatches.map((command, i) => (
                        <button
                          type="button"
                          role="option"
                          id={`twr-slash-${i}`}
                          className={
                            slashIndex >= 0 &&
                            i === slashIndex % slashMatches.length
                              ? "active"
                              : ""
                          }
                          aria-selected={
                            slashIndex >= 0 &&
                            i === slashIndex % slashMatches.length
                          }
                          tabIndex={-1}
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
                          <span className="visually-hidden">, </span>
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
                <div
                  className="manager-actions"
                  role="group"
                  aria-label="Thread filters"
                >
                  <button
                    className={focus ? "active" : ""}
                    aria-pressed={focus}
                    onClick={() => setFocus((v) => !v)}
                  >
                    <Filter size={14} />
                    Focus {focus ? "on" : "off"}
                  </button>
                  <button
                    aria-pressed={parked}
                    onClick={() => setParked((v) => !v)}
                  >
                    {parked ? "Hide" : "Show"} parked
                  </button>
                </div>
                <div className="thread-grid" role="list" aria-label="Threads">
                  {visible.map((t) => (
                    <ThreadCard
                      listItem
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
              <NeedsYouInbox
                source={{
                  hubId: hostId,
                  cached: !connected,
                  provisional:
                    !connected &&
                    !hasLiveSnapshot.current &&
                    !journal?.snapshot.workspaces[workspace],
                  updatedAt: !connected
                    ? journal?.snapshot.workspaces[workspace]?.savedAt
                    : undefined,
                  hubName:
                    hostId === "local"
                      ? "This computer"
                      : String(
                          inboxSources.find((source) => source.hubId === hostId)
                            ?.hubName ||
                            info?.hostname ||
                            hostId,
                        ),
                  workspace,
                  attention:
                    !connected && !hasLiveSnapshot.current
                      ? journal?.snapshot.workspaces[workspace]?.view
                          .attention || view.attention
                      : view.attention,
                  threads:
                    !connected && !hasLiveSnapshot.current
                      ? journal?.snapshot.workspaces[workspace]?.view.threads ||
                        view.threads
                      : view.threads,
                }}
                sources={inboxSources}
                refreshing={inboxRefreshing}
                onRefresh={() => void refreshInboxSources()}
                onOpenSource={(source) => {
                  if (source.hubId === hostId)
                    void connect(source.workspace).then(() =>
                      setSection("attention"),
                    );
                  else
                    window.dispatchEvent(
                      new CustomEvent("twr:select-host", {
                        detail: {
                          hubId: source.hubId,
                          workspace: source.workspace,
                          attention: true,
                        },
                      }),
                    );
                }}
                renderItem={(item) => (
                  <AttentionCard
                    key={item.attn_id + ":" + item.revision}
                    item={item}
                    thread={view.threads.find(
                      (thread) => thread.id === item.thread,
                    )}
                    readOnly={!connected || !permitted("operate")}
                    onRespond={respond}
                    onDismiss={dismissAttention}
                    rpc={rpc}
                    fail={fail}
                  />
                )}
              />
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
                canAdmin={permitted("admin")}
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
            <aside
              className="inspector"
              id="twr-inspector"
              aria-label={`Thread details: ${selectedThread.name}`}
            >
              <PaneResize pane="inspector" />
              <header>
                <span className="eyebrow">Thread details</span>
                <button
                  className="icon-button"
                  aria-label="Close thread details"
                  title="Close thread details"
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
                    title="Reveal worktree"
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
                    readOnly={!connected || !permitted("operate")}
                    onRespond={respond}
                    onDismiss={dismissAttention}
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
                  <button
                    aria-pressed={pins.includes(selectedThread.id)}
                    onClick={() => togglePin(selectedThread.id)}
                  >
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
        <nav className="compact-nav" aria-label="Sections">
          <button
            aria-current={section === "chat" ? "page" : undefined}
            onClick={() => {
              setSection("chat");
            }}
          >
            <MessageSquare size={15} />
            Chat
          </button>
          <button
            aria-current={section === "attention" ? "page" : undefined}
            onClick={() => setSection("attention")}
          >
            <Bell size={15} />
            Needs you
            {view.attention.length > 0 && (
              <>
                {" "}
                <span className="count">{view.attention.length}</span>
              </>
            )}
          </button>
          <button
            aria-current={section === "grid" ? "page" : undefined}
            onClick={() => setSection("grid")}
          >
            <LayoutGrid size={15} />
            Threads
          </button>
          <button
            aria-expanded={sidebar}
            aria-controls="twr-sidebar"
            onClick={() => setSidebar((v) => !v)}
          >
            <MoreHorizontal size={16} />
            More
          </button>
        </nav>

        {terminals.length > 0 && (
          <section
            className={`terminal-dock ${terminalVisible ? "" : "collapsed"}`}
            aria-label="Terminals"
          >
            <div className="terminal-tabs">
              <button
                className="icon-button"
                aria-label={
                  terminalVisible ? "Collapse terminals" : "Expand terminals"
                }
                title={
                  terminalVisible ? "Collapse terminals" : "Expand terminals"
                }
                aria-expanded={terminalVisible}
                aria-controls="twr-terminal-content"
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
                    aria-current={t.id === activeTerminal ? "true" : undefined}
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
                    {t.exited && (
                      <span className="visually-hidden"> (exited)</span>
                    )}
                  </button>
                  <button
                    aria-label={`Close ${t.title}`}
                    title={`Close ${t.title}`}
                    onClick={() =>
                      setConfirmation({
                        title: "Close terminal?",
                        detail: t.exited
                          ? "Remove this terminal tab?"
                          : "Its interactive program will exit. The hub and other agents keep running.",
                        run: () => {
                          terminalSnapshots.current.changed();
                          void hostApi
                            .terminalClose(t.id)
                            .then(() => {
                              terminalSnapshots.current.removed(t.id);
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
                title="New shell terminal"
                onClick={() => void openTerminal({ shell: true })}
              >
                <Plus size={15} />
              </button>
            </div>
            <div
              className="terminal-content"
              id="twr-terminal-content"
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
        <div role="alert" className="toast" data-modal-keep="">
          <AlertCircle size={17} />
          <span>{error}</span>
          <button
            aria-label="Dismiss error"
            title="Dismiss"
            onClick={() => setError("")}
          >
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
              aria-controls="twr-palette-list"
              aria-autocomplete="list"
              aria-activedescendant={
                matching[paletteIndex]
                  ? `twr-palette-${paletteIndex}`
                  : undefined
              }
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
            <div
              className="palette-options"
              id="twr-palette-list"
              role="listbox"
              aria-label="Commands"
            >
              {matching.map((command, index) => (
                <button
                  data-cli={command.cli ? "true" : undefined}
                  data-palette-active={index === paletteIndex}
                  className={index === paletteIndex ? "active" : ""}
                  role="option"
                  id={`twr-palette-${index}`}
                  aria-selected={index === paletteIndex}
                  tabIndex={-1}
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
                  <span className="visually-hidden">, </span>
                  <span className="command-group">{command.group}</span>
                  <ChevronRight size={15} />
                </button>
              ))}
            </div>
            {!matching.length && (
              <p className="muted" role="status">
                No matching commands.
              </p>
            )}
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
                      : field.type === "model" || field.type === "effort"
                        ? {
                            ...field,
                            choices: (field.type === "model"
                              ? modelChoices
                              : effortChoices)(
                              modelCatalog,
                              formRuntime(
                                operation,
                                values,
                                view.threads,
                                info?.orchestrator?.configured?.explicit ===
                                  false
                                  ? info.orchestrator.configured.runtime
                                  : undefined,
                              ),
                            ),
                          }
                        : field.key === "mode" && !permitted("admin")
                          ? {
                              ...field,
                              options: [
                                "manual",
                                "plan",
                                "read-only",
                                "dontAsk",
                              ],
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
                      {result?.releaseUrl && (
                        <button
                          onClick={() =>
                            void hostApi
                              .openExternal(
                                result.download?.url || result.releaseUrl,
                              )
                              .catch(fail)
                          }
                        >
                          {result.download
                            ? `Download ${result.download.name}`
                            : "Open release page"}{" "}
                          <ExternalLink size={14} aria-hidden="true" />
                        </button>
                      )}
                      <button onClick={() => setSection("settings")}>
                        Update settings
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
      {searchOpen && (
        <ConversationSearch
          rpc={rpc}
          workspace={workspace}
          hubName={
            hostId === "local"
              ? "This computer"
              : String(
                  inboxSources.find((source) => source.hubId === hostId)
                    ?.hubName ||
                    info?.hostname ||
                    hostId,
                )
          }
          threads={view.threads}
          onClose={() => setSearchOpen(false)}
        />
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
                  aria-current={w.name === workspace ? "true" : undefined}
                  onClick={() => {
                    setWorkspaceModal(false);
                    void connect(w.name);
                  }}
                >
                  {w.name}
                  <span className="visually-hidden">, </span>
                  <span>{w.state || w.status}</span>
                </button>
                <button
                  className="icon-button danger-quiet"
                  aria-label={`Remove workspace ${w.name}`}
                  title={`Remove workspace ${w.name}`}
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
                  aria-describedby="twr-workspace-name-hint"
                  pattern="[a-z0-9][a-z0-9-]{0,31}"
                  maxLength={32}
                  required
                  placeholder="e.g. client-project"
                />
                <span className="visually-hidden" id="twr-workspace-name-hint">
                  Lowercase letters, numbers and hyphens, starting with a letter
                  or number; up to 32 characters.
                </span>
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
            <span>Chat</span>
          </button>
          <button
            aria-current={section === "attention" ? "page" : undefined}
            onClick={() => {
              setSection("attention");
              setSidebar(false);
            }}
          >
            <Bell size={19} />
            <span>
              Needs you
              {view.attention.length ? ` · ${view.attention.length}` : ""}
            </span>
          </button>
          <button
            aria-current={section === "grid" ? "page" : undefined}
            onClick={() => {
              setSection("grid");
              setSidebar(false);
            }}
          >
            <LayoutGrid size={19} />
            <span>Threads</span>
          </button>
          <button
            aria-expanded={sidebar}
            aria-controls="twr-sidebar"
            onClick={() => setSidebar((v) => !v)}
          >
            <MoreHorizontal size={19} />
            <span>More</span>
          </button>
        </nav>
      )}
      <span id="twr-thread-open-hint" hidden>
        Details shown. Press Enter to open this conversation.
      </span>
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
  listItem = false,
}: {
  thread: Thread;
  items: Attention[];
  selected: boolean;
  dived?: boolean;
  pinned: boolean;
  onSelect: () => void;
  onOpen: () => void;
  large?: boolean;
  listItem?: boolean;
}) {
  const card = (
    <ThreadCardButton
      thread={thread}
      items={items}
      selected={selected}
      dived={dived}
      pinned={pinned}
      onSelect={onSelect}
      onOpen={onOpen}
      large={large}
    />
  );
  return listItem ? (
    <div role="listitem" className="thread-item">
      {card}
    </div>
  ) : (
    card
  );
}
function ThreadCardButton({
  thread,
  items,
  selected,
  dived,
  pinned,
  onSelect,
  onOpen,
  large,
}: {
  thread: Thread;
  items: Attention[];
  selected: boolean;
  dived: boolean;
  pinned: boolean;
  onSelect: () => void;
  onOpen: () => void;
  large: boolean;
}) {
  const git = thread.git || thread.git_snapshot;
  const report = thread.last_report;
  return (
    <button
      className={`thread-card ${selected ? "selected" : ""} ${dived ? "dived" : ""} ${large ? "large" : ""}`}
      aria-current={dived ? "location" : undefined}
      aria-describedby={selected ? "twr-thread-open-hint" : undefined}
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
        <span className="thread-color-dot" aria-hidden="true" />
        <b>{thread.name}</b>
        {dived && (
          <ArrowUpRight
            size={13}
            className="dive-marker"
            role="img"
            aria-label="Current destination"
          />
        )}
        {pinned && <Pin size={11} role="img" aria-label="Pinned" />}
        <span className="visually-hidden">, </span>
        <span className={`thread-status status-${thread.status}`}>
          {statusIcon(thread.status)}
          {thread.status_label || thread.status}
        </span>
      </div>

      <span className="visually-hidden">. </span>
      {items[0] ? (
        <span className="thread-card-request">
          <AlertCircle size={12} />
          <span className="visually-hidden">Request: </span>
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
      <span className="visually-hidden">. </span>
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
/** Tracks how long an attention card's options have been on screen: from mount, or from
 * when they scroll into view, and again from zero after they leave it or the window is
 * hidden. `identity` restarts it for a new item or revision. */
function useArming(target: RefObject<HTMLElement | null>, identity: string) {
  const state = useRef<Arming>({});
  const [isArmed, setArmed] = useState(false);
  useEffect(() => {
    state.current = hidden();
    setArmed(false);
    const node = target.current;
    const observable = !!node && typeof IntersectionObserver !== "undefined";
    let onScreen = !observable;
    let timer: ReturnType<typeof setTimeout> | undefined;
    const update = () => {
      clearTimeout(timer);
      const now = performance.now();
      state.current =
        onScreen && document.visibilityState !== "hidden"
          ? shown(state.current, now)
          : hidden();
      setArmed(armed(state.current, now));
      const wait = armsIn(state.current, now);
      if (wait) timer = setTimeout(update, wait);
    };
    let observer: IntersectionObserver | undefined;
    if (observable) {
      observer = new IntersectionObserver((entries) => {
        onScreen = entries[entries.length - 1].isIntersecting;
        update();
      });
      observer.observe(node);
    } else update();
    document.addEventListener("visibilitychange", update);
    return () => {
      clearTimeout(timer);
      observer?.disconnect();
      document.removeEventListener("visibilitychange", update);
    };
  }, [identity]);
  return { armed: isArmed, state: () => state.current };
}
function AttentionCard({
  item,
  readOnly = false,
  thread,
  onRespond,
  onDismiss,
  rpc,
  fail,
}: {
  item: Attention;
  readOnly?: boolean;
  thread?: Thread;
  /** Answers through App's live-view guard; rejects when it can't be answered now. */
  onRespond: (
    item: Attention,
    answer: string,
    message?: string,
  ) => Promise<void>;
  onDismiss: (item: Attention) => Promise<void>;
  rpc: (method: string, params?: Json) => Promise<Json>;
  fail: (e: unknown) => void;
}) {
  const [answer, setAnswer] = useState("");
  const [message, setMessage] = useState("");
  const [busy, setBusy] = useState(false);
  // Once an answer is accepted the card stays answered until the next state removes it:
  // a second click would only meet the stale revision.
  const [sent, setSent] = useState(false);
  const [requestError, setRequestError] = useState("");
  const options = item.options || [];
  const ids = useId();
  const response = useRef<HTMLDivElement>(null);
  const arming = useArming(response, item.attn_id + ":" + item.revision);
  const pending = useRef(false);
  const settle = async (run: () => Promise<void>) => {
    // Clicks and keys that land before the options have been on screen for 600 ms were
    // meant for whatever was there before (a card that just slid away, the previous
    // revision): they answer nothing.
    if (
      pending.current ||
      !canRespond(item, busy) ||
      !mayAnswer({
        arming: arming.state(),
        now: performance.now(),
        busy,
        sent,
        readOnly,
      })
    )
      return;
    pending.current = true;
    setBusy(true);
    setRequestError("");
    try {
      await run();
      setSent(true);
    } catch (e) {
      setRequestError(errorMessage(e));
    } finally {
      pending.current = false;
      setBusy(false);
    }
  };
  const respond = (value: string) =>
    settle(() => onRespond(item, value, message || undefined));
  const locked = readOnly || busy || sent || !item.revision || item.incomplete;
  const waiting = !locked && !arming.armed;
  return (
    <article
      className={`attention-card attention-${item.reason}`}
      tabIndex={0}
      aria-labelledby={`${ids}-kind ${ids}-thread`}
      aria-describedby={`${ids}-summary`}
      aria-keyshortcuts={
        options.length && !readOnly
          ? options.map((_, i) => String(i + 1)).join(" ")
          : undefined
      }
      onKeyDown={(event) => {
        if (
          event.target !== event.currentTarget ||
          locked ||
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
        <span className="attention-kind" id={`${ids}-kind`}>
          <AlertCircle size={14} />
          {item.reason === "permission"
            ? "Permission requested"
            : item.reason === "question"
              ? "Question for you"
              : item.reason}
        </span>
        <span id={`${ids}-thread`}>{thread?.name || "twr"}</span>
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
        <pre className="attention-summary" id={`${ids}-summary`}>
          {item.summary}
        </pre>
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
      <div className="attention-response" ref={response}>
        <input
          aria-label="Optional message with your decision"
          aria-describedby={requestError ? `${ids}-error` : undefined}
          placeholder="Add a note (optional)…"
          value={message}
          onChange={(e) => setMessage(e.target.value)}
          disabled={readOnly || busy || sent}
        />
        <div
          className={`attention-options ${waiting ? "arming" : ""}`}
          data-armed={arming.armed && !locked ? "true" : "false"}
        >
          {options.map((option, i) => (
            <button
              disabled={locked}
              aria-disabled={waiting || undefined}
              key={option + ":" + i}
              aria-keyshortcuts={String(i + 1)}
              onClick={() => void respond(option)}
            >
              <kbd aria-hidden="true">{i + 1}</kbd>
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
                  aria-required="true"
                  aria-describedby={requestError ? `${ids}-error` : undefined}
                  aria-invalid={requestError ? true : undefined}
                  value={answer}
                  onChange={(e) => setAnswer(e.target.value)}
                  placeholder="Your answer…"
                  disabled={readOnly || busy || sent}
                />
                <button
                  disabled={locked || !answer.trim()}
                  aria-disabled={waiting || undefined}
                >
                  Answer
                </button>
              </form>
            )}
          {!["permission", "question"].includes(item.reason) && (
            <button
              disabled={locked}
              aria-disabled={waiting || undefined}
              onClick={() => void settle(() => onDismiss(item))}
            >
              Dismiss
            </button>
          )}
          {busy && <Spinner />}
          {sent && (
            <span className="attention-sent" role="status">
              <Check size={14} aria-hidden="true" />
              Sent
            </span>
          )}
        </div>
        {requestError && (
          <p role="alert" className="error-text" id={`${ids}-error`}>
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
  // Expanded messages and open tool details, by block: rows scrolled out of the virtual
  // list unmount, and come back as they were left.
  const [opened, setOpened] = useState<ReadonlyMap<string, boolean>>(
    () => new Map(),
  );
  const setOpen = useCallback((key: string, open: boolean) => {
    setOpened((old) => {
      if ((old.get(key) ?? false) === open) return old;
      const next = new Map(old);
      if (open) next.set(key, true);
      else next.delete(key);
      return next;
    });
  }, []);
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
      role="region"
      aria-label={`Conversation with ${target?.name || "orchestrator"}`}
      aria-busy={loading || undefined}
      tabIndex={0}
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
      {error && (
        <div className="error-banner" role="status">
          {error}
        </div>
      )}
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
                <span className="message-avatar" aria-hidden="true">
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
                  title="Copy message"
                  onClick={() =>
                    void hostApi.copyText(block.detail || block.text)
                  }
                >
                  <Copy size={13} />
                </button>
              </div>
              {block.children ? (
                <details
                  className="routine-updates"
                  open={opened.get("details:" + block.id) ?? false}
                  onToggle={(event) =>
                    setOpen("details:" + block.id, event.currentTarget.open)
                  }
                >
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
                <details
                  open={opened.get("details:" + block.id) ?? false}
                  onToggle={(event) =>
                    setOpen("details:" + block.id, event.currentTarget.open)
                  }
                >
                  <summary>
                    {block.detail?.split("\n")[0] || "Tool call"}
                  </summary>
                  <MessageBody
                    text={block.detail || ""}
                    mono
                    expanded={opened.get(block.id) ?? false}
                    onExpandedChange={(open) => setOpen(block.id, open)}
                  />
                </details>
              ) : (
                <MessageBody
                  text={block.text}
                  clamp={block.kind === "user" || block.kind === "notice"}
                  streaming={block.kind === "assistant" && !!block.pending}
                  expanded={opened.get(block.id) ?? false}
                  onExpandedChange={(open) => setOpen(block.id, open)}
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
              behavior: scrollBehavior(),
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

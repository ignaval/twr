import type { Appearance } from "./appearance";
export type Json = any;
export interface Thread {
  id: string;
  name: string;
  project?: string;
  runtime: string;
  box_name: string;
  mode: string;
  model?: string;
  model_actual?: string;
  effort?: string;
  account?: string;
  status: string;
  status_label?: string;
  accent: string;
  is_orchestrator: boolean;
  worktree?: string;
  repo?: string;
  report?: Json;
  git?: Json;
  /** Stopped (`thread.stopped`): by someone, or only by the hub's own shutdown. */
  stopped?: boolean;
  /** Stopped only by the hub's own shutdown (only while `stopped`): the thread goes on, and
   * a loop may start on it, as soon as it is sent something. */
  stopped_by_hub?: boolean;
  [key: string]: Json;
}
export interface Attention {
  attn_id: string;
  revision: number;
  thread: string;
  reason: string;
  summary: string;
  options: string[];
  note?: string;
  context?: Json;
  incomplete?: boolean;
  /** A confirm or delete card: the hub's own question only the user answers. */
  proposal?: boolean;
  /** A loop's setup card: only the user answers it (Start and save, Cancel). */
  loop_setup?: boolean;
  /** A stuck loop's item: only the user answers it (resume, stop); never dismissed. */
  loop_stuck?: boolean;
}
export interface ViewState {
  threads: Thread[];
  attention: Attention[];
  decisions: [number, Json][];
  decisions_seen: number;
  head: number;
  went_on?: string[];
}
export interface HubEvent {
  id: string;
  seq: number;
  kind: string;
  thread: string;
  turn?: number;
  ts: string;
  data: Json;
  /** Shared Rust classification; never inferred from attention text in the renderer. */
  routine?: boolean;
  stall?: string;
}
export interface DesktopEvent {
  type: string;
  scope?: "terminal" | "control";
  state?: ViewState;
  event?: HubEvent;
  connected?: boolean;
  error?: string;
  thread?: string;
  /** `menu-attention`: the workspace to open it in, when it is not the one shown. */
  workspace?: string;
  subscription?: string;
  sequence?: number;
  id?: string;
  data?: string;
  exitCode?: number;
  theme?: Record<string, string> | null;
  /** `voice-speaker`: this computer's own speaker (desktop/voice.ts). */
  speaker?: {
    running: boolean;
    device?: string;
    active: boolean;
    name: string;
    playing: boolean;
    paused: boolean;
    queued: number;
    error?: string;
  };
}
export interface TerminalSession {
  id: string;
  title: string;
  workspace: string;
  exited?: boolean;
}
export interface DesktopAppearance {
  preference: Appearance;
  colorScheme: "dark" | "light";
  background: string;
}
export interface InboxSource {
  scanError?: boolean;
  provisional?: boolean;
  running?: boolean;
  updatedAt?: number;
  hubId: string;
  hubName: string;
  workspace: string;
  attention: Attention[];
  threads: Thread[];
  cached?: boolean;
  error?: string;
}
export interface DesktopAPI {
  sendMessage?(message: import("./offline").DurableMessage): Promise<Json>;
  inboxSources?(): Promise<InboxSource[]>;
  secureRead?(key: string): Promise<string | null>;
  secureWrite?(key: string, value: string): Promise<void>;
  secureDelete?(key: string): Promise<void>;
  setAppearance?(appearance: DesktopAppearance): Promise<void>;
  uploadFile?(
    file: File,
    thread: string,
    options?: import("./uploads").UploadOptions,
  ): Promise<{ path: string }>;
  cancelUpload?(
    resume: import("./uploads").UploadResume,
  ): Promise<import("./uploads").UploadCancelResult>;
  /** Voice mode (shared/voice.ts): desktop recognition through `twr voice`. */
  voiceStatus?(): Promise<import("./voice").VoiceCaptureStatus>;
  /** Downloads the speech model (`twr voice setup`), when the status says it is `setup`. */
  voiceSetup?(): Promise<import("./voice").VoiceCaptureStatus>;
  voiceArm?(on: boolean): Promise<void>;
  voiceTranscribe?(
    recording: Uint8Array,
    hint: string[],
  ): Promise<{ text: string }>;
  voiceSpeaker?(
    op: "start" | "stop" | "pause" | "resume" | "skip" | "silence",
  ): Promise<NonNullable<DesktopEvent["speaker"]>>;
  /** Voice mode with the platform's own recognizer and voice (mobile). */
  nativeVoice?: import("./voice").NativeVoice;
  quit(): Promise<void>;
  copyText(text: string): Promise<void>;
  bootstrap(): Promise<{
    workspace: string;
    hostId?: string;
    scopes?: string[];
    allowed_methods?: string[];
    platform: string;
    home: string;
    version: string;
    theme: Record<string, string> | null;
    terminals: TerminalSession[];
  }>;
  connect(workspace: string): Promise<Json>;
  rpc(method: string, params?: Json): Promise<Json>;
  cli(
    args: string[],
  ): Promise<{ stdout: string; stderr: string; code: number }>;
  catalog(): Promise<import("./commands").Command[]>;
  terminalOpen(request: {
    args?: string[];
    cwd?: string;
    title?: string;
    shell?: boolean;
  }): Promise<TerminalSession>;
  terminalFocus(focused: boolean): void;
  terminalAck(id: string, count: number): void;
  terminalWrite(id: string, data: string): void;
  terminalResize(id: string, cols: number, rows: number): void;
  terminalClose(id: string): Promise<void>;
  terminalRead(
    id: string,
  ): Promise<{ data: string; sequence: number; exited?: boolean }>;
  pickDirectory(): Promise<string | null>;
  openExternal(url: string): Promise<void>;
  reveal(path: string): Promise<void>;
  configRead(): Promise<{
    content: string;
    hash: string;
    path: string;
    settings?: Json;
  }>;
  configSave(content: string, hash: string): Promise<void>;
  configSet(
    table: string[],
    key: string,
    value: string | number | boolean,
  ): Promise<void>;
  exportText(name: string, content: string): Promise<void>;
  onEvent(callback: (event: DesktopEvent) => void): () => void;
}
/** One request waiting on the user, in a summary for the host app. */
export interface AttentionSummaryItem {
  workspace: string;
  thread: string;
  /** The thread's name. */
  title: string;
  /** What is asked, for the tray menu. Never put in an OS notification. */
  summary: string;
  /** The attention item (`attn_id`) and its revision: a new revision is a new request. */
  id: string;
  revision: number;
  /** `permission`, `question`, `review`… */
  reason: string;
  /**
   * The hub's own cards, which only the user answers: `confirm`, what the orchestrator
   * proposed from your message; `delete`, a deletion of threads; `loop_setup`, the settings
   * a loop starts on; `loop_stuck`, a loop that waits for you to resume or stop it.
   */
  card?: "confirm" | "delete" | "loop_setup" | "loop_stuck";
  /** Still being raised: not notified until it is complete. */
  incomplete?: boolean;
}
/**
 * What waits on the user in every workspace of the connected hub, for the host app's
 * tray/menu bar, Dock badge and notifications (shared/attentionSummary.ts).
 */
export interface AttentionSummary {
  /** Everything waiting, in every workspace; `items` may list fewer. */
  count: number;
  /** The shown workspace's items first, then the others'. */
  items: AttentionSummaryItem[];
  /**
   * The workspaces the summary covers. `stream` names where a workspace's items come from
   * while they are current: the live view of one connection, or the periodic reads. It is
   * absent while they are not (connecting, disconnected, a saved or failed read), and it
   * changes when the source starts over (a reconnect, a resync, a switch of workspace), so
   * what the new source lists first is never taken for news.
   */
  workspaces: { name: string; stream?: string }[];
  /**
   * The last loop that ended on each thread, with how it ended: the desktop notifies a loop
   * that ends once, as it does a new request (desktop/notify-state.ts `LoopWatch`).
   */
  loops?: LoopEndSummary[];
}
/** A thread's last loop that ended, for the host app's notification of it. */
export interface LoopEndSummary {
  workspace: string;
  thread: string;
  /** The thread's name. */
  title: string;
  /** The loop's id: a new id is a new end. */
  id: string;
  /** How it ended: `review loop clean after 6 rounds`, an implementation loop's commit or
   * `not committed`. */
  words: string;
}
export type InstallKind =
  "development" | "mac" | "appimage" | "deb" | "pacman" | "portable";
export interface UpdateStatus {
  kind: InstallKind;
  /** Downloads and installs in the app (signed macOS, AppImage). */
  automatic: boolean;
  /** Why this install cannot update itself, when it cannot. */
  manualReason?: string;
  state:
    | "idle"
    | "checking"
    | "up-to-date"
    | "available"
    | "downloading"
    | "downloaded"
    | "error";
  current: string;
  latest?: string;
  releaseUrl?: string;
  download?: { name: string; url: string };
  instructions?: string;
  progress?: number;
  error?: string;
  checkedAt?: number;
}
export interface DesktopAppSettings {
  login: { supported: boolean; enabled: boolean; detail?: string };
  tray: { enabled: boolean; available: boolean };
  /** OS notifications for new permissions and questions (desktop/notifier.ts). */
  notifications: { enabled: boolean; supported: boolean };
  background: boolean;
  autoUpdate: boolean;
  update: UpdateStatus;
  logDirectory: string;
}
/** Desktop application services, independent of which hub is shown. */
export interface DesktopAppAPI {
  settings(): Promise<DesktopAppSettings>;
  setLaunchAtLogin(enabled: boolean): Promise<DesktopAppSettings>;
  setTray(enabled: boolean): Promise<DesktopAppSettings>;
  setNotifications(enabled: boolean): Promise<DesktopAppSettings>;
  setBackground(enabled: boolean): Promise<DesktopAppSettings>;
  setAutoUpdate(enabled: boolean): Promise<DesktopAppSettings>;
  checkForUpdates(): Promise<UpdateStatus>;
  downloadUpdate(): Promise<UpdateStatus>;
  installUpdate(): Promise<void>;
  setAttention(summary: AttentionSummary): void;
  reportError(report: {
    message: string;
    stack?: string;
    source?: string;
  }): void;
  diagnostics(): Promise<string>;
  openLogs(): Promise<void>;
  onUpdate(callback: (status: UpdateStatus) => void): () => void;
}
declare global {
  interface Window {
    twr: DesktopAPI;
    /** Present only in the desktop application. */
    twrApp?: DesktopAppAPI;
  }
}

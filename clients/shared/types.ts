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
  subscription?: string;
  sequence?: number;
  id?: string;
  data?: string;
  exitCode?: number;
  theme?: Record<string, string> | null;
}
export interface TerminalSession {
  id: string;
  title: string;
  workspace: string;
  exited?: boolean;
}
export interface DesktopAPI {
  uploadFile?(file: File, thread: string): Promise<{ path: string }>;
  quit(): Promise<void>;
  copyText(text: string): Promise<void>;
  bootstrap(): Promise<{
    workspace: string;
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
declare global {
  interface Window {
    twr: DesktopAPI;
  }
}

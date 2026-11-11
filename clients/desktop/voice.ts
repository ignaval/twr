// The desktop's half of voice mode (docs/voice.md): recognizing a recording with the bundled
// `twr voice transcribe`, and running this computer's own speaker (`twr speak --control stdio`)
// so voice mode can pause it the moment the user talks. The renderer records; no audio
// leaves this computer unless `[voice] engine = "command"` sends it somewhere.
import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import { hostname } from "node:os";
import { z } from "zod";
import type { DesktopEvent } from "../shared/types";

/** `twr voice transcribe`'s limit: two minutes of 16 kHz 16-bit mono, and a header. */
export const MAX_RECORDING = 16000 * 2 * 120 + 4096;
/**
 * `twr voice` keeps `[voice] timeout_secs` (at most 600) itself; this only bounds a `twr` that
 * stopped answering. It is asked to stop first (SIGTERM: it kills its recognizer and removes
 * its copy of the recording), and killed only if it doesn't.
 */
const TRANSCRIBE_TIMEOUT = 630_000;
const STOP_GRACE = 5_000;
/** `twr voice setup` downloads 148 MB: generous for a slow connection. */
const SETUP_TIMEOUT = 30 * 60_000;

export const recordingSchema = z
  .instanceof(Uint8Array)
  .refine((b) => b.byteLength > 44 && b.byteLength <= MAX_RECORDING, {
    message: "The recording is empty or longer than two minutes",
  });
export const hintSchema = z
  .array(z.string().regex(/^[A-Za-z0-9._][A-Za-z0-9._-]{0,63}$/))
  .max(64);
export const speakerOpSchema = z.enum([
  "start",
  "stop",
  "pause",
  "resume",
  "skip",
  "silence",
]);

/** This computer's speaker name in `/read status` (1-64 printable characters). */
export function speakerName(host = hostname()) {
  const short =
    host
      .split(".")[0]
      .replace(/[^A-Za-z0-9_-]/g, "")
      .slice(0, 48) || "this computer";
  return `${short} desktop`;
}

interface Run {
  stdout: string;
  stderr: string;
  code: number | null;
}

/** Runs `program` with `args` (no shell), `input` on its stdin, until it ends or `timeout`. */
export function runWithInput(
  program: string,
  args: string[],
  input: Uint8Array | undefined,
  env: NodeJS.ProcessEnv,
  timeout: number,
): Promise<Run> {
  return new Promise((resolve, reject) => {
    const child = spawn(program, args, { env, stdio: "pipe" });
    let stdout = "";
    let stderr = "";
    const timer = setTimeout(() => {
      child.kill("SIGTERM");
      setTimeout(() => child.kill("SIGKILL"), STOP_GRACE).unref();
      reject(new Error("Speech recognition took too long"));
    }, timeout);
    child.stdout.on("data", (d) => {
      if (stdout.length < 256 * 1024) stdout += d;
    });
    child.stderr.on("data", (d) => {
      if (stderr.length < 16 * 1024) stderr += d;
    });
    child.on("error", (error) => {
      clearTimeout(timer);
      reject(error);
    });
    child.on("close", (code) => {
      clearTimeout(timer);
      resolve({ stdout, stderr, code });
    });
    child.stdin.on("error", () => {});
    child.stdin.end(input ? Buffer.from(input) : undefined);
  });
}

const cliError = (run: Run) =>
  run.stderr
    .split("\n")
    .map((l) => l.replace(/^twr:\s*/, "").trim())
    .filter(Boolean)
    .pop() || `twr exited with ${run.code}`;

export interface SpeakerEvent {
  running: boolean;
  /** The device id it speaks as, shared by every speaker of this computer. */
  device?: string;
  /** Whether it speaks now (false: another speaker of this computer does, it stands by). */
  active: boolean;
  name: string;
  playing: boolean;
  paused: boolean;
  queued: number;
  error?: string;
}

export class DesktopVoice {
  private speaker?: ChildProcessWithoutNullStreams;
  /** The download under way: a second click waits for it rather than starting another. */
  private setupRun?: Promise<unknown>;
  /** What it plays per workspace: the state shown is all of them together. */
  private workspaces = new Map<
    string,
    { playing: boolean; paused: boolean; queued: number; error?: string }
  >();
  private state: SpeakerEvent = {
    running: false,
    active: false,
    name: speakerName(),
    playing: false,
    paused: false,
    queued: 0,
  };
  constructor(
    private twr: () => string,
    private env: () => NodeJS.ProcessEnv,
    private emit: (event: DesktopEvent) => void,
  ) {}

  async status() {
    const run = await runWithInput(
      this.twr(),
      ["voice", "status", "--json"],
      undefined,
      this.env(),
      15_000,
    );
    if (run.code !== 0)
      return { available: false, local: false, problem: cliError(run) };
    const value = JSON.parse(run.stdout);
    return {
      available: Boolean(value.available),
      local: Boolean(value.local),
      engine: typeof value.engine === "string" ? value.engine : undefined,
      problem: typeof value.problem === "string" ? value.problem : undefined,
      setup: value.setup === true,
      // `[voice]`'s listening settings; an older `twr` says none, and the defaults hold.
      pauseMs: typeof value.pause_ms === "number" ? value.pause_ms : undefined,
      listenWhileSpeaking: value.listen_while_speaking === true,
    };
  }

  /** Downloads the default model (`twr voice setup`), then says what is there now. */
  async setup() {
    this.setupRun ??= runWithInput(
      this.twr(),
      ["voice", "setup"],
      undefined,
      this.env(),
      SETUP_TIMEOUT,
    )
      .then((run) => {
        if (run.code !== 0) throw new Error(cliError(run));
      })
      .finally(() => {
        this.setupRun = undefined;
      });
    await this.setupRun;
    return this.status();
  }

  async transcribe(recording: unknown, hint: unknown) {
    const wav = recordingSchema.parse(recording);
    const words = hintSchema.parse(hint);
    const args = ["voice", "transcribe", "--json"];
    if (words.length) args.push("--expect", words.join(" "));
    const run = await runWithInput(
      this.twr(),
      args,
      wav,
      this.env(),
      TRANSCRIBE_TIMEOUT,
    );
    if (run.code !== 0) throw new Error(cliError(run));
    const text = JSON.parse(run.stdout).text;
    return { text: typeof text === "string" ? text : "" };
  }

  private publish(patch: Partial<SpeakerEvent>) {
    this.state = { ...this.state, ...patch };
    this.emit({ type: "voice-speaker", speaker: this.state } as DesktopEvent);
  }

  /** Starts this computer's speaker, or controls it. */
  async speakerControl(op: unknown) {
    const action = speakerOpSchema.parse(op);
    if (action === "start") return this.startSpeaker();
    if (action === "stop") {
      this.write("quit");
      return this.state;
    }
    if (!this.speaker)
      throw new Error("This computer's speaker is not running");
    this.write(action === "silence" ? "stop" : action);
    return this.state;
  }

  private write(line: string) {
    if (this.speaker && this.speaker.stdin.writable)
      this.speaker.stdin.write(line + "\n");
  }

  private startSpeaker() {
    if (this.speaker) return this.state;
    const child = spawn(
      this.twr(),
      // `--announce`: in voice mode you also hear when something needs you.
      ["speak", "--control", "stdio", "--announce", "--name", this.state.name],
      { env: this.env(), stdio: "pipe" },
    );
    this.speaker = child;
    this.publish({ running: true, active: false, error: undefined });
    let out = "";
    child.stdout.on("data", (chunk) => {
      out += chunk;
      let end;
      while ((end = out.indexOf("\n")) >= 0) {
        const line = out.slice(0, end);
        out = out.slice(end + 1);
        try {
          const v = JSON.parse(line);
          if (v.type === "speaker" && typeof v.device === "string")
            this.publish({ device: v.device });
          if (v.type === "state" && typeof v.workspace === "string") {
            this.workspaces.set(v.workspace, {
              playing: Boolean(v.playing),
              paused: Boolean(v.paused),
              queued: Number(v.queued) || 0,
              error: typeof v.error === "string" ? v.error : undefined,
            });
            const all = [...this.workspaces.values()];
            this.publish({
              active: true,
              playing: all.some((w) => w.playing),
              paused: all.some((w) => w.paused),
              queued: all.reduce((n, w) => n + w.queued, 0),
              error: all.find((w) => w.error)?.error,
            });
          }
        } catch {
          /* A line of a newer speaker. */
        }
      }
    });
    let err = "";
    child.stderr.on("data", (chunk) => {
      err = (err + chunk).slice(-4096);
      if (/standing by/.test(String(chunk))) this.publish({ active: false });
      if (/this speaker speaks now|connected; live reading/.test(String(chunk)))
        this.publish({ active: true });
    });
    child.on("error", (error) => {
      this.speaker = undefined;
      this.publish({ running: false, active: false, error: error.message });
    });
    child.on("close", (code) => {
      if (this.speaker === child) this.speaker = undefined;
      this.workspaces.clear();
      const last = err.split("\n").filter(Boolean).pop();
      this.publish({
        running: false,
        active: false,
        playing: false,
        paused: false,
        queued: 0,
        error: code ? last?.replace(/^.*twr speak:\s*/, "") : undefined,
      });
    });
    return this.state;
  }

  /** Stops the speaker (the app quits). */
  dispose() {
    this.write("quit");
    const child = this.speaker;
    setTimeout(() => child?.kill("SIGTERM"), 1500).unref();
  }
}

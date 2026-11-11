// Voice mode (docs/voice.md): the user talks to the orchestrator and hears its replies.
//
// The device with the microphone owns the audio. It records, recognizes speech (the desktop
// through `twr voice transcribe`, mobile through the platform's own recognizer) and sends
// the text through the same `say` path as a typed message, marked `voice: true`. The hub never
// receives audio. Replies come back through the existing read-aloud speaker.
//
// This module holds what is shared by every client:
// - `VoiceCapture`: what a platform provides (a microphone and a recognizer);
// - `createWebCapture`: the browser implementation (the desktop renderer), with a
//   voice-activity detector for hands-free sessions;
// - `localCommand`, `isLikelyEcho`: deterministic readings of a transcript;
// - `VoiceSession`: the state machine the composer drives.

export type VoiceMode = "push" | "hands-free";
export type VoicePhase =
  | "off"
  | "ready"
  | "listening"
  | "hearing"
  | "transcribing"
  | "sending"
  | "error";

export interface VoiceCaptureEvents {
  /** Speech began: the device's own voice is paused at once (barge-in). */
  speechStart(): void;
  /** A transcript in progress (native recognizers only). */
  partial?(text: string): void;
  /** One utterance, recognized. Empty: nothing was said. */
  final(text: string): void;
  /** The input level, 0 to 1, a few times per second. */
  level?(level: number): void;
  error(message: string): void;
}

export interface VoiceCaptureStatus {
  available: boolean;
  /** Whether the audio stays on this device. */
  local: boolean;
  engine?: string;
  problem?: string;
  /** Only the speech model is missing, and the host can download it (`voiceSetup`). */
  setup?: boolean;
  /** `[voice] pause_ms`: the silence that ends a hands-free utterance (desktop). */
  pauseMs?: number;
  /** `[voice] listen_while_speaking`: hands-free keeps listening while replies are read. */
  listenWhileSpeaking?: boolean;
}

export interface VoiceCapture {
  status(): Promise<VoiceCaptureStatus>;
  /** Starts listening. Push mode records until `stop`; hands-free emits one `final` per utterance. */
  start(
    mode: VoiceMode,
    events: VoiceCaptureEvents,
    hint: string[],
  ): Promise<void>;
  /** Ends the current utterance (push-to-talk released); its `final` follows. */
  stop(): Promise<void>;
  /** Stops listening and drops what was not recognized yet. */
  cancel(): Promise<void>;
  /** While the device's own voice plays, louder speech is needed to count (its echo). */
  setEchoGuard?(playing: boolean): void;
  /**
   * Hands-free stops hearing (what reaches the microphone is dropped, and an utterance under
   * way with it) until called with false: the device's own voice is being read aloud.
   */
  setDeaf?(deaf: boolean): void;
}

/** What the device's own speaker does now (`twr speak --control stdio`'s lines, or the mobile one's). */
export interface SpeakerState {
  playing: boolean;
  paused: boolean;
  queued: number;
  /** This device can't see what it plays (another speaker of this computer speaks for it). */
  unknown?: boolean;
}

/** The device's own speaker: what voice mode pauses when the user talks. */
export interface VoiceSpeaker {
  control(op: "pause" | "resume" | "skip" | "stop"): Promise<void>;
  /** Live reading of this workspace on this device's speaker. */
  live(on: boolean): Promise<void>;
}

/** An event of the platform's own recognizer and voice (mobile/src/voice.ts). */
export interface NativeVoiceEvent {
  type: "speechStart" | "partial" | "final" | "level" | "error" | "speaker";
  text?: string;
  level?: number;
  message?: string;
  speaker?: SpeakerState & { running: boolean };
}

/**
 * Voice mode with the platform's own recognizer (iOS Speech, Android SpeechRecognizer) and a
 * speaker that plays on the device (the gateway's feed, said with the platform's voice).
 */
export interface NativeVoice {
  status(): Promise<VoiceCaptureStatus>;
  start(options: { mode: VoiceMode; hint: string[] }): Promise<void>;
  stop(): Promise<void>;
  cancel(): Promise<void>;
  onEvent(callback: (event: NativeVoiceEvent) => void): () => void;
  /** The device's own speaker for this workspace: started, controlled, stopped. */
  speaker?(
    op: "start" | "stop" | "pause" | "resume" | "skip" | "silence",
  ): Promise<SpeakerState & { running: boolean }>;
}

/** A `VoiceCapture` over the platform's own recognizer. */
export function createNativeCapture(native: NativeVoice): VoiceCapture {
  let unsubscribe: (() => void) | undefined;
  return {
    status: () => native.status(),
    async start(mode, events, hint) {
      unsubscribe?.();
      unsubscribe = native.onEvent((e) => {
        if (e.type === "speechStart") events.speechStart();
        else if (e.type === "partial") events.partial?.(e.text || "");
        else if (e.type === "final") events.final(e.text || "");
        else if (e.type === "level") events.level?.(e.level || 0);
        else if (e.type === "error") events.error(e.message || "Voice failed");
      });
      await native.start({ mode, hint });
    },
    stop: () => native.stop(),
    async cancel() {
      unsubscribe?.();
      unsubscribe = undefined;
      await native.cancel();
    },
  };
}

// ---------------------------------------------------------------------------
// Transcript readings

const words = (text: string) =>
  text
    .toLowerCase()
    .normalize("NFKD")
    .replace(/[̀-ͯ]/g, "")
    .replace(/[^a-z0-9' ]+/g, " ")
    .split(/\s+/)
    .filter(Boolean);

export type LocalCommand =
  "quiet" | "pause" | "resume" | "skip" | "repeat" | "end" | "cancel";

const LOCAL: Record<string, LocalCommand> = {
  stop: "quiet",
  quiet: "quiet",
  "be quiet": "quiet",
  "stop talking": "quiet",
  "stop reading": "quiet",
  silence: "quiet",
  "shut up": "quiet",
  "pause reading": "pause",
  pause: "pause",
  "keep reading": "resume",
  "go on": "resume",
  continue: "resume",
  resume: "resume",
  skip: "skip",
  next: "skip",
  "next message": "skip",
  repeat: "repeat",
  "say that again": "repeat",
  "read that again": "repeat",
  "stop listening": "end",
  "end voice": "end",
  "voice off": "end",
  "never mind": "cancel",
  nevermind: "cancel",
  cancel: "cancel",
};

/**
 * A transcript that is one of voice mode's own controls, said alone: it controls the audio and
 * is never sent. Anything longer ("stop the api worker") is a message for the orchestrator.
 */
export function localCommand(text: string): LocalCommand | undefined {
  const w = words(text).filter((x) => x !== "please" && x !== "twr");
  return LOCAL[w.join(" ")];
}

/**
 * Whether a transcript is the device's own voice heard back: most of its words, in order, are
 * a stretch of what was read aloud lately. Only for transcripts heard while it played.
 */
export function isLikelyEcho(text: string, spoken: string[]): boolean {
  const heard = words(text);
  if (heard.length < 2) return false;
  for (const said of spoken) {
    const source = words(said);
    let at = 0;
    let matched = 0;
    for (const word of heard) {
      const found = source.indexOf(word, at);
      // The echo may start anywhere in what was said; after that, words follow closely.
      if (found >= 0 && (matched === 0 || found - at <= 3)) {
        matched++;
        at = found + 1;
      }
    }
    if (matched / heard.length >= 0.75) return true;
  }
  return false;
}

/** What a speaker announces ([speech] read "attention", twr_core::speech::announcement). */
const ANNOUNCED = [
  "needs your permission",
  "has a question",
  "failed",
  "needs your review",
  "seems hung",
  "is stalled",
  "finished",
  "replied",
];

/** The sentences a speaker may announce for these threads: echoes to drop, never messages. */
export function announcements(threads: { name: string }[]): string[] {
  return [
    "twr has a question for you",
    ...threads.flatMap((t) => ANNOUNCED.map((says) => `${t.name} ${says}`)),
  ];
}

/** The words a recognizer should expect: the names of the threads ("api" is heard as "appy"). */
export function voiceHint(
  threads: { name: string; aliases?: string[] }[],
): string[] {
  const out: string[] = [];
  for (const t of threads)
    for (const n of [t.name, ...(t.aliases || [])])
      if (/^[A-Za-z0-9._][A-Za-z0-9._-]{0,63}$/.test(n) && !out.includes(n))
        out.push(n);
  return out.slice(0, 64);
}

// ---------------------------------------------------------------------------
// Voice activity

export interface VadOptions {
  /** Frame length in samples at 16 kHz (20 ms). */
  frame: number;
  /** Speech frames in a row that start an utterance. */
  startFrames: number;
  /** Silence after speech that ends it, in ms. */
  endSilenceMs: number;
  /** Longest utterance, in ms. */
  maxMs: number;
  /** Audio kept from before speech began, in ms. */
  preRollMs: number;
}

/**
 * The silence that ends a hands-free utterance, in ms (`[voice] pause_ms`). Long enough for
 * a pause to think mid-sentence; 900 ms cut messages off at a comma.
 */
export const PAUSE_MS = 2000;
/** `[voice] pause_ms`'s bounds. */
export const PAUSE_MS_MIN = 500;
export const PAUSE_MS_MAX = 10_000;

/**
 * How long hands-free stays deaf after the device's own voice stops: the end of a sentence
 * still in the speakers, the room's echo, a speaker's state reported a moment late.
 */
export const ECHO_TAIL_MS = 800;

export const VAD_DEFAULTS: VadOptions = {
  frame: 320,
  startFrames: 4,
  endSilenceMs: PAUSE_MS,
  maxMs: 30_000,
  preRollMs: 300,
};

/**
 * An energy detector with an adaptive noise floor: speech is a frame louder than the floor by
 * a margin (raised while the device's own voice plays, so its echo rarely counts). Pure:
 * `push` takes 16 kHz samples and returns what happened.
 */
export class VoiceActivity {
  private floor = 0.004;
  private run = 0;
  private silent = 0;
  private active = false;
  private pre: Float32Array[] = [];
  private utterance: Float32Array[] = [];
  private length = 0;
  private pending = new Float32Array(0);
  guard = false;
  constructor(private options: VadOptions = VAD_DEFAULTS) {}
  /** Feeds samples; returns `start` when speech began, the utterance when it ended. */
  push(samples: Float32Array): { start?: true; utterance?: Float32Array }[] {
    const out: { start?: true; utterance?: Float32Array }[] = [];
    const joined = new Float32Array(this.pending.length + samples.length);
    joined.set(this.pending);
    joined.set(samples, this.pending.length);
    const { frame } = this.options;
    let at = 0;
    for (; at + frame <= joined.length; at += frame) {
      const f = joined.slice(at, at + frame);
      let sum = 0;
      for (const s of f) sum += s * s;
      const rms = Math.sqrt(sum / f.length);
      const margin = this.guard ? 6 : 3;
      const threshold = Math.max(
        this.floor * margin,
        this.guard ? 0.03 : 0.012,
      );
      const speech = rms > threshold;
      // The floor follows quiet frames fast and loud ones slowly.
      if (!speech)
        this.floor = this.floor * 0.95 + Math.min(rms, this.floor * 4) * 0.05;
      else this.floor = this.floor * 0.999 + rms * 0.001;
      this.floor = Math.min(Math.max(this.floor, 0.0005), 0.05);
      const frameMs = (frame / 16000) * 1000;
      if (!this.active) {
        this.pre.push(f);
        while (this.pre.length * frameMs > this.options.preRollMs)
          this.pre.shift();
        this.run = speech ? this.run + 1 : 0;
        if (this.run >= this.options.startFrames) {
          this.active = true;
          this.silent = 0;
          this.utterance = [...this.pre];
          this.length = this.pre.length * frame;
          this.pre = [];
          out.push({ start: true });
        }
        continue;
      }
      this.utterance.push(f);
      this.length += frame;
      this.silent = speech ? 0 : this.silent + frameMs;
      if (
        this.silent >= this.options.endSilenceMs ||
        (this.length / 16000) * 1000 >= this.options.maxMs
      )
        out.push({ utterance: this.finish() });
    }
    this.pending = joined.slice(at);
    return out;
  }
  /** What was heard so far, as an utterance (hands-free stopped mid-sentence). */
  flush(): Float32Array | undefined {
    return this.active ? this.finish() : undefined;
  }
  /** Drops what was heard: the pre-roll, and an utterance under way. The floor stays. */
  reset() {
    this.finish();
    this.pre = [];
    this.pending = new Float32Array(0);
  }
  private finish() {
    const all = new Float32Array(this.length);
    let at = 0;
    for (const f of this.utterance) {
      all.set(f, at);
      at += f.length;
    }
    this.active = false;
    this.run = 0;
    this.utterance = [];
    this.length = 0;
    return all;
  }
}

// ---------------------------------------------------------------------------
// Audio helpers

/** `input` at `rate` Hz, as 16 kHz samples (averaging, enough for speech). */
export function downsample(input: Float32Array, rate: number): Float32Array {
  if (rate === 16000) return input;
  const ratio = rate / 16000;
  const out = new Float32Array(Math.floor(input.length / ratio));
  for (let i = 0; i < out.length; i++) {
    const start = Math.floor(i * ratio);
    const end = Math.min(input.length, Math.floor((i + 1) * ratio));
    let sum = 0;
    for (let j = start; j < end; j++) sum += input[j];
    out[i] = end > start ? sum / (end - start) : 0;
  }
  return out;
}

/** 16 kHz mono 16-bit PCM WAV of `samples` (what `twr voice transcribe` takes). */
export function encodeWav(samples: Float32Array): Uint8Array {
  const bytes = new Uint8Array(44 + samples.length * 2);
  const view = new DataView(bytes.buffer);
  const text = (at: number, s: string) =>
    [...s].forEach((c, i) => view.setUint8(at + i, c.charCodeAt(0)));
  text(0, "RIFF");
  view.setUint32(4, 36 + samples.length * 2, true);
  text(8, "WAVEfmt ");
  view.setUint32(16, 16, true);
  view.setUint16(20, 1, true);
  view.setUint16(22, 1, true);
  view.setUint32(24, 16000, true);
  view.setUint32(28, 32000, true);
  view.setUint16(32, 2, true);
  view.setUint16(34, 16, true);
  text(36, "data");
  view.setUint32(40, samples.length * 2, true);
  for (let i = 0; i < samples.length; i++) {
    const s = Math.max(-1, Math.min(1, samples[i]));
    view.setInt16(44 + i * 2, s < 0 ? s * 0x8000 : s * 0x7fff, true);
  }
  return bytes;
}

/** The longest push-to-talk recording, in seconds (`twr voice transcribe`'s limit). */
export const PUSH_MAX_SECONDS = 120;

/**
 * The browser's microphone and an external recognizer (`transcribe`): the desktop renderer.
 * Echo cancellation, noise suppression and gain control are the browser's.
 */
export function createWebCapture(
  transcribe: (wav: Uint8Array, hint: string[]) => Promise<string>,
  status: () => Promise<VoiceCaptureStatus>,
): VoiceCapture {
  let stream: MediaStream | undefined;
  let context: AudioContext | undefined;
  let node: ScriptProcessorNode | undefined;
  let mode: VoiceMode = "push";
  let events: VoiceCaptureEvents | undefined;
  let vad = new VoiceActivity();
  let recorded: Float32Array[] = [];
  let recordedLength = 0;
  let hints: string[] = [];
  let lastLevel = 0;
  let pauseMs = PAUSE_MS;
  let deaf = false;
  // Recognitions run one at a time, in order.
  let chain = Promise.resolve();
  const recognize = (samples: Float32Array) => {
    const target = events;
    chain = chain.then(async () => {
      if (!target || samples.length < 16000 * 0.25) {
        target?.final("");
        return;
      }
      try {
        target.final(await transcribe(encodeWav(samples), hints));
      } catch (error) {
        target.error(error instanceof Error ? error.message : String(error));
      }
    });
  };
  const release = async () => {
    node?.disconnect();
    node = undefined;
    stream?.getTracks().forEach((t) => t.stop());
    stream = undefined;
    await context?.close().catch(() => {});
    context = undefined;
  };
  return {
    async status() {
      const s = await status();
      if (s.pauseMs) pauseMs = clampPause(s.pauseMs);
      return s;
    },
    async start(nextMode, nextEvents, hint) {
      await release();
      mode = nextMode;
      events = nextEvents;
      hints = hint;
      // `deaf` is the session's to say, before or after the microphone opens.
      vad = new VoiceActivity({ ...VAD_DEFAULTS, endSilenceMs: pauseMs });
      recorded = [];
      recordedLength = 0;
      stream = await navigator.mediaDevices.getUserMedia({
        audio: {
          channelCount: 1,
          echoCancellation: true,
          noiseSuppression: true,
          autoGainControl: true,
        },
      });
      context = new AudioContext();
      const source = context.createMediaStreamSource(stream);
      node = context.createScriptProcessor(4096, 1, 1);
      const rate = context.sampleRate;
      node.onaudioprocess = (event) => {
        const samples = downsample(event.inputBuffer.getChannelData(0), rate);
        let peak = 0;
        for (const s of samples) peak = Math.max(peak, Math.abs(s));
        const now = performance.now();
        const handsFreeDeaf = deaf && mode === "hands-free";
        if (now - lastLevel > 100) {
          lastLevel = now;
          events?.level?.(handsFreeDeaf ? 0 : Math.min(1, peak * 2));
        }
        if (mode === "push") {
          if (recordedLength / 16000 < PUSH_MAX_SECONDS) {
            recorded.push(samples);
            recordedLength += samples.length;
          }
          return;
        }
        // The device's own voice plays: nothing reaches the detector (its floor included).
        if (handsFreeDeaf) return;
        for (const happened of vad.push(samples)) {
          if (happened.start) events?.speechStart();
          if (happened.utterance) recognize(happened.utterance);
        }
      };
      source.connect(node);
      // A processor only runs connected to an output; it writes silence there.
      node.connect(context.destination);
      if (mode === "push") events.speechStart();
    },
    async stop() {
      if (mode === "push") {
        const all = new Float32Array(recordedLength);
        let at = 0;
        for (const r of recorded) {
          all.set(r, at);
          at += r.length;
        }
        recorded = [];
        recordedLength = 0;
        await release();
        recognize(all);
      } else {
        const rest = vad.flush();
        await release();
        if (rest) recognize(rest);
      }
      await chain;
    },
    async cancel() {
      events = undefined;
      recorded = [];
      await release();
    },
    setEchoGuard(playing) {
      vad.guard = playing;
    },
    setDeaf(on) {
      if (on && !deaf) vad.reset();
      deaf = on;
    },
  };
}

/** A `pause_ms` within its bounds; anything else is the default. */
export function clampPause(ms: number): number {
  if (!Number.isFinite(ms)) return PAUSE_MS;
  return Math.min(PAUSE_MS_MAX, Math.max(PAUSE_MS_MIN, Math.round(ms)));
}

// ---------------------------------------------------------------------------
// The session

export interface VoiceSubmitResult {
  action?: string;
  heard?: string;
  reason?: string;
}

export interface VoiceSessionDeps {
  capture: VoiceCapture;
  speaker?: VoiceSpeaker;
  /** Sends a transcript to the orchestrator (`say` with `voice: true`). */
  submit(text: string): Promise<VoiceSubmitResult>;
  /** Reads the last orchestrator message aloud again. */
  repeat?(): Promise<void>;
  /** Readies the microphone (recognizer set up, access granted); false: not now. */
  prepare?(): Promise<boolean>;
  hint(): string[];
  /** What was read aloud lately (the orchestrator's last messages), for echo checks. */
  spoken(): string[];
  onChange(state: VoiceSessionState): void;
}

export interface VoiceSessionState {
  phase: VoicePhase;
  mode: VoiceMode;
  level: number;
  partial: string;
  /** The last thing heard, and what became of it. */
  heard?: {
    text: string;
    outcome: "sent" | "confirm" | "control" | "echo" | "empty";
  };
  /** A transcript the hub handed back to confirm (it reads as an answer or a mode change). */
  confirm?: { text: string; reason: string };
  error?: string;
  speaker?: SpeakerState;
  /**
   * Hands-free is not listening now: the device's own voice is being read aloud (or just
   * was), and what the microphone hears is its echo.
   */
  deaf?: boolean;
}

export interface VoiceSessionOptions {
  /**
   * Hands-free keeps listening while replies are read aloud, so talking interrupts them
   * (`[voice] listen_while_speaking`). Off: it stops listening until they end.
   */
  listenWhileSpeaking: boolean;
  /** How long it stays deaf after they end, in ms. */
  echoTailMs: number;
}

/**
 * Voice mode's state machine. Push-to-talk: `press` starts recording, `release` sends it.
 * Hands-free: `begin` listens until `end`, one message per utterance. While the device's own
 * voice reads a reply, and for `echoTailMs` after, hands-free does not listen (its echo would be
 * sent as a message); a user already talking when a reply starts holds the reply until what
 * they said is handled. With `listenWhileSpeaking`, and in push to talk, the device's own voice
 * is paused at once when speech starts (barge-in), without waiting for the hub. A transcript is
 * a local control (`quiet`, `skip`), the device's own echo (dropped), or a message.
 */
export class VoiceSession {
  private s: VoiceSessionState = {
    phase: "off",
    mode: "push",
    level: 0,
    partial: "",
  };
  /** Whether this session paused the speaker (and so may resume it). */
  private held = false;
  /** The hold is the user talking over the voice (barge-in): a message sent goes past it. */
  private barged = false;
  /** Utterances begun while deaf: their transcripts are dropped. */
  private dropping = 0;
  /** Deaf until this time (ms, `Date.now()`); Infinity while the voice plays. */
  private deafUntil = 0;
  private deafTimer?: ReturnType<typeof setTimeout>;
  options: VoiceSessionOptions = {
    listenWhileSpeaking: false,
    echoTailMs: ECHO_TAIL_MS,
  };
  /** Utterances begun (speech started) and not yet handled: the hold lasts until none is. */
  private pending = 0;
  /** A message was sent while the speaker was held: it goes past what it was reading. */
  private sentWhileHeld = false;
  /** Whether the speaker played (or may have) while these utterances were spoken. */
  private heardWhilePlaying = false;
  /** The microphone is being opened; a release meanwhile is kept for when it is. */
  private starting = false;
  private releaseWanted = false;
  private speakerStarted = false;
  private busy = Promise.resolve();
  constructor(private deps: VoiceSessionDeps) {}
  get state() {
    return this.s;
  }
  private set(patch: Partial<VoiceSessionState>) {
    this.s = { ...this.s, ...patch };
    this.deps.onChange(this.s);
  }
  /** Whether a recording can start now (none is being made, recognized or sent). */
  get idle() {
    return (
      !this.starting &&
      this.pending === 0 &&
      ["off", "ready", "error"].includes(this.s.phase)
    );
  }
  /** What the device's speaker reports. */
  speakerState(state: SpeakerState) {
    this.set({ speaker: state });
    this.deps.capture.setEchoGuard?.(
      Boolean(state.unknown) || (state.playing && !state.paused),
    );
    this.updateDeaf();
  }
  /** Whether hands-free stops listening while the device's own voice plays. */
  private get gated() {
    return (
      this.s.mode === "hands-free" &&
      this.s.phase !== "off" &&
      !this.options.listenWhileSpeaking
    );
  }
  /**
   * Hands-free goes deaf while this device's voice plays (one it can see: a speaker of this
   * computer standing by can't tell, and the echo check stays its guard), and for the echo
   * tail after. A user already talking when it starts holds it instead.
   */
  private updateDeaf() {
    const sp = this.s.speaker;
    const playing = Boolean(sp && !sp.unknown && sp.playing && !sp.paused);
    if (!this.gated) {
      this.listenAgain();
      return;
    }
    if (playing) {
      if (this.deafUntil === Infinity) return;
      if (this.pending > 0) {
        // The user spoke first: the reply waits until what they said is handled. (Deafening
        // the microphone now would drop their utterance half heard.)
        if (!this.held && this.deps.speaker) {
          this.held = true;
          this.barged = false;
          void this.deps.speaker.control("pause").catch(() => {});
        }
        return;
      }
      clearTimeout(this.deafTimer);
      this.deafTimer = undefined;
      this.deafUntil = Infinity;
      this.deps.capture.setDeaf?.(true);
      if (!this.s.deaf) this.set({ deaf: true, partial: "" });
      return;
    }
    if (this.deafUntil !== Infinity) return;
    // It stopped: the end of its echo is still in the room.
    const tail = Math.max(0, this.options.echoTailMs);
    this.deafUntil = Date.now() + tail;
    clearTimeout(this.deafTimer);
    this.deafTimer = setTimeout(() => this.listenAgain(), tail);
  }
  /** Hands-free hears again. */
  private listenAgain() {
    clearTimeout(this.deafTimer);
    this.deafTimer = undefined;
    this.deafUntil = 0;
    this.deps.capture.setDeaf?.(false);
    if (this.s.deaf) this.set({ deaf: false });
  }
  private get deaf() {
    return this.gated && Date.now() < this.deafUntil;
  }
  private events(): VoiceCaptureEvents {
    return {
      speechStart: () => this.speechStarted(),
      partial: (text) => {
        if (this.dropping === 0 && !this.deaf) this.set({ partial: text });
      },
      level: (level) => this.set({ level: this.deaf ? 0 : level }),
      final: (text) => {
        // An utterance begun while deaf: the device's own voice, or the end of it.
        if (this.dropping > 0) {
          this.dropping--;
          return;
        }
        this.busy = this.busy.then(() => this.heard(text));
      },
      error: (message) => {
        if (this.dropping > 0) this.dropping--;
        this.busy = this.busy.then(() => this.failed(message));
      },
    };
  }
  private speechStarted() {
    if (this.deaf || this.dropping > 0) {
      this.dropping++;
      return;
    }
    this.pending++;
    const sp = this.s.speaker;
    // A speaker this device can't see (another one of this computer speaks for it) may be
    // playing: it is paused all the same, and what is heard is checked for its echo.
    const playing = Boolean(sp?.unknown) || Boolean(sp?.playing && !sp.paused);
    if (playing) this.heardWhilePlaying = true;
    if (playing && !this.held && this.deps.speaker) {
      this.held = true;
      this.barged = true;
      void this.deps.speaker.control("pause").catch(() => {});
    }
    this.set({ phase: "hearing", partial: "" });
  }
  private after(): VoicePhase {
    if (this.s.phase === "off") return "off";
    if (this.pending > 0) return "hearing";
    return this.s.mode === "hands-free" ? "listening" : "ready";
  }
  /** One utterance is handled: once none is left, the speaker this session paused goes on. */
  private async done(patch: Partial<VoiceSessionState>) {
    this.pending = Math.max(0, this.pending - 1);
    if (this.pending === 0) {
      await this.letGo(this.sentWhileHeld);
      this.sentWhileHeld = false;
      this.heardWhilePlaying = false;
    }
    this.set({ partial: "", ...patch, phase: patch.phase || this.after() });
    if (this.s.phase === "off") this.listenAgain();
  }
  private async failed(message: string) {
    await this.done({ phase: "error", error: message });
  }
  private async heard(raw: string) {
    const text = raw.trim();
    if (!text) {
      await this.done({ heard: undefined });
      return;
    }
    if (this.heardWhilePlaying && isLikelyEcho(text, this.deps.spoken())) {
      await this.done({ heard: { text, outcome: "echo" } });
      return;
    }
    const control = localCommand(text);
    if (control) {
      await this.applyControl(control);
      await this.done({
        heard: { text, outcome: "control" },
        ...(control === "end" ? { phase: "off" as VoicePhase } : {}),
      });
      return;
    }
    if (this.pending <= 1) this.set({ phase: "sending", partial: "" });
    try {
      const result = await this.deps.submit(text);
      if (result.action === "confirm") {
        // Nothing was sent: what was being read goes on.
        await this.done({
          confirm: { text, reason: result.reason || "" },
          heard: { text, outcome: "confirm" },
        });
        return;
      }
      // The message moves the conversation on: what the user talked over is not resumed.
      this.sentWhileHeld = this.held && this.barged;
      await this.done({
        heard: { text, outcome: "sent" },
        confirm: undefined,
        error: undefined,
      });
    } catch (error) {
      await this.failed(error instanceof Error ? error.message : String(error));
    }
  }
  private async applyControl(control: LocalCommand) {
    const speaker = this.deps.speaker;
    // Each of these decides what the speaker does next: the hold is over.
    const own = () => {
      this.held = false;
      this.barged = false;
      this.sentWhileHeld = false;
    };
    switch (control) {
      case "quiet":
        own();
        await speaker?.control("stop");
        await speaker?.control("resume");
        break;
      case "pause":
        own();
        await speaker?.control("pause");
        break;
      case "resume":
        own();
        await speaker?.control("resume");
        break;
      case "skip":
        own();
        await speaker?.control("skip");
        await speaker?.control("resume");
        break;
      case "repeat":
        own();
        await this.deps.repeat?.();
        await speaker?.control("resume");
        break;
      case "cancel":
        this.set({ confirm: undefined });
        break;
      case "end":
        await this.deps.capture.cancel();
        break;
    }
  }
  /** Resumes the speaker this session paused; `skip`: past the message it was reading. */
  private async letGo(skip: boolean) {
    if (!this.held || !this.deps.speaker) return;
    this.held = false;
    this.barged = false;
    if (skip) await this.deps.speaker.control("skip").catch(() => {});
    await this.deps.speaker.control("resume").catch(() => {});
  }
  /** This device's speaker reads the replies: started once, the first time voice is used. */
  private async startSpeaker() {
    if (this.speakerStarted || !this.deps.speaker) return;
    this.speakerStarted = true;
    try {
      await this.deps.speaker.live(true);
    } catch (error) {
      // An older gateway or a missing engine: voice still sends, nothing is read aloud here.
      this.speakerStarted = false;
      this.set({
        error:
          "Replies are not read aloud on this device: " +
          (error instanceof Error ? error.message : String(error)),
      });
    }
  }
  /** Push-to-talk pressed. Refused while a recording is made, recognized or sent. */
  async press() {
    if (!this.idle) return;
    this.starting = true;
    this.releaseWanted = false;
    const previous = this.s.phase;
    this.set({ mode: "push", phase: "hearing", error: undefined, partial: "" });
    try {
      if (this.deps.prepare && !(await this.deps.prepare())) {
        this.starting = false;
        this.set({ phase: previous === "error" ? "off" : previous });
        return;
      }
      void this.startSpeaker();
      await this.deps.capture.start("push", this.events(), this.deps.hint());
    } catch (error) {
      this.starting = false;
      this.pending = 0;
      await this.letGo(false);
      this.set({
        phase: "error",
        error: error instanceof Error ? error.message : String(error),
      });
      return;
    }
    this.starting = false;
    if (this.releaseWanted) await this.release();
  }
  /** Push-to-talk released: what was said is recognized and sent. */
  async release() {
    if (this.starting) {
      this.releaseWanted = true;
      return;
    }
    if (this.s.mode !== "push" || this.s.phase !== "hearing") return;
    this.set({ phase: "transcribing" });
    await this.deps.capture.stop();
    await this.busy;
  }
  /** A hands-free session: listens until `end`. */
  async begin() {
    if (!this.idle) return;
    this.starting = true;
    this.set({
      mode: "hands-free",
      phase: "listening",
      error: undefined,
      partial: "",
    });
    this.dropping = 0;
    this.listenAgain();
    try {
      if (this.deps.prepare && !(await this.deps.prepare())) {
        this.set({ phase: "off" });
        return;
      }
      await this.startSpeaker();
      await this.deps.capture.start(
        "hands-free",
        this.events(),
        this.deps.hint(),
      );
      // A reply may be read aloud already.
      this.updateDeaf();
    } catch (error) {
      this.set({
        phase: "error",
        error: error instanceof Error ? error.message : String(error),
      });
    } finally {
      this.starting = false;
    }
  }
  /** Ends a session (hands-free) or drops a recording (push). */
  async end() {
    this.releaseWanted = false;
    await this.deps.capture.cancel();
    this.pending = 0;
    this.dropping = 0;
    this.sentWhileHeld = false;
    await this.letGo(false);
    this.set({ phase: "off", level: 0, partial: "" });
    this.listenAgain();
  }
  /** Drops the transcript waiting for confirmation. */
  dismissConfirm() {
    this.set({ confirm: undefined });
  }
}

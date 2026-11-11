// The phone's half of voice mode (docs/mobile.md, Voice). The platform's own recognizer
// (iOS Speech, Android SpeechRecognizer) hears the user; the text goes to the orchestrator
// through `say` like a typed message. Replies are read aloud by a speaker the gateway runs
// for this phone on the hub's machine (`voice-speaker`): it sends each utterance here
// (`voice-speak`), the platform's voice says it (AVSpeechSynthesizer, TextToSpeech) and this
// module acks it. No audio leaves the phone; utterances only come from the hub's speech feed.
import { Capacitor, registerPlugin } from "@capacitor/core";
import type { DesktopEvent, Json } from "../../shared/types";
import type {
  NativeVoice,
  NativeVoiceEvent,
  SpeakerState,
  VoiceCaptureStatus,
  VoiceMode,
} from "../../shared/voice";

/** An event of the native plugin (`TwrVoice`, its one `voice` listener). */
export interface VoicePluginEvent {
  type:
    | "speechStart"
    | "partial"
    | "final"
    | "level"
    | "error"
    | "spoken"
    | "speakError";
  text?: string;
  level?: number;
  message?: string;
  /** The utterance (`spoken`, `speakError`). */
  id?: number;
  /** The error came from the app leaving the foreground: voice mode ends. */
  background?: boolean;
  /** iOS: a call, Siri or another app's audio interrupted listening and the voice. */
  interrupted?: boolean;
}

/** The native plugin: TwrVoicePlugin.swift (iOS), TwrVoicePlugin.java (Android). */
export interface VoicePlugin {
  status(): Promise<VoiceCaptureStatus>;
  start(options: { mode: VoiceMode; hint: string[] }): Promise<void>;
  stop(): Promise<void>;
  cancel(): Promise<void>;
  /** Says an utterance; `spoken` or `speakError` with its id follows. */
  speak(options: {
    id: number;
    text: string;
    voice?: string | null;
    rate?: number;
  }): Promise<void>;
  /** Stops the voice now: utterance `id` only, or whatever it says. Nothing is acked. */
  stopSpeaking(options: { id?: number }): Promise<void>;
  addListener(
    event: "voice",
    callback: (event: VoicePluginEvent) => void,
  ): Promise<{ remove(): Promise<void> }>;
}

/** The gateway connection: its calls and its events (a `RemoteHost`). */
export interface VoiceGateway {
  request(method: string, params?: Json, timeout?: number): Promise<any>;
  onEvent(callback: (event: DesktopEvent) => void): () => void;
}

type SpeakerOp = "start" | "stop" | "pause" | "resume" | "skip" | "silence";
export type MobileSpeakerState = SpeakerState & {
  running: boolean;
  name?: string;
};

/** The gateway's `voice-device-speaker` status, as the composer takes it. */
export function speakerState(value: any): MobileSpeakerState {
  return {
    running: value?.running === true,
    playing: value?.playing === true,
    paused: value?.paused === true,
    queued: Number.isFinite(value?.queued) ? Number(value.queued) : 0,
    ...(typeof value?.name === "string" ? { name: value.name } : {}),
  };
}

const SPEAKER_TIMEOUT = 15_000;

/**
 * Voice mode over the native plugin and the gateway's speaker. Recognition events pass
 * through; the gateway's utterances are said and acked; its speaker state becomes `speaker`
 * events. Pausing, skipping and silencing stop the platform's voice at once, before the
 * gateway answers (barge-in never waits for the network).
 */
export function createMobileVoice(
  plugin: VoicePlugin,
  gateway: VoiceGateway,
): NativeVoice & { dispose(): void } {
  const listeners = new Set<(event: NativeVoiceEvent) => void>();
  const emit = (event: NativeVoiceEvent) =>
    listeners.forEach((listener) => listener(event));
  let running = false;
  const ack = (id: number, error?: string) =>
    gateway
      .request(
        "voice-speaker",
        error ? { op: "failed", id, error } : { op: "said", id },
        SPEAKER_TIMEOUT,
      )
      .catch(() => {});
  const speaker = async (op: SpeakerOp): Promise<MobileSpeakerState> => {
    if (op === "pause" || op === "skip" || op === "silence" || op === "stop")
      void plugin.stopSpeaking({}).catch(() => {});
    const state = speakerState(
      await gateway.request("voice-speaker", { op }, SPEAKER_TIMEOUT),
    );
    running = state.running;
    return state;
  };
  const native = plugin.addListener("voice", (event) => {
    switch (event.type) {
      case "spoken":
        if (Number.isSafeInteger(event.id)) void ack(event.id!);
        return;
      case "speakError":
        if (Number.isSafeInteger(event.id))
          void ack(event.id!, event.message || "The phone could not say it");
        return;
      case "error":
        emit({ type: "error", message: event.message || "Voice failed" });
        // Voice mode is foreground only: leaving the app ends this phone's speaker too.
        if (event.background && running) void speaker("stop").catch(() => {});
        return;
      case "speechStart":
      case "partial":
      case "final":
      case "level":
        emit({
          type: event.type,
          text: event.text,
          level: event.level,
        });
    }
  });
  const offGateway = gateway.onEvent((event) => {
    const value = event as DesktopEvent & {
      id?: number;
      text?: string;
      voice?: string | null;
      rate?: number;
    };
    if (value.type === "voice-speak") {
      const id = value.id;
      if (!Number.isSafeInteger(id) || typeof value.text !== "string") return;
      plugin
        .speak({
          id: id!,
          text: value.text,
          voice: typeof value.voice === "string" ? value.voice : null,
          rate: Number.isFinite(value.rate) ? value.rate : 0,
        })
        .catch((error) => void ack(id!, String(error?.message || error)));
    } else if (value.type === "voice-speak-stop") {
      if (Number.isSafeInteger(value.id))
        void plugin.stopSpeaking({ id: value.id }).catch(() => {});
    } else if (value.type === "voice-device-speaker" && value.speaker) {
      const state = speakerState(value.speaker);
      running = state.running;
      if (!state.running) void plugin.stopSpeaking({}).catch(() => {});
      emit({ type: "speaker", speaker: state });
    }
  });
  return {
    status: () => plugin.status(),
    start: (options) => plugin.start(options),
    stop: () => plugin.stop(),
    cancel: () => plugin.cancel(),
    onEvent(callback) {
      listeners.add(callback);
      return () => {
        listeners.delete(callback);
      };
    },
    speaker,
    dispose() {
      offGateway();
      listeners.clear();
      void native.then((handle) => handle.remove()).catch(() => {});
      void plugin.stopSpeaking({}).catch(() => {});
      void plugin.cancel().catch(() => {});
    },
  };
}

let plugin: VoicePlugin | undefined;

/** Voice mode for this hub connection, where the app has the native plugin. */
export function mobileVoice(gateway: VoiceGateway) {
  if (!Capacitor.isNativePlatform() || !Capacitor.isPluginAvailable("TwrVoice"))
    return undefined;
  plugin ||= registerPlugin<VoicePlugin>("TwrVoice");
  return createMobileVoice(plugin, gateway);
}

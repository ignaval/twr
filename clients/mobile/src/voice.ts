// The phone's half of voice mode (docs/mobile.md, Voice). The platform's own recognizer
// (iOS Speech, Android SpeechRecognizer) hears the user; the text goes to the orchestrator
// through `say` like a typed message. Replies are read aloud by a speaker the gateway runs
// for this phone on the hub's machine (`voice-speaker`): it sends each utterance here
// (`voice-speak`), the platform's voice says it (AVSpeechSynthesizer, TextToSpeech) and this
// module acks it. With one of twr's natural voices chosen, the hub's computer makes the audio
// and sends it instead (`voice-audio`), which the plugin plays and this module acks the same
// way. No audio leaves the phone; utterances only come from the hub's speech feed.
// `readAloudVoices` is Settings › Read-aloud voice: the phone's own voices and twr's natural
// ones, the choice kept by the gateway for this phone's speaker.
import { Capacitor, registerPlugin } from "@capacitor/core";
import type { DesktopEvent, Json } from "../../shared/types";
import type {
  ReadAloudVoices,
  ReadAloudVoicesEvent,
  VoiceListing,
  VoiceOption,
} from "../../shared/readAloudVoices";
import { previewText, voiceLanguages } from "../../shared/readAloudVoices";
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
  /**
   * Says an utterance; `spoken` or `speakError` with its id follows. Without `voice` (or one
   * the phone doesn't have), a voice of `lang`.
   */
  speak(options: {
    id: number;
    text: string;
    voice?: string | null;
    rate?: number;
    lang?: string | null;
  }): Promise<void>;
  /**
   * Plays one piece of an utterance's audio (16-bit little-endian mono PCM, base64): pieces
   * of one id back to back; a first piece of a new id replaces what plays. `spoken` follows
   * the last piece once it has played; a rejected piece ends the utterance with no event.
   */
  playAudio(options: {
    id: number;
    seq: number;
    pcm: string;
    sampleRate: number;
    last: boolean;
  }): Promise<void>;
  /** The phone's own voices. */
  voices(): Promise<{ voices: PhoneVoice[] }>;
  /** Stops the voice now: utterance `id` only, or whatever it says. Nothing is acked. */
  stopSpeaking(options: { id?: number }): Promise<void>;
  addListener(
    event: "voice",
    callback: (event: VoicePluginEvent) => void,
  ): Promise<{ remove(): Promise<void> }>;
}

/** A voice of the phone's (TwrVoicePlugin `voices`). */
export interface PhoneVoice {
  id: string;
  name: string;
  /** BCP-47 (`en-US`). */
  language: string;
  quality: "premium" | "enhanced" | "default";
  /** Android: the text goes to its maker's servers. */
  network?: boolean;
  /** Android: false until its data is downloaded on the phone. */
  installed?: boolean;
}

/** The phone's voices as Settings lists them: those of the languages twr tells apart. */
export function phoneOptions(voices: PhoneVoice[]): VoiceOption[] {
  return voices
    .filter((v) => v.installed !== false)
    .map((v) => ({
      v,
      lang: v.language.split(/[-_]/)[0].toLowerCase(),
    }))
    .filter(({ lang }) => (voiceLanguages as readonly string[]).includes(lang))
    .map(({ v, lang }) => ({
      id: v.id,
      name: v.name,
      engine: "system" as const,
      languages: [lang],
      locale: v.language,
      quality: v.quality,
      ready: true,
      ...(v.network ? { network: true } : {}),
    }));
}

/** The gateway's `voice-voices` and the phone's voices, as Settings shows them. */
export function mobileListing(
  gateway: any,
  phone: PhoneVoice[],
  platform: string,
): VoiceListing {
  const natural = gateway?.natural || {};
  const byLanguage: VoiceListing["byLanguage"] = {};
  for (const lang of voiceLanguages) {
    const v = gateway?.by_language?.[lang];
    if (typeof v === "string" && v) byLanguage[lang] = v;
  }
  return {
    voices: [
      ...phoneOptions(phone),
      ...(Array.isArray(natural.voices) ? natural.voices : []),
    ],
    models: Array.isArray(natural.models)
      ? natural.models.map((m: any) => ({
          ...m,
          needs: typeof m.needs === "string" ? m.needs : undefined,
        }))
      : [],
    byLanguage,
    platform,
    naturalWhere: "hub",
    canInstall: gateway?.can_install === true,
    ...(typeof natural.error === "string"
      ? { naturalProblem: natural.error }
      : {}),
  };
}

/** One of twr's natural voices (`<model>/<voice>`): the hub's computer makes it. */
export const isNatural = (voice: string) => voice.includes("/");

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
  platform = "ios",
): NativeVoice & { voices: ReadAloudVoices; dispose(): void } {
  const listeners = new Set<(event: NativeVoiceEvent) => void>();
  const voiceListeners = new Set<(event: ReadAloudVoicesEvent) => void>();
  const voiceEvent = (event: ReadAloudVoicesEvent) =>
    voiceListeners.forEach((listener) => listener(event));
  // The preview playing: its utterance id (negative: said by the phone from its own text,
  // never acked to the gateway) and voice.
  let previewing: { id: number; voice: string } | undefined;
  let localPreview = 0;
  const endPreview = (id?: number) => {
    if (!previewing || (id !== undefined && previewing.id !== id)) return;
    previewing = undefined;
    voiceEvent({ type: "preview" });
  };
  const emit = (event: NativeVoiceEvent) =>
    listeners.forEach((listener) => listener(event));
  let running = false;
  const ack = (id: number, error?: string) => {
    endPreview(id);
    // A preview the phone says by itself is no utterance of the gateway's.
    if (id < 0) return Promise.resolve();
    return gateway
      .request(
        "voice-speaker",
        error ? { op: "failed", id, error } : { op: "said", id },
        SPEAKER_TIMEOUT,
      )
      .catch(() => {});
  };
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
      lang?: string | null;
      seq?: number;
      pcm?: string;
      last?: boolean;
      install?: any;
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
          lang: typeof value.lang === "string" ? value.lang : null,
        })
        .catch((error) => void ack(id!, String(error?.message || error)));
    } else if (value.type === "voice-audio") {
      const id = value.id;
      if (
        !Number.isSafeInteger(id) ||
        !Number.isSafeInteger(value.seq) ||
        typeof value.pcm !== "string" ||
        !Number.isFinite(value.rate)
      )
        return;
      plugin
        .playAudio({
          id: id!,
          seq: value.seq!,
          pcm: value.pcm,
          sampleRate: value.rate!,
          last: value.last === true,
        })
        .catch((error) => void ack(id!, String(error?.message || error)));
    } else if (value.type === "voice-preview-error") {
      if (Number.isSafeInteger(value.id)) {
        void plugin.stopSpeaking({ id: value.id }).catch(() => {});
        endPreview(value.id);
      }
    } else if (value.type === "voice-install" && value.install) {
      voiceEvent({ type: "install", install: value.install });
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
    async start(options) {
      if (options.mode === "one-shot")
        throw new Error("One-shot voice capture is not supported on mobile");
      await plugin.start(options);
    },
    stop: () => plugin.stop(),
    cancel: () => plugin.cancel(),
    onEvent(callback) {
      listeners.add(callback);
      return () => {
        listeners.delete(callback);
      };
    },
    speaker,
    voices: {
      async list() {
        const [hub, phone] = await Promise.all([
          gateway.request("voice-voices", {}, 45_000),
          plugin.voices().catch(() => ({ voices: [] as PhoneVoice[] })),
        ]);
        return mobileListing(hub, phone.voices || [], platform);
      },
      async select(lang, voice) {
        await gateway.request("voice-select", { lang, voice }, SPEAKER_TIMEOUT);
        return this.list();
      },
      async install(model) {
        // The gateway starts the download and answers; its end comes as an event (a model is
        // hundreds of MB: as long as the hub's computer takes, up to an hour here).
        let off = () => {};
        const finished = new Promise<void>((resolve, reject) => {
          const timer = setTimeout(
            () => reject(new Error("The download is taking too long")),
            60 * 60_000,
          );
          const stop = gateway.onEvent((event) => {
            const install = (event as any).install;
            if (event.type !== "voice-install" || install?.model !== model)
              return;
            if (install.phase !== "done" && install.phase !== "error") return;
            off();
            if (install.phase === "done") resolve();
            else reject(new Error(install.error || "The download failed"));
          });
          off = () => {
            clearTimeout(timer);
            stop();
          };
        });
        try {
          await gateway.request("voice-install", { model }, SPEAKER_TIMEOUT);
        } catch (error) {
          off();
          throw error;
        }
        await finished;
        return this.list();
      },
      async preview(voice, lang) {
        await this.stopPreview();
        // The voice reading replies stops: one voice at a time.
        if (running) await speaker("silence").catch(() => {});
        if (voice && isNatural(voice)) {
          const { id } = await gateway.request(
            "voice-preview",
            { voice, lang },
            SPEAKER_TIMEOUT,
          );
          previewing = { id, voice };
        } else {
          const id = --localPreview;
          previewing = { id, voice: voice || `default:${lang}` };
          await plugin
            .speak({ id, text: previewText[lang], voice: voice || null, lang })
            .catch((error) => {
              endPreview(id);
              throw error;
            });
        }
        voiceEvent({ type: "preview", voice: previewing.voice });
      },
      async stopPreview() {
        const was = previewing;
        if (!was) return;
        endPreview();
        if (was.id > 0)
          await gateway
            .request("voice-preview-stop", {}, SPEAKER_TIMEOUT)
            .catch(() => {});
        await plugin.stopSpeaking({ id: was.id }).catch(() => {});
      },
      onEvent(callback) {
        voiceListeners.add(callback);
        return () => {
          voiceListeners.delete(callback);
        };
      },
    },
    dispose() {
      offGateway();
      listeners.clear();
      voiceListeners.clear();
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
  return createMobileVoice(plugin, gateway, Capacitor.getPlatform());
}

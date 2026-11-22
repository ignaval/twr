import { describe, expect, it, vi } from "vitest";
vi.mock("@capacitor/core", () => ({
  Capacitor: {
    isNativePlatform: () => false,
    isPluginAvailable: () => false,
  },
  registerPlugin: () => ({}),
}));
import {
  createMobileVoice,
  mobileVoice,
  speakerState,
  type VoicePlugin,
  type VoicePluginEvent,
} from "../mobile/src/voice";
import { createNativeCapture, type NativeVoiceEvent } from "../shared/voice";
import type { DesktopEvent } from "../shared/types";

const flush = () => new Promise((resolve) => setTimeout(resolve, 0));

const hubVoices = {
  by_language: { es: "kokoro/ef_dora" },
  can_install: true,
  natural: {
    voices: [
      {
        id: "kokoro/ef_dora",
        name: "Kokoro Dora",
        engine: "twr",
        model: "kokoro",
        languages: ["es"],
        locale: "es-ES",
        quality: "neural",
        ready: true,
      },
    ],
    models: [
      {
        name: "kokoro",
        title: "Kokoro",
        installed: true,
        bytes: 1,
        size: "350 MB",
        license: "Apache-2.0",
        needs: null,
      },
    ],
  },
};

function rig(
  options: { speakFails?: string; audioFails?: string; hub?: any } = {},
) {
  let native: ((event: VoicePluginEvent) => void) | undefined;
  const removed = vi.fn(async () => {});
  const plugin = {
    status: vi.fn(async () => ({ available: true, local: true })),
    start: vi.fn(async () => {}),
    stop: vi.fn(async () => {}),
    cancel: vi.fn(async () => {}),
    speak: vi.fn(async () => {
      if (options.speakFails) throw new Error(options.speakFails);
    }),
    stopSpeaking: vi.fn(async () => {}),
    playAudio: vi.fn(async () => {
      if (options.audioFails) throw new Error(options.audioFails);
    }),
    voices: vi.fn(async () => ({
      voices: [
        {
          id: "com.apple.voice.premium.en-US.Ava",
          name: "Ava",
          language: "en-US",
          quality: "premium" as const,
        },
        {
          id: "es-us-x-esd-network",
          name: "Spanish (United States) · ESD",
          language: "es-US",
          quality: "enhanced" as const,
          network: true,
        },
        {
          id: "fr-fr-x-frd-local",
          name: "French",
          language: "fr-FR",
          quality: "default" as const,
        },
        {
          id: "es-es-x-eed-local",
          name: "Spanish (Spain)",
          language: "es-ES",
          quality: "default" as const,
          installed: false,
        },
      ],
    })),
    addListener: vi.fn(async (_name: "voice", callback) => {
      native = callback;
      return { remove: removed };
    }),
  } satisfies VoicePlugin;
  const gatewayListeners = new Set<(event: DesktopEvent) => void>();
  const status = {
    running: true,
    playing: false,
    paused: false,
    queued: 0,
    live: true,
    name: "Pixel phone",
    workspace: "default",
  };
  const request = vi.fn(async (method: string, params?: any) =>
    method === "voice-voices"
      ? (options.hub ?? hubVoices)
      : method === "voice-select"
        ? { by_language: { [params.lang]: params.voice } }
        : method === "voice-preview"
          ? { id: 41 }
          : method === "voice-install" || method === "voice-preview-stop"
            ? null
            : params?.op === "said" || params?.op === "failed"
              ? { acked: true }
              : { ...status, running: params?.op !== "stop" },
  );
  const voice = createMobileVoice(plugin, {
    request,
    onEvent: (callback) => {
      gatewayListeners.add(callback);
      return () => gatewayListeners.delete(callback);
    },
  });
  const events: NativeVoiceEvent[] = [];
  voice.onEvent((event) => events.push(event));
  return {
    voice,
    plugin,
    request,
    events,
    removed,
    native: (event: VoicePluginEvent) => native!(event),
    gateway: (event: Record<string, unknown>) =>
      gatewayListeners.forEach((listener) =>
        listener(event as unknown as DesktopEvent),
      ),
    gatewayListeners,
  };
}

describe("mobile voice", () => {
  it("rejects direct one-shot calls without changing ordinary native modes", async () => {
    const r = rig();
    await expect(r.voice.start({ mode: "one-shot", hint: [] })).rejects.toThrow(
      "not supported",
    );
    expect(r.plugin.start).not.toHaveBeenCalled();
    await r.voice.start({ mode: "push", hint: ["api"] });
    expect(r.plugin.start).toHaveBeenCalledExactlyOnceWith({
      mode: "push",
      hint: ["api"],
    });
    r.voice.dispose();
  });

  it("passes recognition events through to the composer's capture", async () => {
    const r = rig();
    await flush();
    const capture = createNativeCapture(r.voice);
    const heard: string[] = [];
    await capture.start(
      "hands-free",
      {
        speechStart: () => heard.push("start"),
        partial: (text) => heard.push(`partial:${text}`),
        final: (text) => heard.push(`final:${text}`),
        level: (level) => heard.push(`level:${level}`),
        error: (message) => heard.push(`error:${message}`),
      },
      ["api", "web"],
    );
    expect(r.plugin.start).toHaveBeenCalledWith({
      mode: "hands-free",
      hint: ["api", "web"],
    });
    r.native({ type: "speechStart" });
    r.native({ type: "level", level: 0.4 });
    r.native({ type: "partial", text: "ask the" });
    r.native({ type: "final", text: "ask the api thread" });
    r.native({ type: "error", message: "Speech recognition is off" });
    expect(heard).toEqual([
      "start",
      "level:0.4",
      "partial:ask the",
      "final:ask the api thread",
      "error:Speech recognition is off",
    ]);
    await capture.stop();
    expect(r.plugin.stop).toHaveBeenCalled();
    await capture.cancel();
    expect(r.plugin.cancel).toHaveBeenCalled();
    expect(await r.voice.status()).toEqual({ available: true, local: true });
  });

  it("says the gateway's utterances and acks each one", async () => {
    const r = rig();
    await flush();
    r.gateway({
      type: "voice-speak",
      id: 7,
      text: "The tests pass.",
      voice: "Ava",
      rate: 180,
    });
    expect(r.plugin.speak).toHaveBeenCalledWith({
      id: 7,
      text: "The tests pass.",
      voice: "Ava",
      rate: 180,
      // An older gateway names no language: the phone's own then.
      lang: null,
    });
    r.native({ type: "spoken", id: 7 });
    expect(r.request).toHaveBeenCalledWith(
      "voice-speaker",
      { op: "said", id: 7 },
      15000,
    );
    r.native({ type: "speakError", id: 8, message: "No voice installed" });
    expect(r.request).toHaveBeenCalledWith(
      "voice-speaker",
      { op: "failed", id: 8, error: "No voice installed" },
      15000,
    );
    // A malformed utterance is ignored, never said.
    r.gateway({ type: "voice-speak", id: "x", text: "hi" });
    r.gateway({ type: "voice-speak", id: 9 });
    expect(r.plugin.speak).toHaveBeenCalledTimes(1);
    r.gateway({ type: "voice-speak-stop", id: 7 });
    expect(r.plugin.stopSpeaking).toHaveBeenCalledWith({ id: 7 });
    // Recognition events never ack anything.
    expect(r.events).toEqual([]);
  });

  it("acks a failure when the platform voice cannot start", async () => {
    const r = rig({ speakFails: "TextToSpeech is not ready" });
    await flush();
    r.gateway({ type: "voice-speak", id: 3, text: "Hello." });
    await flush();
    expect(r.request).toHaveBeenCalledWith(
      "voice-speaker",
      { op: "failed", id: 3, error: "TextToSpeech is not ready" },
      15000,
    );
  });

  it("maps the gateway speaker state to speaker events", async () => {
    const r = rig();
    await flush();
    r.gateway({
      type: "voice-device-speaker",
      speaker: {
        running: true,
        playing: true,
        paused: false,
        queued: 2,
        live: true,
        name: "Pixel phone",
      },
    });
    expect(r.events).toEqual([
      {
        type: "speaker",
        speaker: {
          running: true,
          playing: true,
          paused: false,
          queued: 2,
          name: "Pixel phone",
        },
      },
    ]);
    expect(r.plugin.stopSpeaking).not.toHaveBeenCalled();
    // A speaker that ended silences the platform voice.
    r.gateway({ type: "voice-device-speaker", speaker: { running: false } });
    expect(r.events[1]).toEqual({
      type: "speaker",
      speaker: { running: false, playing: false, paused: false, queued: 0 },
    });
    expect(r.plugin.stopSpeaking).toHaveBeenCalledWith({});
    expect(speakerState({ queued: "lots" }).queued).toBe(0);
  });

  it("sends speaker ops to the gateway and stops the voice at once for barge-in", async () => {
    const r = rig();
    await flush();
    const started = await r.voice.speaker!("start");
    expect(started).toEqual({
      running: true,
      playing: false,
      paused: false,
      queued: 0,
      name: "Pixel phone",
    });
    expect(r.request).toHaveBeenLastCalledWith(
      "voice-speaker",
      { op: "start" },
      15000,
    );
    expect(r.plugin.stopSpeaking).not.toHaveBeenCalled();
    for (const op of ["pause", "skip", "silence"] as const) {
      r.plugin.stopSpeaking.mockClear();
      await r.voice.speaker!(op);
      expect(r.plugin.stopSpeaking).toHaveBeenCalledWith({});
      expect(r.request).toHaveBeenLastCalledWith(
        "voice-speaker",
        { op },
        15000,
      );
    }
    r.plugin.stopSpeaking.mockClear();
    await r.voice.speaker!("resume");
    expect(r.plugin.stopSpeaking).not.toHaveBeenCalled();
    const stopped = await r.voice.speaker!("stop");
    expect(stopped.running).toBe(false);
  });

  it("ends this phone's speaker when the app leaves the foreground", async () => {
    const r = rig();
    await flush();
    await r.voice.speaker!("start");
    r.native({
      type: "error",
      message: "Voice mode stops while the app is in the background",
      background: true,
    });
    await flush();
    expect(r.events).toContainEqual({
      type: "error",
      message: "Voice mode stops while the app is in the background",
    });
    expect(r.request).toHaveBeenLastCalledWith(
      "voice-speaker",
      { op: "stop" },
      15000,
    );
  });

  it("lets go of everything when the hub connection closes", async () => {
    const r = rig();
    await flush();
    r.voice.dispose();
    await flush();
    expect(r.gatewayListeners.size).toBe(0);
    expect(r.removed).toHaveBeenCalled();
    expect(r.plugin.cancel).toHaveBeenCalled();
    r.native({ type: "final", text: "late" });
    expect(r.events).toEqual([]);
  });

  it("is only offered where the native plugin exists", () => {
    expect(
      mobileVoice({ request: async () => ({}), onEvent: () => () => {} }),
    ).toBeUndefined();
  });

  it("plays a natural voice's audio pieces and acks the utterance like a spoken one", async () => {
    const r = rig();
    await flush();
    r.gateway({
      type: "voice-audio",
      id: 5,
      seq: 0,
      rate: 24000,
      pcm: "AAAA",
      last: false,
    });
    r.gateway({
      type: "voice-audio",
      id: 5,
      seq: 1,
      rate: 24000,
      pcm: "",
      last: true,
    });
    expect(r.plugin.playAudio).toHaveBeenNthCalledWith(1, {
      id: 5,
      seq: 0,
      pcm: "AAAA",
      sampleRate: 24000,
      last: false,
    });
    expect(r.plugin.playAudio).toHaveBeenNthCalledWith(2, {
      id: 5,
      seq: 1,
      pcm: "",
      sampleRate: 24000,
      last: true,
    });
    r.native({ type: "spoken", id: 5 });
    expect(r.request).toHaveBeenCalledWith(
      "voice-speaker",
      { op: "said", id: 5 },
      15000,
    );
    // Malformed audio is ignored.
    r.gateway({
      type: "voice-audio",
      id: 6,
      seq: "0",
      rate: 24000,
      pcm: "AAAA",
    });
    r.gateway({ type: "voice-audio", id: 6, seq: 0, rate: 24000 });
    expect(r.plugin.playAudio).toHaveBeenCalledTimes(2);
  });

  it("acks a failure when the phone refuses a piece of audio", async () => {
    const r = rig({ audioFails: "A piece of the audio went missing" });
    await flush();
    r.gateway({
      type: "voice-audio",
      id: 4,
      seq: 0,
      rate: 24000,
      pcm: "AAAA",
      last: true,
    });
    await flush();
    expect(r.request).toHaveBeenCalledWith(
      "voice-speaker",
      { op: "failed", id: 4, error: "A piece of the audio went missing" },
      15000,
    );
  });

  it("passes a message's language to the phone's voice", async () => {
    const r = rig();
    await flush();
    r.gateway({
      type: "voice-speak",
      id: 2,
      text: "Hola.",
      voice: null,
      rate: 0,
      lang: "es",
    });
    expect(r.plugin.speak).toHaveBeenCalledWith({
      id: 2,
      text: "Hola.",
      voice: null,
      rate: 0,
      lang: "es",
    });
  });

  it("lists the phone's voices of English and Spanish with the hub's natural ones", async () => {
    const r = rig();
    const listing = await r.voice.voices.list();
    expect(listing.voices.map((v) => [v.id, v.engine, v.languages])).toEqual([
      ["com.apple.voice.premium.en-US.Ava", "system", ["en"]],
      ["es-us-x-esd-network", "system", ["es"]],
      ["kokoro/ef_dora", "twr", ["es"]],
    ]);
    expect(listing.voices[1].network).toBe(true);
    expect(listing.byLanguage).toEqual({ es: "kokoro/ef_dora" });
    expect(listing.naturalWhere).toBe("hub");
    expect(listing.canInstall).toBe(true);
    expect(listing.models[0].needs).toBeUndefined();
    // A hub without natural voices still lists the phone's.
    const bare = await rig({
      hub: {
        by_language: {},
        natural: { voices: [], models: [], error: "no `twr voices`" },
      },
    }).voice.voices.list();
    expect(bare.voices).toHaveLength(2);
    expect(bare.naturalProblem).toBe("no `twr voices`");
    expect(bare.canInstall).toBe(false);
  });

  it("previews a phone voice on the phone and a natural one through the gateway", async () => {
    const r = rig();
    await flush();
    const seen: unknown[] = [];
    r.voice.voices.onEvent((e) => seen.push(e));
    await r.voice.voices.preview("com.apple.voice.premium.en-US.Ava", "en");
    const local = (r.plugin.speak.mock.calls[0] as any)[0];
    expect(local.id).toBeLessThan(0);
    expect(local.voice).toBe("com.apple.voice.premium.en-US.Ava");
    expect(local.text).toContain("read your replies aloud");
    // Its end is no ack of the gateway's.
    r.native({ type: "spoken", id: local.id });
    expect(r.request).not.toHaveBeenCalledWith(
      "voice-speaker",
      expect.objectContaining({ op: "said" }),
      15000,
    );
    expect(seen).toEqual([
      { type: "preview", voice: "com.apple.voice.premium.en-US.Ava" },
      { type: "preview" },
    ]);
    await r.voice.voices.preview("kokoro/ef_dora", "es");
    expect(r.request).toHaveBeenCalledWith(
      "voice-preview",
      { voice: "kokoro/ef_dora", lang: "es" },
      15000,
    );
    await r.voice.voices.stopPreview();
    expect(r.request).toHaveBeenCalledWith("voice-preview-stop", {}, 15000);
    expect(r.plugin.stopSpeaking).toHaveBeenLastCalledWith({ id: 41 });
  });

  it("forwards the hub's download progress and saves a choice through the gateway", async () => {
    const r = rig();
    await flush();
    const seen: unknown[] = [];
    r.voice.voices.onEvent((e) => seen.push(e));
    r.gateway({
      type: "voice-install",
      install: { model: "kokoro", phase: "downloading", bytes: 1, total: 2 },
    });
    expect(seen).toEqual([
      {
        type: "install",
        install: { model: "kokoro", phase: "downloading", bytes: 1, total: 2 },
      },
    ]);
    await r.voice.voices.select("en", "com.apple.voice.premium.en-US.Ava");
    expect(r.request).toHaveBeenCalledWith(
      "voice-select",
      { lang: "en", voice: "com.apple.voice.premium.en-US.Ava" },
      15000,
    );
    const installing = r.voice.voices.install("kokoro");
    await flush();
    expect(r.request).toHaveBeenCalledWith(
      "voice-install",
      { model: "kokoro" },
      15000,
    );
    // Another model's end, then this one's: only its own ends the wait.
    r.gateway({
      type: "voice-install",
      install: { model: "supertonic", phase: "done" },
    });
    r.gateway({
      type: "voice-install",
      install: { model: "kokoro", phase: "done" },
    });
    expect((await installing).voices.length).toBeGreaterThan(0);
    const failing = r.voice.voices.install("piper-en");
    await flush();
    r.gateway({
      type: "voice-install",
      install: { model: "piper-en", phase: "error", error: "disk full" },
    });
    await expect(failing).rejects.toThrow("disk full");
  });
});

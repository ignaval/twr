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

function rig(options: { speakFails?: string } = {}) {
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
  const request = vi.fn(async (_method: string, params?: any) =>
    params?.op === "said" || params?.op === "failed"
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
});

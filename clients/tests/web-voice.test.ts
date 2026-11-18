import { afterEach, describe, expect, it, vi } from "vitest";
import {
  createNativeCapture,
  createWebCapture,
  type VoiceCaptureEvents,
  type VoiceMode,
} from "../shared/voice";

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (error: Error) => void;
  const promise = new Promise<T>((yes, no) => {
    resolve = yes;
    reject = no;
  });
  return { promise, resolve, reject };
}
const flush = () => new Promise((resolve) => setTimeout(resolve, 0));
const audio = (ms: number, amplitude = 0.2) =>
  Float32Array.from({ length: ms * 16 }, (_, i) =>
    i % 20 < 10 ? amplitude : -amplitude,
  );
const events = () =>
  ({
    speechStart: vi.fn(),
    final: vi.fn(),
    error: vi.fn(),
    level: vi.fn(),
  }) satisfies VoiceCaptureEvents;
function rig() {
  const contexts: FakeContext[] = [];
  class FakeContext {
    sampleRate = 16000;
    destination = {};
    node = {
      onaudioprocess: null as ((event: AudioProcessingEvent) => void) | null,
      connect: vi.fn(),
      disconnect: vi.fn(),
    };
    source = { connect: vi.fn(), disconnect: vi.fn() };
    close = vi.fn(async () => {});
    createMediaStreamSource = vi.fn(() => this.source);
    createScriptProcessor = vi.fn(() => this.node);
    constructor() {
      contexts.push(this);
    }
  }
  const microphone = () => {
    const stop = vi.fn();
    return {
      stop,
      stream: { getTracks: () => [{ stop }] } as unknown as MediaStream,
    };
  };
  const first = microphone();
  const getUserMedia = vi.fn(async () => first.stream);
  vi.stubGlobal("navigator", { mediaDevices: { getUserMedia } });
  vi.stubGlobal("AudioContext", FakeContext);
  const transcribe = vi.fn(
    async (_wav: Uint8Array, _hint: string[]) => "run the tests",
  );
  const capture = createWebCapture(transcribe, async () => ({
    available: true,
    local: true,
    pauseMs: 500,
  }));
  const feed = (samples: Float32Array, index = contexts.length - 1) =>
    contexts[index].node.onaudioprocess?.({
      inputBuffer: { getChannelData: () => samples },
    } as unknown as AudioProcessingEvent);
  const utterance = () => {
    feed(audio(600));
    feed(audio(600, 0.001));
  };
  const start = async (
    mode: VoiceMode = "one-shot",
    e = events(),
    hints = ["api"],
  ) => {
    await capture.status();
    await capture.start(mode, e, hints);
    return e;
  };
  return {
    capture,
    transcribe,
    contexts,
    first,
    microphone,
    getUserMedia,
    feed,
    utterance,
    start,
  };
}
afterEach(() => vi.unstubAllGlobals());

describe("browser one-shot capture", () => {
  it("ends at configured silence and releases all audio before inference and its single final", async () => {
    const r = rig();
    const e = await r.start();
    r.transcribe.mockImplementation(async () => {
      expect(r.first.stop).toHaveBeenCalledTimes(1);
      expect(r.contexts[0].node.disconnect).toHaveBeenCalledOnce();
      expect(r.contexts[0].source.disconnect).toHaveBeenCalledOnce();
      expect(r.contexts[0].close).toHaveBeenCalledOnce();
      return "run the tests";
    });
    r.feed(audio(600));
    r.feed(audio(400, 0.001));
    expect(r.first.stop).not.toHaveBeenCalled();
    expect(r.transcribe).not.toHaveBeenCalled();
    r.feed(audio(120, 0.001));
    expect(r.first.stop).toHaveBeenCalledOnce();
    await flush();
    expect(e.speechStart).toHaveBeenCalledOnce();
    expect(e.final).toHaveBeenCalledExactlyOnceWith("run the tests");
    expect(r.transcribe.mock.calls[0][1]).toEqual(["api"]);
    expect(
      new DataView(r.transcribe.mock.calls[0][0].buffer).getUint32(24, true),
    ).toBe(16000);
    await r.capture.stop();
    await r.capture.stop();
    expect(r.transcribe).toHaveBeenCalledOnce();
  });
  it("takes only the first of multiple utterances within one callback", async () => {
    const r = rig();
    const e = await r.start();
    const all = new Float32Array(2400 * 16);
    all.set(audio(600));
    all.set(audio(600, 0.001), 600 * 16);
    all.set(audio(600), 1200 * 16);
    all.set(audio(600, 0.001), 1800 * 16);
    r.feed(all);
    r.utterance();
    await flush();
    expect(e.speechStart).toHaveBeenCalledOnce();
    expect(e.final).toHaveBeenCalledOnce();
    expect(r.transcribe).toHaveBeenCalledOnce();
  });
  it("waits for speech through initial silence and clicks", async () => {
    const r = rig();
    const e = await r.start();
    r.feed(audio(2000, 0.001));
    r.feed(audio(40));
    r.feed(audio(1000, 0.001));
    await flush();
    expect(e.final).not.toHaveBeenCalled();
    expect(r.first.stop).not.toHaveBeenCalled();
    await r.capture.cancel();
  });
  it("stops a microphone granted after cancellation without opening an audio context", async () => {
    const r = rig();
    const permission = deferred<MediaStream>();
    r.getUserMedia.mockReturnValueOnce(permission.promise);
    const e = events();
    const starting = r.capture.start("one-shot", e, []);
    await r.capture.cancel();
    permission.resolve(r.first.stream);
    await starting;
    expect(r.first.stop).toHaveBeenCalledOnce();
    expect(r.contexts).toHaveLength(0);
    expect(e.speechStart).not.toHaveBeenCalled();
    expect(e.final).not.toHaveBeenCalled();
  });
  it("cancels before permission is requested while the previous context closes", async () => {
    const r = rig();
    await r.start();
    const closing = deferred<void>();
    r.contexts[0].close.mockReturnValueOnce(closing.promise);
    const e = events();
    const starting = r.capture.start("one-shot", e, []);
    await r.capture.cancel();
    closing.resolve();
    await starting;
    expect(r.getUserMedia).toHaveBeenCalledOnce();
    expect(e.speechStart).not.toHaveBeenCalled();
  });
  it("cancels active audio and ignores queued processor callbacks", async () => {
    const r = rig();
    const e = await r.start();
    r.feed(audio(600));
    const stale = r.contexts[0].node.onaudioprocess!;
    await r.capture.cancel();
    stale({
      inputBuffer: { getChannelData: () => audio(1000, 0.001) },
    } as unknown as AudioProcessingEvent);
    await flush();
    expect(r.first.stop).toHaveBeenCalledOnce();
    expect(e.final).not.toHaveBeenCalled();
    expect(r.transcribe).not.toHaveBeenCalled();
  });
  it.each([false, true])(
    "drops cancelled inference completion (reject: %s)",
    async (reject) => {
      const r = rig();
      const result = deferred<string>();
      r.transcribe.mockReturnValueOnce(result.promise);
      const e = await r.start();
      r.utterance();
      await flush();
      expect(r.transcribe).toHaveBeenCalledOnce();
      await r.capture.cancel();
      if (reject) result.reject(new Error("cancelled inference"));
      else result.resolve("stale text");
      await flush();
      expect(e.final).not.toHaveBeenCalled();
      expect(e.error).not.toHaveBeenCalled();
    },
  );
  it("isolates a replacement from old inference, hints and processor callbacks", async () => {
    const r = rig();
    const result = deferred<string>();
    r.transcribe.mockReturnValueOnce(result.promise);
    const old = await r.start();
    const stale = r.contexts[0].node.onaudioprocess!;
    r.utterance();
    await flush();
    const second = r.microphone();
    r.getUserMedia.mockResolvedValueOnce(second.stream);
    const hints = ["web"];
    const next = await r.start("one-shot", events(), hints);
    hints[0] = "mutated";
    stale({
      inputBuffer: { getChannelData: () => audio(600) },
    } as unknown as AudioProcessingEvent);
    r.utterance();
    await flush();
    expect(next.final).toHaveBeenCalledExactlyOnceWith("run the tests");
    expect(r.transcribe.mock.calls[1][1]).toEqual(["web"]);
    result.resolve("stale text");
    await flush();
    expect(old.final).not.toHaveBeenCalled();
    expect(next.speechStart).toHaveBeenCalledOnce();
    expect(second.stop).toHaveBeenCalledOnce();
  });
  it("disposes out-of-order microphone grants without touching the replacement", async () => {
    const r = rig();
    const pending = deferred<MediaStream>();
    r.getUserMedia.mockReturnValueOnce(pending.promise);
    const old = events();
    const starting = r.capture.start("one-shot", old, []);
    const second = r.microphone();
    r.getUserMedia.mockResolvedValueOnce(second.stream);
    const next = await r.start();
    pending.resolve(r.first.stream);
    await starting;
    expect(r.first.stop).toHaveBeenCalledOnce();
    expect(second.stop).not.toHaveBeenCalled();
    expect(r.contexts).toHaveLength(1);
    r.utterance();
    await flush();
    expect(next.final).toHaveBeenCalledOnce();
    expect(old.final).not.toHaveBeenCalled();
  });
  it("cancellation during context close prevents inference from starting", async () => {
    const r = rig();
    const e = await r.start();
    const closing = deferred<void>();
    r.contexts[0].close.mockReturnValueOnce(closing.promise);
    r.utterance();
    const cancelling = r.capture.cancel();
    closing.resolve();
    await cancelling;
    await flush();
    expect(r.transcribe).not.toHaveBeenCalled();
    expect(e.final).not.toHaveBeenCalled();
  });
  it("cleans up initialization failures and rejects permission failures", async () => {
    const r = rig();
    r.getUserMedia.mockRejectedValueOnce(new Error("permission denied"));
    await expect(r.start()).rejects.toThrow("permission denied");
    vi.stubGlobal(
      "AudioContext",
      class {
        constructor() {
          throw new Error("audio failed");
        }
      },
    );
    await expect(r.start()).rejects.toThrow("audio failed");
    expect(r.first.stop).toHaveBeenCalledOnce();
  });
});

describe("existing browser modes", () => {
  it("push records until stop; duplicate stops produce one final", async () => {
    const r = rig();
    const e = await r.start("push");
    r.feed(audio(600));
    r.feed(audio(1000, 0.001));
    expect(r.transcribe).not.toHaveBeenCalled();
    expect(e.speechStart).toHaveBeenCalledOnce();
    await Promise.all([r.capture.stop(), r.capture.stop()]);
    expect(e.final).toHaveBeenCalledOnce();
    expect(r.first.stop).toHaveBeenCalledOnce();
  });
  it("hands-free holds its microphone open and recognizes utterances in order", async () => {
    const r = rig();
    const first = deferred<string>();
    r.transcribe.mockReturnValueOnce(first.promise);
    const e = await r.start("hands-free");
    r.utterance();
    r.utterance();
    await flush();
    expect(r.transcribe).toHaveBeenCalledOnce();
    expect(r.first.stop).not.toHaveBeenCalled();
    first.resolve("first");
    await flush();
    expect(e.final.mock.calls).toEqual([["first"], ["run the tests"]]);
    await r.capture.stop();
    expect(r.first.stop).toHaveBeenCalledOnce();
  });
  it("cancelling hands-free drops in-flight and queued recognitions", async () => {
    const r = rig();
    const first = deferred<string>();
    r.transcribe.mockReturnValueOnce(first.promise);
    const e = await r.start("hands-free");
    r.utterance();
    r.utterance();
    await flush();
    await r.capture.cancel();
    first.resolve("stale");
    await flush();
    expect(r.transcribe).toHaveBeenCalledOnce();
    expect(e.final).not.toHaveBeenCalled();
  });
  it.each(["hands-free", "one-shot"] as const)(
    "%s preserves deaf and echo guards set before start",
    async (mode) => {
      const r = rig();
      r.capture.setDeaf!(true);
      r.capture.setEchoGuard!(true);
      const e = await r.start(mode);
      r.utterance();
      expect(e.speechStart).not.toHaveBeenCalled();
      r.capture.setDeaf!(false);
      r.feed(audio(500, 0.02));
      expect(e.speechStart).not.toHaveBeenCalled();
      r.utterance();
      await flush();
      expect(e.final).toHaveBeenCalledOnce();
      await r.capture.cancel();
    },
  );
});
it("rejects native one-shot without subscribing or starting native recognition", async () => {
  const native = {
    status: vi.fn(async () => ({ available: true, local: true })),
    start: vi.fn(async () => {}),
    stop: vi.fn(async () => {}),
    cancel: vi.fn(async () => {}),
    onEvent: vi.fn(() => () => {}),
  };
  await expect(
    createNativeCapture(native).start("one-shot", events(), []),
  ).rejects.toThrow("not supported");
  expect(native.start).not.toHaveBeenCalled();
  expect(native.onEvent).not.toHaveBeenCalled();
});

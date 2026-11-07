import { describe, expect, it } from "vitest";
import {
  VoiceActivity,
  VoiceSession,
  downsample,
  encodeWav,
  isLikelyEcho,
  localCommand,
  voiceHint,
  type VoiceCapture,
  type VoiceCaptureEvents,
  type VoiceSessionState,
} from "../shared/voice";

const tone = (ms: number, amplitude: number) =>
  Float32Array.from({ length: (ms / 1000) * 16000 }, (_, i) =>
    i % 20 < 10 ? amplitude : -amplitude,
  );
const quiet = (ms: number) => tone(ms, 0.001);

describe("transcript readings", () => {
  it("takes only voice mode's own controls said alone", () => {
    expect(localCommand("Stop.")).toBe("quiet");
    expect(localCommand("Be quiet, please")).toBe("quiet");
    expect(localCommand("skip")).toBe("skip");
    expect(localCommand("Stop listening.")).toBe("end");
    expect(localCommand("never mind")).toBe("cancel");
    expect(localCommand("Stop the api worker")).toBeUndefined();
    expect(localCommand("pause the deploy")).toBeUndefined();
    expect(localCommand("")).toBeUndefined();
  });
  it("recognizes the device's own voice heard back", () => {
    const said = [
      "The api tests pass. I merged the branch and started the docs worker.",
    ];
    expect(isLikelyEcho("I merged the branch and started the docs", said)).toBe(
      true,
    );
    expect(isLikelyEcho("Tell the api worker to stop", said)).toBe(false);
    expect(isLikelyEcho("yes", said)).toBe(false);
  });
  it("hints only plain thread names, once each", () => {
    expect(
      voiceHint([
        { name: "api", aliases: ["backend", "api"] },
        { name: "rm -rf" },
        { name: "-x" },
      ]),
    ).toEqual(["api", "backend"]);
  });
});

describe("audio", () => {
  it("encodes 16 kHz mono 16-bit WAV", () => {
    const wav = encodeWav(Float32Array.from([0, 1, -1, 0.5]));
    const view = new DataView(wav.buffer);
    expect(new TextDecoder().decode(wav.slice(0, 4))).toBe("RIFF");
    expect(view.getUint32(24, true)).toBe(16000);
    expect(view.getUint16(22, true)).toBe(1);
    expect(view.getUint16(34, true)).toBe(16);
    expect(view.getUint32(40, true)).toBe(8);
    expect(view.getInt16(46, true)).toBe(0x7fff);
    expect(view.getInt16(48, true)).toBe(-0x8000);
  });
  it("downsamples 48 kHz to 16 kHz", () => {
    expect(downsample(new Float32Array(48000), 48000).length).toBe(16000);
  });
});

describe("voice activity", () => {
  it("finds one utterance with its pre-roll and ends it after silence", () => {
    const vad = new VoiceActivity();
    const happened = [
      ...vad.push(quiet(1000)),
      ...vad.push(tone(800, 0.2)),
      ...vad.push(quiet(1200)),
    ];
    expect(happened.filter((h) => h.start)).toHaveLength(1);
    const utterances = happened.filter((h) => h.utterance);
    expect(utterances).toHaveLength(1);
    const seconds = utterances[0].utterance!.length / 16000;
    expect(seconds).toBeGreaterThan(1.6);
    expect(seconds).toBeLessThan(2.2);
  });
  it("needs louder speech while the device's own voice plays", () => {
    const vad = new VoiceActivity();
    vad.push(quiet(1000));
    vad.guard = true;
    expect(vad.push(tone(500, 0.02)).some((h) => h.start)).toBe(false);
    expect(vad.push(tone(500, 0.3)).some((h) => h.start)).toBe(true);
  });
  it("ignores a click", () => {
    const vad = new VoiceActivity();
    vad.push(quiet(500));
    expect(vad.push(tone(40, 0.5)).some((h) => h.start)).toBe(false);
  });
});

function rig(submitResult: Record<string, string> = { action: "delegate" }) {
  let events: VoiceCaptureEvents | undefined;
  const log: string[] = [];
  const sent: string[] = [];
  const states: VoiceSessionState[] = [];
  const capture: VoiceCapture = {
    status: async () => ({ available: true, local: true }),
    async start(mode, e) {
      log.push("start:" + mode);
      events = e;
      if (mode === "push") e.speechStart();
    },
    async stop() {
      log.push("stop");
    },
    async cancel() {
      log.push("cancel");
    },
    setEchoGuard(playing) {
      log.push("guard:" + playing);
    },
  };
  const session = new VoiceSession({
    capture,
    speaker: {
      async control(op) {
        log.push("speaker:" + op);
      },
      async live(on) {
        log.push("live:" + on);
      },
    },
    async submit(text) {
      sent.push(text);
      return submitResult;
    },
    async repeat() {
      log.push("repeat");
    },
    hint: () => ["api"],
    spoken: () => ["The api tests pass and the docs are merged."],
    onChange: (s) => states.push(s),
  });
  const say = async (text: string) => {
    events!.final(text);
    await new Promise((r) => setTimeout(r, 0));
    await new Promise((r) => setTimeout(r, 0));
  };
  return { session, log, sent, states, say, events: () => events! };
}

describe("voice session", () => {
  it("pauses the device's voice at once when the user talks, and goes past it once sent", async () => {
    const r = rig();
    r.session.speakerState({ playing: true, paused: false, queued: 0 });
    await r.session.press();
    expect(r.log).toContain("speaker:pause");
    await r.say("Check on the api worker");
    expect(r.sent).toEqual(["Check on the api worker"]);
    expect(r.log.slice(-2)).toEqual(["speaker:skip", "speaker:resume"]);
    expect(r.session.state.heard).toEqual({
      text: "Check on the api worker",
      outcome: "sent",
    });
  });
  it("never pauses a speaker that was not playing, nor resumes one the user paused", async () => {
    const r = rig();
    r.session.speakerState({ playing: true, paused: true, queued: 0 });
    await r.session.press();
    await r.say("hello there");
    expect(r.log.filter((l) => l.startsWith("speaker:"))).toEqual([]);
  });
  it("keeps controls local and drops its own echo", async () => {
    const r = rig();
    await r.session.begin();
    expect(r.log).toEqual(["live:true", "start:hands-free"]);
    r.session.speakerState({ playing: true, paused: false, queued: 1 });
    expect(r.log).toContain("guard:true");
    r.events().speechStart();
    await r.say("the api tests pass and the docs");
    expect(r.sent).toEqual([]);
    expect(r.session.state.heard?.outcome).toBe("echo");
    expect(r.log.slice(-1)).toEqual(["speaker:resume"]);
    r.events().speechStart();
    await r.say("Skip.");
    expect(r.sent).toEqual([]);
    expect(r.log).toContain("speaker:skip");
    expect(r.session.state.phase).toBe("listening");
    await r.say("stop listening");
    expect(r.session.state.phase).toBe("off");
    expect(r.log).toContain("cancel");
  });
  it("holds a transcript the hub hands back for confirmation", async () => {
    const r = rig({
      action: "confirm",
      reason: "reads as an answer",
      heard: "allow it",
    });
    await r.session.press();
    await r.say("allow it");
    expect(r.session.state.confirm).toEqual({
      text: "allow it",
      reason: "reads as an answer",
    });
    r.session.dismissConfirm();
    expect(r.session.state.confirm).toBeUndefined();
  });
  it("sends nothing for silence and reports a recognizer error", async () => {
    const r = rig();
    await r.session.press();
    await r.say("");
    expect(r.sent).toEqual([]);
    expect(r.session.state.phase).toBe("ready");
    await r.session.press();
    r.events().error("no speech recognizer");
    expect(r.session.state.phase).toBe("error");
    expect(r.session.state.error).toBe("no speech recognizer");
  });
});

describe("desktop voice host", async () => {
  const { speakerName, recordingSchema, hintSchema, runWithInput } =
    await import("../desktop/voice");
  it("names this computer's speaker within a hub's limits", () => {
    expect(speakerName("nachos-mbp.local")).toBe("nachos-mbp desktop");
    expect(speakerName("é;rm -rf")).toBe("rm-rf desktop");
    expect(speakerName("")).toBe("this computer desktop");
    expect(speakerName("x".repeat(90)).length).toBeLessThanOrEqual(64);
  });
  it("takes only bounded recordings and plain hint words", () => {
    expect(recordingSchema.safeParse(new Uint8Array(10)).success).toBe(false);
    expect(recordingSchema.safeParse(new Uint8Array(1000)).success).toBe(true);
    expect(
      recordingSchema.safeParse(new Uint8Array(16000 * 2 * 121)).success,
    ).toBe(false);
    expect(hintSchema.safeParse(["api", "docs-2"]).success).toBe(true);
    expect(hintSchema.safeParse(["-rf"]).success).toBe(false);
    expect(hintSchema.safeParse(["a b"]).success).toBe(false);
  });
  it("passes the recording on stdin, never as an argument", async () => {
    const run = await runWithInput(
      "/bin/sh",
      ["-c", "wc -c"],
      new Uint8Array(1234),
      process.env,
      5000,
    );
    expect(run.code).toBe(0);
    expect(run.stdout.trim()).toBe("1234");
  });
});

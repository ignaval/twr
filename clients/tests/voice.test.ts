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

function rig(
  submitResult: Record<string, string> = { action: "delegate" },
  options: {
    submit?: (text: string) => Promise<Record<string, string>>;
    start?: () => Promise<void>;
  } = {},
) {
  let events: VoiceCaptureEvents | undefined;
  const log: string[] = [];
  const sent: string[] = [];
  const states: VoiceSessionState[] = [];
  const capture: VoiceCapture = {
    status: async () => ({ available: true, local: true }),
    async start(mode, e) {
      await options.start?.();
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
      return options.submit ? options.submit(text) : submitResult;
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
    await new Promise((x) => setTimeout(x, 0));
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
  it("downloads the speech model once with `twr voice setup`, then reports it ready", async () => {
    const { DesktopVoice } = await import("../desktop/voice");
    const { mkdtempSync, writeFileSync, chmodSync, readFileSync } =
      await import("node:fs");
    const { join } = await import("node:path");
    const { tmpdir } = await import("node:os");
    const dir = mkdtempSync(join(tmpdir(), "twr-voice-"));
    const log = join(dir, "log");
    const model = join(dir, "model");
    const twr = join(dir, "twr");
    // A stand-in `twr voice`: status says only the model is missing until setup made it.
    writeFileSync(
      twr,
      `#!/bin/sh
echo "$*" >> "${log}"
if [ "$2" = setup ]; then sleep 0.2; touch "${model}"; exit 0; fi
if [ -f "${model}" ]; then echo '{"available":true,"engine":"whisper","local":true}';
else echo '{"available":false,"local":false,"problem":"no speech model yet","setup":true}'; fi
`,
    );
    chmodSync(twr, 0o755);
    const voice = new DesktopVoice(
      () => twr,
      () => process.env,
      () => {},
    );
    expect(await voice.status()).toMatchObject({
      available: false,
      setup: true,
    });
    const [a, b] = await Promise.all([voice.setup(), voice.setup()]);
    expect(a).toMatchObject({ available: true, setup: false });
    expect(b).toMatchObject({ available: true });
    const calls = readFileSync(log, "utf8").trim().split("\n");
    expect(calls.filter((c) => c === "voice setup")).toHaveLength(1);
  });
  it("says why the download failed", async () => {
    const { DesktopVoice } = await import("../desktop/voice");
    const voice = new DesktopVoice(
      () => "/bin/sh",
      () => process.env,
      () => {},
    );
    // `sh voice setup`: no such script, a nonzero exit with a line on stderr.
    await expect(voice.setup()).rejects.toThrow(/voice/);
  });
});

const tick = () => new Promise((r) => setTimeout(r, 0));

describe("voice session races (review regressions)", () => {
  it("resumes the speaker it paused when recognition fails", async () => {
    const r = rig();
    r.session.speakerState({ playing: true, paused: false, queued: 0 });
    await r.session.press();
    expect(r.log).toContain("speaker:pause");
    r.events().error("whisper timed out");
    await tick();
    expect(r.log.slice(-1)).toEqual(["speaker:resume"]);
    expect(r.session.state.phase).toBe("error");
  });
  it("refuses a new recording while one is being sent, and never loses the release", async () => {
    let finish!: (v: Record<string, string>) => void;
    const r = rig(undefined, {
      submit: () => new Promise((resolve) => (finish = resolve)),
    });
    await r.session.press();
    r.events().final("check the docs");
    await tick();
    expect(r.session.state.phase).toBe("sending");
    expect(r.session.idle).toBe(false);
    await r.session.press();
    expect(r.log.filter((l) => l === "start:push")).toHaveLength(1);
    finish({ action: "delegate" });
    await tick();
    await tick();
    expect(r.session.state.phase).toBe("ready");
    expect(r.session.idle).toBe(true);
  });
  it("holds the speaker until the last of overlapping utterances is handled", async () => {
    let finish!: (v: Record<string, string>) => void;
    const r = rig(undefined, {
      submit: () => new Promise((resolve) => (finish = resolve)),
    });
    await r.session.begin();
    r.session.speakerState({ playing: true, paused: false, queued: 0 });
    r.events().speechStart();
    r.events().final("first message");
    await tick();
    r.events().speechStart();
    finish({ action: "delegate" });
    await tick();
    await tick();
    expect(r.log).not.toContain("speaker:resume");
    expect(r.session.state.phase).toBe("hearing");
    r.events().final("");
    await tick();
    await tick();
    expect(r.log.slice(-2)).toEqual(["speaker:skip", "speaker:resume"]);
    expect(r.session.state.phase).toBe("listening");
  });
  it("stops a hold-to-talk released while the microphone was opening", async () => {
    let open!: () => void;
    const r = rig(undefined, {
      start: () => new Promise<void>((resolve) => (open = resolve)),
    });
    const pressing = r.session.press();
    await tick();
    await r.session.release();
    expect(r.log).not.toContain("stop");
    open();
    await pressing;
    expect(r.log).toContain("stop");
  });
  it("resumes what was read when the hub hands a transcript back", async () => {
    const r = rig({ action: "confirm", reason: "reads as an answer" });
    r.session.speakerState({ playing: true, paused: false, queued: 0 });
    await r.session.press();
    await r.say("allow it");
    expect(r.log.slice(-1)).toEqual(["speaker:resume"]);
    expect(r.log).not.toContain("speaker:skip");
    expect(r.session.state.heard?.outcome).toBe("confirm");
  });
  it("pauses a speaker it can't see, and checks for its echo", async () => {
    const r = rig();
    await r.session.begin();
    r.session.speakerState({
      playing: false,
      paused: false,
      queued: 0,
      unknown: true,
    });
    r.events().speechStart();
    expect(r.log).toContain("speaker:pause");
    await r.say("api needs your permission");
    expect(r.sent).toEqual(["api needs your permission"]);
  });
  it("drops an announcement heard back", async () => {
    const r = rig(undefined, {});
    const { announcements } = await import("../shared/voice");
    expect(
      isLikelyEcho(
        "api needs your permission",
        announcements([{ name: "api" }]),
      ),
    ).toBe(true);
  });
  it("still listens when the device's speaker can't start", async () => {
    const r = rig();
    const failing = new VoiceSession({
      capture: {
        status: async () => ({ available: true, local: true }),
        start: async () => {},
        stop: async () => {},
        cancel: async () => {},
      },
      speaker: {
        control: async () => {},
        live: async () => {
          throw new Error("Unsupported mobile operation");
        },
      },
      submit: async () => ({}),
      hint: () => [],
      spoken: () => [],
      onChange: () => {},
    });
    await failing.begin();
    expect(failing.state.phase).toBe("listening");
    expect(failing.state.error).toMatch(/not read aloud/);
    void r;
  });
});

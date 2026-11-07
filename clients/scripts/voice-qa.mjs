// Voice mode, end to end (docs/voice.md): real Electron, a real hub with twr's deterministic
// fake runtime, real whisper.cpp, and Chromium's fake microphone playing a recording of a
// spoken sentence. Needs whisper.cpp's CLI on PATH and a model (TWR_VOICE_QA_MODEL); without
// them it says so and exits 0, as the packaged QA does without a display.
//
//   TWR_VOICE_QA_MODEL=/path/ggml-base.en.bin node scripts/voice-qa.mjs
//
// Checks: push to talk sends the transcript to the orchestrator marked spoken; the reply is
// read aloud by the app's own speaker (a command engine that logs what it is given); a
// transcript that reads as a mode change is held for confirmation, nothing sent.
import { _electron as electron } from "@playwright/test";
import { spawn, execFileSync } from "node:child_process";
import {
  mkdtempSync,
  realpathSync,
  writeFileSync,
  readFileSync,
  existsSync,
  rmSync,
  chmodSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import assert from "node:assert/strict";
import { randomUUID } from "node:crypto";

const model = process.env.TWR_VOICE_QA_MODEL;
const which = (name) => {
  try {
    return execFileSync("/usr/bin/which", [name], { encoding: "utf8" }).trim();
  } catch {
    return "";
  }
};
if (!model || !existsSync(model) || !which("whisper-cli")) {
  console.log(
    "SKIP: voice QA needs whisper-cli on PATH and TWR_VOICE_QA_MODEL naming a ggml model",
  );
  process.exit(0);
}
if (process.platform !== "darwin") {
  console.log("SKIP: voice QA synthesizes its recordings with macOS say");
  process.exit(0);
}

const home = realpathSync(mkdtempSync(join(tmpdir(), "twr-voice-qa-")));
const bin = resolve("../target/debug");
const spoken = join(home, "spoken.log");
const engine = join(home, "say-to-log");
writeFileSync(engine, `#!/bin/sh\ncat >> "${spoken}"\n`);
chmodSync(engine, 0o755);
writeFileSync(
  join(home, "config.toml"),
  `default_runtime = "fake"
[orchestrator]
autostart = false
[threads]
idle_compaction = "off"
[tui]
suggest = false
summaries = false
[speech]
engine = "command"
command = [${JSON.stringify(engine)}]
local = "off"
[voice]
model = ${JSON.stringify(model)}
`,
  { mode: 0o600 },
);

/** A 16 kHz mono recording of `text`, with a second of silence after it. */
function recording(name, text) {
  const raw = join(home, name + ".raw.wav");
  execFileSync("say", [
    "-o",
    raw,
    "--data-format=LEI16@16000",
    "--file-format=WAVE",
    text,
  ]);
  const bytes = readFileSync(raw);
  const data = bytes.indexOf(Buffer.from("data"));
  const samples = bytes.subarray(data + 8);
  const silence = Buffer.alloc(16000 * 2 * 2);
  const body = Buffer.concat([samples, silence]);
  const header = Buffer.alloc(44);
  header.write("RIFF", 0);
  header.writeUInt32LE(36 + body.length, 4);
  header.write("WAVEfmt ", 8);
  header.writeUInt32LE(16, 16);
  header.writeUInt16LE(1, 20);
  header.writeUInt16LE(1, 22);
  header.writeUInt32LE(16000, 24);
  header.writeUInt32LE(32000, 28);
  header.writeUInt16LE(2, 32);
  header.writeUInt16LE(16, 34);
  header.write("data", 36);
  header.writeUInt32LE(body.length, 40);
  const out = join(home, name + ".wav");
  writeFileSync(out, Buffer.concat([header, body]));
  return out;
}

// The orchestrator: a stand-in for Claude Code's stream-json CLI that answers each message
// with "Heard you: <its last line>". No account, no network.
const claude = join(home, "claude");
const claudePy = join(home, "claude.py");
writeFileSync(
  claudePy,
  `import json, sys, uuid
sid = str(uuid.uuid4())
for line in sys.stdin:
    try:
        m = json.loads(line)
    except ValueError:
        continue
    if m.get("type") != "user":
        continue
    c = m["message"]["content"]
    text = c if isinstance(c, str) else " ".join(x.get("text", "") for x in c if isinstance(x, dict))
    lines = [l for l in text.splitlines() if l.strip()]
    reply = "Heard you: " + (lines[-1] if lines else "")
    out = lambda v: print(json.dumps(v), flush=True)
    out({"type": "system", "subtype": "init", "session_id": sid, "cwd": ".", "tools": [], "mcp_servers": [], "model": "stand-in", "permissionMode": "dontAsk"})
    out({"type": "assistant", "session_id": sid, "message": {"id": "m" + uuid.uuid4().hex, "type": "message", "role": "assistant", "model": "stand-in", "content": [{"type": "text", "text": reply}], "stop_reason": "end_turn", "usage": {"input_tokens": 1, "output_tokens": 1}}})
    out({"type": "result", "subtype": "success", "is_error": False, "duration_ms": 1, "num_turns": 1, "result": reply, "session_id": sid, "usage": {"input_tokens": 1, "output_tokens": 1}, "total_cost_usd": 0})
`,
);
writeFileSync(
  claude,
  `#!/bin/sh
case "$1" in --help|--version) exit 0;; esac
exec python3 "${claudePy}" "$@"
`,
);
chmodSync(claude, 0o755);

const tokenFile = join(home, "test-user.token");
writeFileSync(tokenFile, randomUUID(), { mode: 0o600 });
const env = {
  ...process.env,
  TWR_HOME: home,
  TWR_TEST_USER_TOKEN_FILE: tokenFile,
  TWR_ENABLE_FAKE: "1",
  TWR_DESKTOP_TEST_AUTOMATION: "1",
  TWR_DESKTOP_BINARY_DIR: bin,
  TWR_DESKTOP_TEST_USER_DATA: join(home, "electron"),
  TWR_WORKSPACE: "default",
  TWR_CLAUDE_BIN: claude,
  HOME: home,
  SHELL: "/bin/sh",
};
delete env.ELECTRON_RUN_AS_NODE;

const wait = (ms) => new Promise((r) => setTimeout(r, ms));
async function until(check, ms, message) {
  const deadline = Date.now() + ms;
  for (;;) {
    const value = await check();
    if (value) return value;
    if (Date.now() > deadline) throw new Error(message);
    await wait(250);
  }
}

// The microphone plays `wav` from the moment it opens, then silence. Chromium's own
// file-backed fake capture stays silent under Electron, so the recording is put in at
// getUserMedia: everything after the OS microphone (the app's capture, voice activity, WAV
// encoding, IPC, `twr voice transcribe`, `say`) is the real thing.
function microphone(base64) {
  const real = navigator.mediaDevices.getUserMedia.bind(navigator.mediaDevices);
  navigator.mediaDevices.getUserMedia = async (constraints) => {
    if (!constraints || !constraints.audio) return real(constraints);
    const bytes = Uint8Array.from(atob(base64), (c) => c.charCodeAt(0));
    const context = new AudioContext();
    const buffer = await context.decodeAudioData(bytes.buffer);
    const source = context.createBufferSource();
    source.buffer = buffer;
    const out = context.createMediaStreamDestination();
    source.connect(out);
    source.start();
    return out.stream;
  };
}

async function launch(wav) {
  const app = await electron.launch({
    args: [".", "--use-fake-ui-for-media-stream"],
    cwd: resolve("."),
    env,
    timeout: 60000,
  });
  const page = await app.firstWindow();
  page.on("console", (m) => logs.push(`console.${m.type()}: ${m.text()}`));
  page.on("pageerror", (e) => logs.push(`pageerror: ${e.message}`));
  current = page;
  await page
    .locator(".connection-status")
    .filter({ hasText: "Hub connected" })
    .waitFor({ timeout: 30000 });
  // The app opens the microphone only when voice is turned on, after this.
  await page.evaluate(microphone, readFileSync(wav).toString("base64"));
  return { app, page };
}

const twr = (...args) =>
  execFileSync(join(bin, "twr"), args, { env, encoding: "utf8" });

let hub;
let app;
let current;
const logs = [];
try {
  hub = spawn(join(bin, "twr"), ["hub"], {
    env,
    stdio: ["ignore", "ignore", "pipe"],
  });
  await until(
    () => {
      try {
        twr("info", "--json");
        return true;
      } catch {
        return false;
      }
    },
    10000,
    "the hub did not start",
  );
  const status = JSON.parse(twr("voice", "status", "--json"));
  assert.equal(status.available, true, JSON.stringify(status));
  assert.equal(status.local, true);
  console.log("PASS: twr voice status finds whisper.cpp and the model");
  // Voice talks to the orchestrator, as the message box does: one is chosen first.
  twr("orchestrator", "use", "claude-code");

  // 1. Push to talk: click, the fake microphone plays the sentence, click to send.
  const sentence = "Check on the docs worker and tell me when the tests pass.";
  ({ app } = await launch(recording("message", sentence)));
  let page = await app.firstWindow();
  const talk = page.getByRole("button", { name: "Talk to the orchestrator" });
  await talk.waitFor();
  await talk.click();
  await page
    .getByRole("button", { name: "Send what you said" })
    .waitFor({ timeout: 10000 });
  // The recording plays from its start once capture opens; leave it time to be heard.
  await wait(5000);
  await page.getByRole("button", { name: "Send what you said" }).click();
  await page
    .locator(".voice-heard")
    .filter({ hasText: "Sent" })
    .waitFor({ timeout: 60000 });
  const heard = await page.locator(".voice-heard").innerText();
  assert.match(heard, /docs worker/i, heard);
  assert.match(heard, /tests pass/i, heard);
  const orchestrator = await until(
    () => {
      const shown = JSON.parse(twr("orchestrator", "show", "--json"));
      const id = shown.thread?.id || shown.thread;
      return typeof id === "string" ? { id } : undefined;
    },
    20000,
    "no orchestrator was made for the spoken message",
  );
  const events = await until(
    () => {
      const out = twr("events", orchestrator.id);
      return /docs worker/i.test(out) ? out : "";
    },
    30000,
    "the orchestrator never received the spoken message",
  );
  assert.match(events, /"from":"user"/);
  console.log("PASS: push to talk sent the transcript to the orchestrator");

  // 2. Hands-free: the app's own speaker starts and reads the orchestrator's replies.
  await page.getByRole("button", { name: "Hands-free voice" }).click();
  const speakers = await until(
    async () => {
      const s = await page.evaluate(() => window.twr.rpc("speech.status"));
      const mine = (s.speakers || []).find((x) => /desktop$/.test(x.name));
      return mine && mine.live ? s : undefined;
    },
    20000,
    "the app's speaker never attached with live reading on",
  );
  assert(speakers);
  console.log("PASS: hands-free started the app's speaker, live reading on");
  await until(
    () =>
      existsSync(spoken) && /docs worker/i.test(readFileSync(spoken, "utf8")),
    60000,
    "the orchestrator's reply was never read aloud",
  );
  console.log(
    "PASS: the reply to a spoken message was read aloud on this computer",
  );
  await page.getByRole("button", { name: "Stop hands-free voice" }).click();
  await app.close();
  app = undefined;

  // 3. A transcript that reads as a mode change is held for confirmation; nothing is sent.
  const before = twr("events", orchestrator.id).split("\n").length;
  ({ app } = await launch(recording("command", "Set all to bypass.")));
  page = await app.firstWindow();
  await page.getByRole("button", { name: "Talk to the orchestrator" }).click();
  await page
    .getByRole("button", { name: "Send what you said" })
    .waitFor({ timeout: 10000 });
  await wait(3500);
  await page.getByRole("button", { name: "Send what you said" }).click();
  await page
    .getByRole("alertdialog", { name: "Confirm spoken message" })
    .waitFor({ timeout: 60000 });
  const after = twr("events", orchestrator.id).split("\n").length;
  assert.equal(after, before, "a held transcript records nothing");
  await page.getByRole("button", { name: "Review in message box" }).click();
  const draft = await page
    .getByRole("textbox", { name: "Message orchestrator" })
    .inputValue();
  assert.match(draft, /bypass/i, draft);
  console.log(
    "PASS: a spoken mode change was held for confirmation and nothing was sent",
  );
  await app.close();
  app = undefined;
  console.log("Voice QA passed");
} catch (error) {
  if (current) {
    const text = await current
      .evaluate(() =>
        [".voice-bar", ".voice-confirm", ".error-banner", "[role=alert]"]
          .map((q) =>
            [...document.querySelectorAll(q)]
              .map((e) => q + ": " + e.textContent)
              .join("\n"),
          )
          .join("\n"),
      )
      .catch((e) => String(e));
    console.log("Page state:\n" + text);
    await current
      .screenshot({ path: join(home, "failure.png") })
      .catch(() => {});
  }
  console.log(logs.slice(-30).join("\n"));
  throw error;
} finally {
  await app?.close().catch(() => {});
  hub?.kill("SIGTERM");
  await wait(500);
  if (!process.env.TWR_VOICE_QA_KEEP)
    rmSync(home, { recursive: true, force: true });
  else console.log("Kept", home);
}

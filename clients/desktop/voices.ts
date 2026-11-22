// Settings › Read-aloud voice on the desktop (docs/read-aloud.md#choosing-a-voice-in-the-apps):
// this computer's voices and twr's natural ones, through the bundled `twr voices`. The choice
// is `[speech] voice` and `voices` in this computer's twr config, which every speaker here
// (the app's voice mode, the TUI's own, `twr speak`) reads with from its next sentence on.
import { spawn, type ChildProcess } from "node:child_process";
import { z } from "zod";
import type { DesktopEvent } from "../shared/types";
import {
  previewText,
  voiceLanguages,
  type VoiceLanguage,
  type VoiceListing,
} from "../shared/readAloudVoices";
import { runWithInput } from "./voice";

/** `twr voices list --json`. */
const listingSchema = z.object({
  voices: z.array(
    z.object({
      id: z.string(),
      name: z.string(),
      engine: z.enum(["system", "twr"]),
      model: z.string().optional(),
      languages: z.array(z.string()),
      locale: z.string(),
      quality: z.enum(["premium", "enhanced", "default", "neural"]),
      ready: z.boolean(),
    }),
  ),
  models: z.array(
    z.object({
      name: z.string(),
      title: z.string(),
      installed: z.boolean(),
      bytes: z.number(),
      size: z.string(),
      license: z.string(),
      rtf: z.number().optional(),
      fast: z.boolean().optional(),
      needs: z.string().optional(),
    }),
  ),
  voice: z.string(),
  by_language: z.record(z.string(), z.string()),
});

/** A line of `twr voices install --json`. */
const installLine = z.union([
  z.object({
    type: z.literal("progress"),
    bytes: z.number(),
    total: z.number(),
  }),
  z.object({
    type: z.literal("phase"),
    phase: z.enum(["downloading", "installing"]),
  }),
  z.object({ type: z.literal("error"), message: z.string() }),
  z.object({ type: z.string() }),
]);

export const voiceModelSchema = z.string().regex(/^[a-z0-9][a-z0-9-]{0,31}$/);
export const voiceLanguageSchema = z.enum(voiceLanguages);
/** A voice name as a speaker takes it: 1-128 printable characters. */
export const voiceNameSchema = z
  .string()
  .min(1)
  .max(128)
  .regex(/^[^\p{Cc}\p{Cf}]+$/u);

/** A TOML basic string (JSON's escapes are TOML's for what a voice name may hold). */
const tomlString = (s: string) => JSON.stringify(s);

/**
 * `text` (a twr config.toml) with `[speech] voice` and `voices` set from `chosen`: the voice
 * of other languages is the English one, else the Spanish one. Every other line, other
 * `[speech]` keys included, stays; with nothing chosen both keys go. A `[speech.voices]`
 * table of its own can't be rewritten here: it is refused, to be edited by hand.
 */
export function withSpeechVoices(
  text: string,
  chosen: Partial<Record<VoiceLanguage, string>>,
) {
  const lines = text.split("\n");
  if (lines.some((l) => /^\s*\[\s*speech\s*\.\s*voices\s*\]/.test(l)))
    throw new Error(
      "Your config has a [speech.voices] table: choose voices by editing it in Settings › Configuration.",
    );
  const header = /^\s*\[\s*speech\s*\]\s*(#.*)?$/;
  const table = /^\s*\[/;
  const ours = /^\s*(voice|voices)\s*=/;
  const fallback = chosen.en || chosen.es;
  let start = lines.findIndex((l) => header.test(l));
  if (start < 0 && !fallback) return text;
  if (start < 0) {
    while (lines.length && lines[lines.length - 1] === "") lines.pop();
    if (lines.length) lines.push("");
    lines.push("[speech]");
    start = lines.length - 1;
    lines.push("");
  }
  let end = lines.findIndex((l, i) => i > start && table.test(l));
  if (end < 0) end = lines.length;
  const section = lines.slice(start + 1, end);
  // A value of ours over several lines (a multi-line inline table) can't be cut out safely.
  for (const [i, l] of section.entries())
    if (ours.test(l) && /[{[]/.test(l) && !/[}\]]\s*(#.*)?$/.test(l))
      throw new Error(
        `[speech] ${l.split("=")[0].trim()} spans several lines: edit it in Settings › Configuration (line ${start + 2 + i}).`,
      );
  const kept = section.filter((l) => !ours.test(l));
  const entries = voiceLanguages
    .filter((l) => chosen[l])
    .map((l) => `${l} = ${tomlString(chosen[l]!)}`);
  const added = fallback
    ? [`voice = ${tomlString(fallback)}`, `voices = { ${entries.join(", ")} }`]
    : [];
  return [
    ...lines.slice(0, start + 1),
    ...added,
    ...kept,
    ...lines.slice(end),
  ].join("\n");
}

/** What `twr voices list --json` says, as Settings shows it. */
export function desktopListing(
  raw: z.infer<typeof listingSchema>,
  platform: NodeJS.Platform,
): VoiceListing {
  const byLanguage: VoiceListing["byLanguage"] = {};
  for (const lang of voiceLanguages)
    if (raw.by_language[lang]) byLanguage[lang] = raw.by_language[lang];
  return {
    voices: raw.voices,
    models: raw.models,
    byLanguage,
    platform: platform === "darwin" ? "macos" : platform,
    naturalWhere: "here",
    canInstall: true,
  };
}

/** The companion's config calls (`desktop.config.read`, `desktop.config.save`). */
export interface ConfigStore {
  read(): Promise<{ content: string; hash: string }>;
  save(content: string, hash: string): Promise<void>;
}

export class DesktopVoices {
  private installs = new Map<string, Promise<VoiceListing>>();
  private children = new Set<ChildProcess>();
  private previewing?: ChildProcess;

  constructor(
    private twr: () => string,
    private env: () => NodeJS.ProcessEnv,
    private config: () => ConfigStore,
    private emit: (event: DesktopEvent) => void,
    private platform: NodeJS.Platform = process.platform,
  ) {}

  async list(): Promise<VoiceListing> {
    const run = await runWithInput(
      this.twr(),
      ["voices", "list", "--json"],
      undefined,
      this.env(),
      30_000,
    );
    if (run.code !== 0)
      throw new Error(
        run.stderr
          .trim()
          .split("\n")
          .pop()
          ?.replace(/^twr: /, "") || "This twr has no `twr voices`.",
      );
    return desktopListing(
      listingSchema.parse(JSON.parse(run.stdout)),
      this.platform,
    );
  }

  /** Downloads `model`, its progress as `voice-install` events; the new listing. */
  install(model: unknown): Promise<VoiceListing> {
    const name = voiceModelSchema.parse(model);
    const running = this.installs.get(name);
    if (running) return running;
    const job = new Promise<void>((resolve, reject) => {
      const child = spawn(this.twr(), ["voices", "install", name, "--json"], {
        env: this.env(),
        stdio: ["ignore", "pipe", "pipe"],
      });
      this.children.add(child);
      let buffer = "";
      let failure: string | undefined;
      let stderr = "";
      child.stdout!.setEncoding("utf8");
      child.stdout!.on("data", (chunk: string) => {
        buffer += chunk;
        let nl: number;
        while ((nl = buffer.indexOf("\n")) >= 0) {
          const line = buffer.slice(0, nl).trim();
          buffer = buffer.slice(nl + 1);
          let parsed;
          try {
            parsed = installLine.safeParse(JSON.parse(line));
          } catch {
            continue;
          }
          if (!parsed.success) continue;
          const m = parsed.data as any;
          if (m.type === "progress")
            this.emit({
              type: "voice-install",
              install: {
                model: name,
                phase: "downloading",
                bytes: m.bytes,
                total: m.total,
              },
            });
          else if (m.type === "phase")
            this.emit({
              type: "voice-install",
              install: { model: name, phase: m.phase },
            });
          else if (m.type === "error") failure = m.message;
        }
      });
      child.stderr!.setEncoding("utf8");
      child.stderr!.on("data", (c: string) => {
        stderr = (stderr + c).slice(-4000);
      });
      child.on("error", (e) => {
        this.children.delete(child);
        reject(e);
      });
      child.on("close", (code) => {
        this.children.delete(child);
        if (code === 0) resolve();
        else
          reject(
            new Error(
              failure ||
                stderr.trim().split("\n").pop() ||
                `The voice download failed (exit ${code})`,
            ),
          );
      });
    });
    const done = job
      .then(() => {
        this.emit({
          type: "voice-install",
          install: { model: name, phase: "done" },
        });
        return this.list();
      })
      .catch((e) => {
        this.emit({
          type: "voice-install",
          install: {
            model: name,
            phase: "error",
            error: String(e?.message ?? e),
          },
        });
        throw e;
      })
      .finally(() => this.installs.delete(name));
    this.installs.set(name, done);
    return done;
  }

  /** Sets the voice for `lang` (null: the default one) in this computer's config. */
  async select(lang: unknown, voice: unknown): Promise<VoiceListing> {
    const language = voiceLanguageSchema.parse(lang);
    const name = voice === null ? null : voiceNameSchema.parse(voice);
    const listing = await this.list();
    if (name && !listing.voices.some((v) => v.id === name))
      throw new Error("That voice is not available on this computer.");
    const chosen = { ...listing.byLanguage };
    if (name) chosen[language] = name;
    else delete chosen[language];
    const store = this.config();
    const { content, hash } = await store.read();
    const next = withSpeechVoices(content, chosen);
    if (next !== content) await store.save(next, hash);
    return this.list();
  }

  /** Says a sample sentence with `voice`; resolves once it was said or stopped. */
  preview(voice: unknown, lang: unknown): Promise<void> {
    const name = voiceNameSchema.parse(voice);
    const language = voiceLanguageSchema.parse(lang);
    this.stopPreview();
    return new Promise((resolve, reject) => {
      const child = spawn(
        this.twr(),
        ["voices", "try", name, previewText[language]],
        { env: this.env(), stdio: ["ignore", "ignore", "pipe"] },
      );
      this.previewing = child;
      this.children.add(child);
      this.emit({ type: "voice-preview", preview: { voice: name } });
      let stderr = "";
      child.stderr!.setEncoding("utf8");
      child.stderr!.on("data", (c: string) => {
        stderr = (stderr + c).slice(-2000);
      });
      const finish = (error?: Error) => {
        this.children.delete(child);
        if (this.previewing === child) {
          this.previewing = undefined;
          this.emit({ type: "voice-preview", preview: {} });
        }
        if (error) reject(error);
        else resolve();
      };
      child.on("error", (e) => finish(e));
      child.on("close", (code, signal) => {
        if (code === 0 || signal) finish();
        else
          finish(
            new Error(
              stderr
                .trim()
                .split("\n")
                .pop()
                ?.replace(/^twr: /, "") || "The voice could not play.",
            ),
          );
      });
    });
  }

  stopPreview() {
    this.previewing?.kill("SIGTERM");
  }

  dispose() {
    for (const c of this.children) c.kill("SIGTERM");
    this.children.clear();
  }
}

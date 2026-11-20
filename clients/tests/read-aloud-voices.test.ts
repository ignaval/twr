import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it, vi } from "vitest";
import {
  DesktopVoices,
  desktopListing,
  voiceModelSchema,
  voiceNameSchema,
  withSpeechVoices,
} from "../desktop/voices";
import { VoicePicker } from "../shared/ReadAloudVoiceOptions";
import {
  suggestion,
  voiceNote,
  type VoiceListing,
} from "../shared/readAloudVoices";

const config =
  '[orchestrator]\nautostart = false\n\n[voice]\nengine = "whisper"\nlanguage = "auto"\n';

describe("the [speech] voices the desktop writes", () => {
  it("adds a [speech] table with the fallback voice and one per language", () => {
    expect(
      withSpeechVoices(config, { en: "Ava (Premium)", es: "kokoro/ef_dora" }),
    ).toBe(
      config +
        '\n[speech]\nvoice = "Ava (Premium)"\nvoices = { en = "Ava (Premium)", es = "kokoro/ef_dora" }\n',
    );
    // Only Spanish chosen: it is the fallback too.
    expect(withSpeechVoices(config, { es: "kokoro/ef_dora" })).toContain(
      'voice = "kokoro/ef_dora"\nvoices = { es = "kokoro/ef_dora" }',
    );
  });

  it("replaces its own keys and keeps every other line", () => {
    const mine =
      '[speech]\n# my rate\nrate = 200\nvoice = "Samantha"\nvoices = { en = "Samantha" }\n\n[voice]\nengine = "whisper"\n';
    expect(withSpeechVoices(mine, { en: "Ava (Premium)" })).toBe(
      '[speech]\nvoice = "Ava (Premium)"\nvoices = { en = "Ava (Premium)" }\n# my rate\nrate = 200\n\n[voice]\nengine = "whisper"\n',
    );
    expect(withSpeechVoices(mine, {})).toBe(
      '[speech]\n# my rate\nrate = 200\n\n[voice]\nengine = "whisper"\n',
    );
    expect(withSpeechVoices(config, {})).toBe(config);
  });

  it("escapes names as TOML strings", () => {
    expect(withSpeechVoices(config, { en: 'Odd "Name" \\ x' })).toContain(
      'voice = "Odd \\"Name\\" \\\\ x"',
    );
  });

  it("refuses what it can't rewrite line by line", () => {
    expect(() =>
      withSpeechVoices('[speech]\nrate = 1\n\n[speech.voices]\nen = "Ava"\n', {
        en: "Samantha",
      }),
    ).toThrow("[speech.voices]");
    expect(() =>
      withSpeechVoices('[speech]\nvoices = {\n  en = "Ava" }\n', {
        en: "Samantha",
      }),
    ).toThrow("several lines");
  });
});

describe("what the desktop's voice calls take", () => {
  it("takes catalog-like model names and printable voice names only", () => {
    for (const ok of ["kokoro", "piper-en"])
      expect(voiceModelSchema.safeParse(ok).success).toBe(true);
    for (const bad of ["../kokoro", "https://example.com/x", "", "Kokoro"])
      expect(voiceModelSchema.safeParse(bad).success, bad).toBe(false);
    for (const ok of ["Ava (Premium)", "kokoro/af_heart", "Mónica"])
      expect(voiceNameSchema.safeParse(ok).success).toBe(true);
    for (const bad of ["", "x".repeat(129), "Ava\nrm -rf", "a​b"])
      expect(voiceNameSchema.safeParse(bad).success, bad).toBe(false);
  });

  it("refuses a voice this computer doesn't have, and saves over the read text", async () => {
    const saved: [string, string][] = [];
    const voices = new DesktopVoices(
      () => "twr",
      () => ({}),
      () => ({
        read: async () => ({ content: config, hash: "h" }),
        save: async (content, hash) => {
          saved.push([content, hash]);
        },
      }),
      () => {},
      "darwin",
    );
    vi.spyOn(voices, "list").mockResolvedValue(listing());
    await expect(voices.select("en", "Nobody")).rejects.toThrow(
      "not available",
    );
    await expect(voices.select("fr", "Ava (Premium)")).rejects.toThrow();
    await voices.select("es", "kokoro/ef_dora");
    expect(saved).toHaveLength(1);
    expect(saved[0][1]).toBe("h");
    expect(saved[0][0]).toContain(
      'voices = { en = "Ava (Premium)", es = "kokoro/ef_dora" }',
    );
  });
});

function listing(patch: Partial<VoiceListing> = {}): VoiceListing {
  return {
    ...desktopListing(
      {
        voices: [
          {
            id: "Ava (Premium)",
            name: "Ava (Premium)",
            engine: "system",
            languages: ["en"],
            locale: "en-US",
            quality: "premium",
            ready: true,
          },
          {
            id: "Mónica",
            name: "Mónica",
            engine: "system",
            languages: ["es"],
            locale: "es-ES",
            quality: "default",
            ready: true,
          },
          {
            id: "kokoro/ef_dora",
            name: "Kokoro Dora",
            engine: "twr",
            model: "kokoro",
            languages: ["es"],
            locale: "es-ES",
            quality: "neural",
            ready: false,
          },
        ],
        models: [
          {
            name: "kokoro",
            title: "Kokoro",
            installed: false,
            bytes: 349886476,
            size: "350 MB",
            license: "Apache-2.0",
          },
        ],
        voice: "Ava (Premium)",
        by_language: { en: "Ava (Premium)", fr: "Thomas" },
      },
      "darwin",
    ),
    ...patch,
  };
}

describe("Settings › Read-aloud voice", () => {
  it("lists only the languages it picks for, as this Mac's", () => {
    const l = listing();
    expect(l.byLanguage).toEqual({ en: "Ava (Premium)" });
    expect(l.platform).toBe("macos");
    expect(l.naturalWhere).toBe("here");
  });

  it("suggests the best voice ready, else Kokoro to download", () => {
    const l = listing();
    expect(suggestion(l, "en")?.voice.id).toBe("Ava (Premium)");
    expect(suggestion(l, "es")?.download?.name).toBe("kokoro");
    // Without the right to download, no download is suggested.
    expect(suggestion({ ...l, canInstall: false }, "es")).toBeUndefined();
  });

  it("says where a natural voice is made and what it needs", () => {
    const l = listing();
    const dora = l.voices.find((v) => v.id === "kokoro/ef_dora")!;
    expect(voiceNote(l, dora)).toBe(
      "Download Kokoro on this computer to use this voice.",
    );
    const phone: VoiceListing = {
      ...l,
      platform: "ios",
      naturalWhere: "hub",
      models: [{ ...l.models[0], installed: true }],
    };
    expect(voiceNote(phone, dora)).toContain("made on the hub's computer");
    expect(voiceNote(phone, undefined)).toBe(
      "The phone's default voice for the language.",
    );
    expect(
      voiceNote(
        {
          ...phone,
          models: [{ ...l.models[0], installed: true, needs: "espeak-ng" }],
        },
        dora,
      ),
    ).toContain("hub's computer needs espeak-ng");
    expect(voiceNote(phone, { ...l.voices[0], network: true })).toContain(
      "maker's servers",
    );
  });

  it("groups the device's voices and the natural ones, with the download", () => {
    const html = renderToStaticMarkup(
      createElement(VoicePicker, {
        lang: "es",
        listing: listing(),
        pending: "kokoro/ef_dora",
        installs: {},
        disabled: false,
        onChoose: () => {},
        onPreview: () => {},
        onDownload: () => {},
      }),
    );
    expect(html).toContain('<optgroup label="This Mac">');
    expect(html).toContain(
      '<optgroup label="Natural voices (made on this computer)">',
    );
    expect(html).toContain("Kokoro Dora · download 350 MB");
    expect(html).toContain("Download Kokoro · 350 MB");
    // A phone's hub-made voices say so, and a phone without the right to download sees none.
    const phone = renderToStaticMarkup(
      createElement(VoicePicker, {
        lang: "es",
        listing: listing({
          platform: "android",
          naturalWhere: "hub",
          canInstall: false,
        }),
        pending: "kokoro/ef_dora",
        installs: {},
        disabled: false,
        onChoose: () => {},
        onPreview: () => {},
        onDownload: () => {},
      }),
    );
    expect(phone).toContain('<optgroup label="This phone">');
    expect(phone).toContain("made on the hub&#x27;s computer");
    expect(phone).not.toContain("Download Kokoro");
  });

  it("shows a chosen voice this device doesn't have rather than dropping it", () => {
    const html = renderToStaticMarkup(
      createElement(VoicePicker, {
        lang: "en",
        listing: listing({ byLanguage: { en: "Gone (Premium)" } }),
        installs: {},
        disabled: false,
        onChoose: () => {},
        onPreview: () => {},
        onDownload: () => {},
      }),
    );
    expect(html).toContain("Gone (Premium) (not on this device)");
  });
});

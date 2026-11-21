// The read-aloud voice for each language (docs/read-aloud.md#choosing-a-voice-in-the-apps),
// as the desktop and mobile apps' Settings › Read-aloud voice shows it. Each host supplies a
// `ReadAloudVoices`: the desktop's lists this computer's voices and twr's natural voices on
// it (`twr voices`); a phone's lists the phone's own voices and twr's natural voices, which
// the hub's computer makes and sends to the phone as audio.

/** The languages a voice is picked for (twr tells them apart per message). */
export const voiceLanguages = ["en", "es"] as const;
export type VoiceLanguage = (typeof voiceLanguages)[number];
export const voiceLanguageLabels: Record<VoiceLanguage, string> = {
  en: "English",
  es: "Spanish",
};

/** One voice a language can be read with. */
export interface VoiceOption {
  /** What the choice saves: a system voice's name or identifier, `<model>/<voice>` for twr's. */
  id: string;
  name: string;
  /**
   * `system`: the device's own voices (macOS's `say`, speech-dispatcher, the phone's);
   * `twr`: twr's natural voices (Kokoro, Supertonic, Piper).
   */
  engine: "system" | "twr";
  model?: string;
  languages: string[];
  locale: string;
  quality: "premium" | "enhanced" | "default" | "neural";
  /** It can speak now: a natural voice's model is installed with what it needs. */
  ready: boolean;
  /** A phone voice that sends the text to its maker's servers (some Android voices). */
  network?: boolean;
}

/** One of twr's natural voice models. */
export interface VoiceModel {
  name: string;
  title: string;
  installed: boolean;
  bytes: number;
  size: string;
  license: string;
  /** Seconds to make one second of speech where it runs, once installed. */
  rtf?: number;
  /** Keeps up with playback there. */
  fast?: boolean;
  /** What that computer lacks for it (espeak-ng, an output device). */
  needs?: string;
}

export interface VoiceListing {
  voices: VoiceOption[];
  models: VoiceModel[];
  /** The voice chosen for each language. */
  byLanguage: Partial<Record<VoiceLanguage, string>>;
  /** `macos`, `linux`, `ios`, `android`: what the notes and the device's group say. */
  platform: string;
  /** Where twr's natural voices are made: here, or on the hub's computer (phones). */
  naturalWhere: "here" | "hub";
  /** Whether this device may download a natural voice model. */
  canInstall: boolean;
  /** Why twr's natural voices aren't offered, if they aren't. */
  naturalProblem?: string;
}

export interface VoiceInstall {
  model: string;
  phase: "downloading" | "installing" | "done" | "error";
  bytes?: number;
  total?: number;
  error?: string;
}

export type ReadAloudVoicesEvent =
  | { type: "install"; install: VoiceInstall }
  /** The voice being previewed; none: the preview ended. */
  | { type: "preview"; voice?: string };

/** A host's voices (DesktopAPI `readAloudVoices`). */
export interface ReadAloudVoices {
  list(): Promise<VoiceListing>;
  /** Sets the voice for `lang` (null: the default voice) and lists again. */
  select(lang: VoiceLanguage, voice: string | null): Promise<VoiceListing>;
  /** Downloads a natural voice model, with `install` events, and lists again. */
  install(model: string): Promise<VoiceListing>;
  /** Says a sample sentence in `lang` with `voice`. */
  preview(voice: string, lang: VoiceLanguage): Promise<void>;
  stopPreview(): Promise<void>;
  onEvent(callback: (event: ReadAloudVoicesEvent) => void): () => void;
  /** Opens where the system's better voices are downloaded (macOS's Spoken Content). */
  openSystemVoices?(): Promise<void>;
}

/** A sentence in each language for Preview. */
export const previewText: Record<VoiceLanguage, string> = {
  en: "Hi. This is how I sound when I read your replies aloud.",
  es: "Hola. Así sueno cuando leo tus respuestas en voz alta.",
};

export const qualityLabel: Record<VoiceOption["quality"], string> = {
  premium: "Premium",
  enhanced: "Enhanced",
  default: "Basic",
  neural: "Natural",
};

const phone = (platform: string) =>
  platform === "ios" || platform === "android";

/** Where the device's own voices are, as the picker's group says it. */
export function deviceLabel(platform: string) {
  return platform === "macos"
    ? "This Mac"
    : phone(platform)
      ? "This phone"
      : "This computer";
}

/** The group of twr's natural voices. */
export function naturalLabel(listing: VoiceListing) {
  return listing.naturalWhere === "hub"
    ? "Natural voices (made on the hub's computer)"
    : "Natural voices (made on this computer)";
}

export function modelOf(listing: VoiceListing, voice?: VoiceOption) {
  return listing.models.find((m) => m.name === voice?.model);
}

/**
 * The voice to suggest for `lang` when none is chosen: the best one ready (Premium,
 * Enhanced, a natural one that keeps up, never a network one), else Kokoro to download.
 */
export function suggestion(listing: VoiceListing, lang: VoiceLanguage) {
  const forLang = listing.voices.filter((v) => v.languages.includes(lang));
  const rank = { premium: 0, enhanced: 1, neural: 2, default: 3 } as const;
  const slow = (v: VoiceOption) => modelOf(listing, v)?.fast === false;
  const ready = forLang
    .filter((v) => v.ready && v.quality !== "default" && !slow(v) && !v.network)
    .sort((a, b) => rank[a.quality] - rank[b.quality]);
  if (ready.length) return { voice: ready[0], download: undefined };
  const kokoro = forLang.find((v) => v.model === "kokoro");
  const model = listing.models.find((m) => m.name === "kokoro");
  return kokoro && model && !model.installed && listing.canInstall
    ? { voice: kokoro, download: model }
    : undefined;
}

/** What Settings says about `voice` on this device. */
export function voiceNote(listing: VoiceListing, voice?: VoiceOption) {
  const { platform } = listing;
  if (!voice)
    return platform === "macos"
      ? "The Mac's default voice."
      : phone(platform)
        ? "The phone's default voice for the language."
        : "The system's default voice (speech-dispatcher).";
  if (voice.engine === "system") {
    if (voice.network)
      return "This voice sends what it reads to its maker's servers. Pick one without “online” to keep it on the phone.";
    if (voice.quality === "default")
      return "A basic voice. Premium and Enhanced voices sound far better.";
    return phone(platform)
      ? `The phone's ${qualityLabel[voice.quality]} voice.`
      : `macOS ${qualityLabel[voice.quality]} voice.`;
  }
  const model = modelOf(listing, voice);
  const where =
    listing.naturalWhere === "hub" ? "the hub's computer" : "this computer";
  if (!model?.installed)
    return listing.canInstall
      ? `Download ${model?.title ?? "it"} on ${where} to use this voice.`
      : `${model?.title ?? "It"} isn't on ${where}. A device with operate permission, or twr voices install there, can download it.`;
  if (model.needs?.includes("espeak-ng"))
    return listing.naturalWhere === "hub"
      ? "The hub's computer needs espeak-ng for it (brew install espeak-ng, or your distribution's package), or pick a Supertonic voice."
      : platform === "macos"
        ? "Needs espeak-ng: run `brew install espeak-ng`, or pick a Supertonic voice."
        : "Needs espeak-ng: install it from your distribution (`pacman -S espeak-ng`, `apt install espeak-ng`), or pick a Supertonic voice.";
  if (model.needs) return `Needs ${model.needs}.`;
  if (model.fast === false)
    return `${listing.naturalWhere === "hub" ? "The hub's computer" : "This computer"} may be too slow for it: replies can pause between sentences. Try Supertonic or Piper.`;
  return listing.naturalWhere === "hub"
    ? "Natural voice, made on the hub's computer and sent to this phone as audio."
    : "Natural voice, made on this computer.";
}

/** A download's progress, 0-100. */
export function percent(install?: VoiceInstall) {
  if (!install?.total || !install.bytes) return 0;
  return Math.min(100, Math.round((install.bytes * 100) / install.total));
}

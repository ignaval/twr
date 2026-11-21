import { useEffect, useState } from "react";
import { errorMessage } from "./errors";
import {
  deviceLabel,
  modelOf,
  naturalLabel,
  percent,
  qualityLabel,
  suggestion,
  voiceLanguageLabels,
  voiceLanguages,
  voiceNote,
  type ReadAloudVoices,
  type VoiceInstall,
  type VoiceLanguage,
  type VoiceListing,
  type VoiceModel,
} from "./readAloudVoices";

/** One language's picker: the device's voices and twr's natural ones, a preview, a download. */
export function VoicePicker({
  lang,
  listing,
  pending,
  previewing,
  installs,
  disabled,
  onChoose,
  onPreview,
  onDownload,
}: {
  lang: VoiceLanguage;
  listing: VoiceListing;
  /** A natural voice chosen but not downloaded yet: saved once it is. */
  pending?: string;
  previewing?: string;
  installs: Record<string, VoiceInstall>;
  disabled: boolean;
  onChoose: (voice: string | null) => void;
  onPreview: (voice: string) => void;
  onDownload: (model: VoiceModel) => void;
}) {
  const chosenId = pending ?? listing.byLanguage[lang] ?? "";
  const voices = listing.voices.filter((v) => v.languages.includes(lang));
  const known = voices.some((v) => v.id === chosenId);
  const chosen = voices.find((v) => v.id === chosenId);
  const device = voices.filter((v) => v.engine === "system");
  const natural = voices.filter((v) => v.engine === "twr");
  const model = modelOf(listing, chosen);
  const install = model ? installs[model.name] : undefined;
  const downloading =
    install?.phase === "downloading" || install?.phase === "installing";
  // The best voice for the language, marked in the list.
  const hint = suggestion(listing, lang);
  const label = voiceLanguageLabels[lang];
  const id = `read-aloud-voice-${lang}`;
  const playing =
    !!previewing && previewing === (chosenId || `default:${lang}`);
  return (
    <div className="read-aloud-voice">
      <label className="field" htmlFor={id}>
        <span>{label}</span>
      </label>
      <div className="voice-controls">
        <select
          id={id}
          value={chosenId}
          disabled={disabled}
          onChange={(e) => onChoose(e.target.value || null)}
        >
          <option value="">Default voice</option>
          {chosenId && !known ? (
            <option value={chosenId}>{chosenId} (not on this device)</option>
          ) : null}
          {device.length ? (
            <optgroup label={deviceLabel(listing.platform)}>
              {device.map((v) => (
                <option key={v.id} value={v.id}>
                  {v.name}
                  {v.quality === "default"
                    ? ""
                    : ` · ${qualityLabel[v.quality]}`}
                  {v.network ? " · online" : ""}
                  {hint?.voice.id === v.id ? " (recommended)" : ""}
                </option>
              ))}
            </optgroup>
          ) : null}
          {natural.length ? (
            <optgroup label={naturalLabel(listing)}>
              {natural.map((v) => {
                const m = modelOf(listing, v);
                return (
                  <option key={v.id} value={v.id}>
                    {v.name}
                    {m && !m.installed ? ` · download ${m.size}` : ""}
                    {m?.fast === false ? " · slow there" : ""}
                    {hint?.voice.id === v.id ? " (recommended)" : ""}
                  </option>
                );
              })}
            </optgroup>
          ) : null}
        </select>
        <button
          type="button"
          aria-pressed={playing}
          disabled={
            disabled ||
            (!!chosen && !chosen.ready) ||
            // A computer's default voice is the system's own: there is nothing to try.
            (!chosenId && listing.naturalWhere === "here")
          }
          onClick={() => onPreview(chosenId)}
        >
          {playing ? "Stop" : "Preview"}
        </button>
      </div>
      <small className="read-aloud-voice-note" aria-live="polite">
        {install?.phase === "error"
          ? `Download failed: ${install.error}`
          : downloading
            ? install!.phase === "installing"
              ? `Installing ${model!.title} and timing it…`
              : `Downloading ${model!.title}… ${percent(install)}%`
            : voiceNote(listing, chosen)}
      </small>
      {downloading ? (
        <progress
          max={100}
          value={percent(install)}
          aria-label={`Downloading ${model!.title}`}
        />
      ) : model && !model.installed && listing.canInstall ? (
        <button
          type="button"
          className="primary"
          disabled={disabled}
          onClick={() => onDownload(model)}
        >
          Download {model.title} · {model.size}
        </button>
      ) : null}
    </div>
  );
}

/**
 * Settings › Read-aloud voice: the voice each language is read with on this device, among
 * its own voices and twr's natural ones.
 */
export function ReadAloudVoiceOptions({ voices }: { voices: ReadAloudVoices }) {
  const [listing, setListing] = useState<VoiceListing>();
  const [problem, setProblem] = useState<string>();
  const [error, setError] = useState<string>();
  const [pending, setPending] = useState<
    Partial<Record<VoiceLanguage, string>>
  >({});
  const [installs, setInstalls] = useState<Record<string, VoiceInstall>>({});
  const [previewing, setPreviewing] = useState<string>();
  const [busy, setBusy] = useState(false);
  useEffect(() => {
    let active = true;
    void voices
      .list()
      .then((l) => active && setListing(l))
      .catch((e) => active && setProblem(errorMessage(e)));
    const off = voices.onEvent((e) => {
      if (!active) return;
      if (e.type === "install")
        setInstalls((all) => ({ ...all, [e.install.model]: e.install }));
      else setPreviewing(e.voice);
    });
    return () => {
      active = false;
      off();
      void voices.stopPreview().catch(() => {});
    };
  }, [voices]);
  const fail = (e: unknown) => setError(errorMessage(e));
  async function save(lang: VoiceLanguage, voice: string | null) {
    setBusy(true);
    try {
      setListing(await voices.select(lang, voice));
      setError(undefined);
    } catch (e) {
      fail(e);
    } finally {
      setBusy(false);
    }
  }
  async function choose(lang: VoiceLanguage, voice: string | null) {
    const v = listing?.voices.find((x) => x.id === voice);
    if (v && !v.ready && modelOf(listing!, v)?.installed === false) {
      // Saved once its model is downloaded: it can't speak before.
      setPending((p) => ({ ...p, [lang]: v.id }));
      return;
    }
    setPending((p) => ({ ...p, [lang]: undefined }));
    await save(lang, voice);
  }
  async function download(model: VoiceModel) {
    try {
      const next = await voices.install(model.name);
      setListing(next);
      for (const lang of voiceLanguages) {
        const want = pending[lang];
        if (
          want &&
          next.voices.find((v) => v.id === want)?.model === model.name
        ) {
          setPending((p) => ({ ...p, [lang]: undefined }));
          await save(lang, want);
        }
      }
    } catch (e) {
      fail(e);
    }
  }
  async function preview(lang: VoiceLanguage, voice: string) {
    const key = voice || `default:${lang}`;
    if (previewing === key) {
      await voices.stopPreview().catch(fail);
      return;
    }
    try {
      await voices.preview(voice, lang);
      setError(undefined);
    } catch (e) {
      fail(e);
    }
  }
  return (
    <section className="settings-section read-aloud-voices">
      <h2>Read-aloud voice</h2>
      <p>
        Each reply is read with the voice of its language.{" "}
        {listing?.naturalWhere === "hub"
          ? "The phone's own voices speak on the phone. Natural voices are made on the hub's computer and sent here as audio, over the same encrypted connection as your messages."
          : "Natural voices are downloaded once from GitHub and run on this computer; nothing you hear is sent anywhere."}
      </p>
      {problem ? (
        <p role="status">{problem}</p>
      ) : !listing ? (
        <p role="status">Checking voices…</p>
      ) : (
        <>
          {voiceLanguages.map((lang) => (
            <VoicePicker
              key={lang}
              lang={lang}
              listing={listing}
              pending={pending[lang]}
              previewing={previewing}
              installs={installs}
              disabled={busy}
              onChoose={(v) => void choose(lang, v)}
              onPreview={(v) => void preview(lang, v)}
              onDownload={(m) => void download(m)}
            />
          ))}
          {listing.naturalProblem ? (
            <p className="read-aloud-voice-note" role="status">
              Natural voices: {listing.naturalProblem}
            </p>
          ) : null}
          {voices.openSystemVoices && listing.platform === "macos" ? (
            <button
              type="button"
              onClick={() => void voices.openSystemVoices!().catch(fail)}
            >
              Get more Mac voices…
            </button>
          ) : null}
        </>
      )}
      {error ? (
        <p className="error-text" role="alert">
          {error}
        </p>
      ) : null}
    </section>
  );
}

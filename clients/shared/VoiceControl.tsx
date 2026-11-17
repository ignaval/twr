// Voice mode's controls in the composer (docs/voice.md): a microphone button (click to talk,
// click again to transcribe; or hold it and let go), a hands-free session, what was heard, and a
// transcript the hub handed back for confirmation. Recognition and playback stay on this
// device; hands-free sends through `say` with `voice: true`, dictation fills the composer.
import {
  useCallback,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
} from "react";
import { createPortal } from "react-dom";
import {
  AudioLines,
  Download,
  Mic,
  MicOff,
  Square,
  VolumeX,
} from "lucide-react";
import type { DesktopAPI, Json } from "./types";
import {
  VoiceSession,
  createNativeCapture,
  createWebCapture,
  type SpeakerState,
  type VoiceCapture,
  type VoiceCaptureStatus,
  type VoiceSessionState,
  type VoiceSpeaker,
} from "./voice";

const PHASES: Record<string, string> = {
  listening: "Listening",
  hearing: "Hearing you",
  transcribing: "Transcribing",
  sending: "Sending to orchestrator",
  error: "Voice stopped",
};

const HOLD_MS = 350;

/**
 * Runs a call nobody waits for. A view's host calls fail once its hub connection is closed
 * (switching hubs, forgetting one): cleanup then has nothing left to undo.
 */
const quietly = (call: Promise<unknown> | undefined) => {
  void call?.catch(() => {});
};

function voiceCapture(host: DesktopAPI): VoiceCapture | undefined {
  if (host.nativeVoice) return createNativeCapture(host.nativeVoice);
  if (host.voiceTranscribe && host.voiceStatus && navigator.mediaDevices)
    return createWebCapture(
      async (wav, hint) => (await host.voiceTranscribe!(wav, hint)).text,
      () => host.voiceStatus!(),
    );
  return undefined;
}

export interface VoiceControlProps {
  host: DesktopAPI;
  /** Inline composer slot: status never floats over the conversation or editor. */
  statusContainer: HTMLDivElement | null;
  /** Changing conversation cancels recognition before it can reach a new draft. */
  context: string;
  rpc: (method: string, params?: Json) => Promise<Json>;
  connected: boolean;
  allowed: boolean;
  conversationAllowed: boolean;
  /** Sends a transcript to the orchestrator (`say`, `voice: true`). */
  submit(text: string): Promise<Json>;
  /** Dictation fills the current composer without sending. */
  draft(text: string): void;
  hint: string[];
  /** The orchestrator's last messages: what this device may have read aloud. */
  spoken(): string[];
  /** Puts a transcript in the message box, to send as typed. */
  edit(text: string): void;
  onError(error: unknown): void;
}

export function VoiceControl(props: VoiceControlProps) {
  const { host, rpc } = props;
  const capture = useMemo(() => voiceCapture(host), [host]);
  const mounted = useRef(true);
  const [state, setState] = useState<VoiceSessionState>({
    phase: "off",
    mode: "push",
    level: 0,
    partial: "",
  });
  const [status, setStatus] = useState<VoiceCaptureStatus>();
  /** Downloading the speech model (`twr voice setup`). Success needs no prompt. */
  const [setup, setSetup] = useState(false);
  const [speakerInfo, setSpeakerInfo] = useState<
    SpeakerState & {
      running?: boolean;
      active?: boolean;
      name?: string;
      device?: string;
    }
  >();
  const speakerRef = useRef(speakerInfo);
  speakerRef.current = speakerInfo;
  const latest = useRef(props);
  latest.current = props;
  const pressedAt = useRef(0);
  const readyRef = useRef<() => Promise<boolean>>(async () => false);
  const wasRecording = useRef(false);

  // This device's speaker: the desktop's own `twr speak`, or the mobile one. When it is not
  // the one speaking (another speaker of this computer is), controls go through the hub.
  const speaker = useMemo<VoiceSpeaker | undefined>(() => {
    const local = host.voiceSpeaker
      ? (op: "pause" | "resume" | "skip" | "stop") =>
          host.voiceSpeaker!(op === "stop" ? "silence" : op)
      : host.nativeVoice?.speaker
        ? (op: "pause" | "resume" | "skip" | "stop") =>
            host.nativeVoice!.speaker!(op === "stop" ? "silence" : op)
        : undefined;
    if (!local) return undefined;
    // Another speaker of this computer (the TUI's own) may speak for it while this one
    // stands by: they share the device id, so the hub reaches whichever speaks.
    const target = () =>
      speakerRef.current?.device || speakerRef.current?.name || undefined;
    return {
      async control(op) {
        const info = speakerRef.current;
        if (info?.running && info.active !== false) await local(op);
        else await rpc("speech.control", { op, speaker: target() });
      },
      async live(on) {
        if (host.voiceSpeaker) {
          const started = await host.voiceSpeaker(on ? "start" : "stop");
          if (!on) return;
          // The speaker attaches in a moment; live reading is turned on once one of this
          // computer's has.
          let last: unknown;
          for (let tries = 0; tries < 20; tries++) {
            try {
              await rpc("speech.control", {
                op: "live",
                value: true,
                speaker: target() || started.device || started.name,
              });
              return;
            } catch (error) {
              last = error;
              await new Promise((r) => setTimeout(r, 250));
            }
          }
          throw last;
        } else await host.nativeVoice?.speaker?.(on ? "start" : "stop");
      },
    };
  }, [host, rpc]);

  const session = useMemo(
    () =>
      capture
        ? new VoiceSession({
            capture,
            speaker,
            submit: async (text) => latest.current.submit(text),
            draft: (text) => latest.current.draft(text),
            repeat: async () => {
              await rpc("speech.request", {
                last: 1,
                ...(speakerRef.current?.name
                  ? { speaker: speakerRef.current.name }
                  : {}),
              });
            },
            hint: () => latest.current.hint,
            spoken: () => latest.current.spoken(),
            prepare: () => readyRef.current(),
            onChange: setState,
          })
        : undefined,
    [capture, speaker, rpc],
  );

  useEffect(() => {
    const off = host.onEvent((event) => {
      if (event.type === "voice-speaker" && event.speaker) {
        const sp = event.speaker;
        setSpeakerInfo(sp);
        // Standing by, it can't see what this computer's speaking speaker plays.
        session?.speakerState({ ...sp, unknown: sp.running && !sp.active });
      }
    });
    const native = host.nativeVoice?.onEvent((event) => {
      if (event.type === "speaker" && event.speaker) {
        setSpeakerInfo({ ...event.speaker, active: true });
        session?.speakerState(event.speaker);
      }
    });
    return () => {
      off();
      native?.();
    };
  }, [host, session]);

  useLayoutEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
      quietly(session?.end());
      quietly(host.voiceArm?.(false));
    };
  }, [props.context, session, host]);

  const ready = useCallback(async () => {
    if (!capture) return false;
    const context = latest.current.context;
    const s = await capture.status().catch((error): VoiceCaptureStatus => ({
      available: false,
      local: false,
      problem: String(error),
    }));
    if (
      context !== latest.current.context ||
      !mounted.current ||
      !latest.current.connected ||
      !latest.current.allowed
    )
      return false;
    setStatus(s);
    if (!s.available) return false;
    if (session)
      session.options = {
        ...session.options,
        listenWhileSpeaking: Boolean(s.listenWhileSpeaking),
      };
    await host.voiceArm?.(true);
    if (
      context !== latest.current.context ||
      !mounted.current ||
      !latest.current.connected ||
      !latest.current.allowed
    ) {
      await host.voiceArm?.(false);
      return false;
    }
    return true;
  }, [capture, host, session]);
  readyRef.current = ready;

  const downloadModel = useCallback(async () => {
    if (!host.voiceSetup) return;
    setSetup(true);
    try {
      const s = await host.voiceSetup();
      setStatus(s.available ? undefined : s);
      setSetup(false);
    } catch (error) {
      setSetup(false);
      setStatus({
        available: false,
        local: false,
        setup: true,
        problem: `The speech model did not download: ${error instanceof Error ? error.message : String(error)}`,
      });
    }
  }, [host]);

  // The microphone is granted while something is recorded or a session listens, and taken
  // back once push to talk has finished (or dropped) its recording.
  const armedFor =
    state.mode === "hands-free"
      ? state.phase !== "off"
      : ["hearing"].includes(state.phase);
  useEffect(() => {
    if (!armedFor && state.phase !== "off") quietly(host.voiceArm?.(false));
  }, [armedFor, state.phase, host]);

  // Losing the connection or permission cancels recognition, including late results.
  useEffect(() => {
    if ((!props.connected || !props.allowed) && state.phase !== "off")
      quietly(session?.end().then(() => host.voiceArm?.(false)));
  }, [props.connected, props.allowed, state.phase, session, host]);

  // Mod+Shift+Space: talk, then transcribe. Mod+Shift+L: a hands-free session on or off.
  const keys = useRef<(event: KeyboardEvent) => void>(() => {});
  useEffect(() => {
    const listener = (event: KeyboardEvent) => keys.current(event);
    window.addEventListener("keydown", listener);
    return () => window.removeEventListener("keydown", listener);
  }, []);

  if (!session || !capture) return null;
  const on = state.phase !== "off";
  const active = !["off", "ready", "error"].includes(state.phase);
  const unavailable = Boolean(status && !status.available);
  const showStatus = active || unavailable || Boolean(state.error) || setup;
  const recording = state.mode === "push" && state.phase === "hearing";
  const handsFree = state.mode === "hands-free" && on;
  const disabled = !props.allowed || !props.connected;
  // Busy: a recording is being opened, recognized or sent; a new one waits for it.
  const busy = !session.idle && !recording;

  const start = async () => {
    try {
      await session.press();
    } catch (error) {
      props.onError(error);
    }
  };
  // A click starts recording and the next click transcribes; a long press is hold-to-talk, and
  // letting go transcribes.
  const micDown = (event: React.PointerEvent<HTMLButtonElement>) => {
    if (disabled || handsFree || busy || event.button !== 0) return;
    event.currentTarget.setPointerCapture?.(event.pointerId);
    pressedAt.current = Date.now();
    wasRecording.current = recording;
    if (!recording) void start();
  };
  const micUp = () => {
    if (!wasRecording.current && Date.now() - pressedAt.current > HOLD_MS)
      session.release().catch(props.onError);
  };
  const micClick = (event: React.MouseEvent) => {
    if (event.detail === 0 && !disabled && !handsFree && !busy) {
      if (recording) session.release().catch(props.onError);
      else void start();
      return;
    }
    if (wasRecording.current) session.release().catch(props.onError);
    wasRecording.current = false;
  };
  const toggleHandsFree = async () => {
    try {
      if (handsFree) {
        await session.end();
        await host.voiceArm?.(false);
        return;
      }
      await session.begin();
    } catch (error) {
      props.onError(error);
    }
  };

  keys.current = (event) => {
    const mac = ["darwin", "ios"].includes(
      document.documentElement.dataset.platform || "",
    );
    if (!(mac ? event.metaKey : event.ctrlKey) || !event.shiftKey) return;
    if (event.code === "Space") {
      event.preventDefault();
      if (disabled || handsFree) return;
      if (recording) session.release().catch(props.onError);
      else if (!busy) void start();
    } else if (event.code === "KeyL") {
      event.preventDefault();
      if (!disabled && props.conversationAllowed) void toggleHandsFree();
    }
  };

  const label =
    state.phase === "listening" && state.deaf
      ? "Speaking · listening again when it ends"
      : state.phase === "listening" &&
          speakerInfo?.playing &&
          !speakerInfo.paused
        ? "Speaking · talk to interrupt"
        : PHASES[state.phase];

  const feedback = (
    <>
      {showStatus && (
        <div className="voice-bar" role="status" aria-live="polite">
          {setup ? (
            <span className="voice-phase">
              <Download size={14} /> Downloading the speech model (148 MB)…
            </span>
          ) : status && !status.available && status.setup && host.voiceSetup ? (
            <span className="voice-problem voice-setup">
              <MicOff size={14} />
              {status.problem?.startsWith("The speech model did not download")
                ? status.problem
                : "Voice needs a speech model: whisper’s base.en, 148 MB, downloaded once and kept on this computer."}
            </span>
          ) : status && !status.available ? (
            <span className="voice-problem">
              <MicOff size={14} /> {status.problem}
            </span>
          ) : (
            <>
              <span className={`voice-phase voice-${state.phase}`}>
                <span
                  className="voice-level"
                  style={{ transform: `scaleY(${0.2 + state.level * 0.8})` }}
                />
                {label}
                {status && !status.local && (
                  <small title="[voice] engine = command: your program receives the audio">
                    {" "}
                    · audio leaves this device
                  </small>
                )}
              </span>
              {(state.partial || state.heard) && (
                <span className="voice-heard">
                  {state.partial ||
                    (state.heard &&
                      (state.heard.outcome === "echo"
                        ? "Ignored its own voice"
                        : state.heard.outcome === "control"
                          ? `“${state.heard.text}”`
                          : state.heard.outcome === "draft"
                            ? "Added to message box · edit, then Send"
                            : state.heard.outcome === "confirm"
                              ? `Not sent yet: “${state.heard.text}”`
                              : `Sent “${state.heard.text}”`))}
                </span>
              )}
              {state.error && (
                <span className="voice-problem">{state.error}</span>
              )}
            </>
          )}
          {speakerInfo?.playing && (
            <button
              type="button"
              onClick={() => void speaker?.control("stop").catch(props.onError)}
              title="Stop reading aloud"
            >
              <VolumeX size={14} /> Quiet
            </button>
          )}
          {state.mode === "push" &&
            ["hearing", "transcribing"].includes(state.phase) && (
              <button type="button" onClick={() => quietly(session.end())}>
                Cancel dictation
              </button>
            )}
          {handsFree && (
            <button type="button" onClick={() => void toggleHandsFree()}>
              <Square size={12} /> Stop listening
            </button>
          )}
          {!active &&
            !setup &&
            status?.setup &&
            !status.available &&
            host.voiceSetup && (
              <button type="button" onClick={() => void downloadModel()}>
                <Download size={14} /> Download model
              </button>
            )}
          {!active && !setup && (unavailable || state.error) && (
            <button
              type="button"
              onClick={() => {
                setStatus(undefined);
                setSetup(false);
                quietly(session.end());
              }}
            >
              Dismiss
            </button>
          )}
        </div>
      )}
      {state.confirm && (
        <div
          className="voice-confirm"
          role="alertdialog"
          aria-label="Confirm spoken message"
        >
          <p>
            Heard <b>“{state.confirm.text}”</b>. It reads as an answer to a
            request or a mode change, and a spoken message never gives one by
            itself.
          </p>
          <div>
            <button
              type="button"
              className="primary"
              onClick={() => {
                props.edit(state.confirm!.text);
                session.dismissConfirm();
              }}
            >
              Review in message box
            </button>
            <button type="button" onClick={() => session.dismissConfirm()}>
              Discard
            </button>
          </div>
        </div>
      )}
    </>
  );

  return (
    <>
      {props.statusContainer && createPortal(feedback, props.statusContainer)}
      <button
        type="button"
        className={`voice-button ${recording ? "recording" : ""}`}
        aria-pressed={recording}
        aria-label={
          recording ? "Transcribe what you said" : "Dictate a message"
        }
        title={
          recording
            ? "Click or let go to add text to the message box"
            : "Dictate a message (click, or hold) · Mod+Shift+Space"
        }
        disabled={disabled || handsFree || busy}
        onPointerDown={micDown}
        onPointerUp={micUp}
        onPointerCancel={() => quietly(session.end())}
        onClick={micClick}
        data-voice-push
      >
        <Mic size={18} />
      </button>
      <button
        type="button"
        className={`voice-button ${handsFree ? "recording" : ""}`}
        aria-pressed={handsFree}
        aria-label={handsFree ? "Stop hands-free voice" : "Hands-free voice"}
        title="Hands-free conversation: automatically send to the orchestrator and read replies aloud · Mod+Shift+L"
        disabled={disabled || !props.conversationAllowed}
        onClick={() => void toggleHandsFree()}
        data-voice-hands-free
      >
        <AudioLines size={18} />
      </button>
    </>
  );
}

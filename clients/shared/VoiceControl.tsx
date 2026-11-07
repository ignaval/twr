// Voice mode's controls in the composer (docs/voice.md): a microphone button (click to talk,
// click again to send; or hold it and let go), a hands-free session, what was heard, and a
// transcript the hub handed back for confirmation. Recognition and playback stay on this
// device; the text goes to the orchestrator through `say` with `voice: true`.
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { AudioLines, Mic, MicOff, Square, VolumeX } from "lucide-react";
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
  ready: "Voice ready",
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
  rpc: (method: string, params?: Json) => Promise<Json>;
  connected: boolean;
  allowed: boolean;
  /** Sends a transcript to the orchestrator (`say`, `voice: true`). */
  submit(text: string): Promise<Json>;
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
  const [state, setState] = useState<VoiceSessionState>({
    phase: "off",
    mode: "push",
    level: 0,
    partial: "",
  });
  const [status, setStatus] = useState<VoiceCaptureStatus>();
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

  useEffect(
    () => () => {
      quietly(session?.end());
      quietly(host.voiceArm?.(false));
    },
    [session, host],
  );

  const ready = useCallback(async () => {
    if (!capture) return false;
    const s = await capture.status().catch((error): VoiceCaptureStatus => ({
      available: false,
      local: false,
      problem: String(error),
    }));
    setStatus(s);
    if (!s.available) return false;
    await host.voiceArm?.(true);
    return true;
  }, [capture, host]);
  readyRef.current = ready;

  // The microphone is granted while something is recorded or a session listens, and taken
  // back once push to talk has sent (or dropped) its recording.
  const armedFor =
    state.mode === "hands-free"
      ? state.phase !== "off"
      : ["hearing"].includes(state.phase);
  useEffect(() => {
    if (!armedFor && state.phase !== "off") quietly(host.voiceArm?.(false));
  }, [armedFor, state.phase, host]);

  // With the hub gone, nothing heard could be sent: a hands-free session ends.
  useEffect(() => {
    if (
      !props.connected &&
      state.mode === "hands-free" &&
      state.phase !== "off"
    )
      quietly(session?.end().then(() => host.voiceArm?.(false)));
  }, [props.connected, state.mode, state.phase, session, host]);

  // Mod+Shift+Space: talk, then send. Mod+Shift+L: a hands-free session on or off.
  const keys = useRef<(event: KeyboardEvent) => void>(() => {});
  useEffect(() => {
    const listener = (event: KeyboardEvent) => keys.current(event);
    window.addEventListener("keydown", listener);
    return () => window.removeEventListener("keydown", listener);
  }, []);

  if (!session || !capture) return null;
  const on = state.phase !== "off";
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
  // A click starts recording and the next click sends; a long press is hold-to-talk, and
  // letting go sends.
  const micDown = () => {
    if (disabled || handsFree || busy) return;
    pressedAt.current = Date.now();
    wasRecording.current = recording;
    if (!recording) void start();
  };
  const micUp = () => {
    if (!wasRecording.current && Date.now() - pressedAt.current > HOLD_MS)
      session.release().catch(props.onError);
  };
  const micClick = () => {
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
      if (!disabled) void toggleHandsFree();
    }
  };

  const label =
    state.phase === "listening" && speakerInfo?.playing && !speakerInfo.paused
      ? "Speaking · talk to interrupt"
      : PHASES[state.phase];

  return (
    <>
      {(on || status?.problem || state.confirm) && (
        <div className="voice-bar" role="status" aria-live="polite">
          {status && !status.available ? (
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
          {handsFree && (
            <button type="button" onClick={() => void toggleHandsFree()}>
              <Square size={12} /> Stop listening
            </button>
          )}
          {!on && status?.problem && (
            <button type="button" onClick={() => setStatus(undefined)}>
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
      <button
        type="button"
        className={`voice-button ${recording ? "recording" : ""}`}
        aria-pressed={recording}
        aria-label={
          recording ? "Send what you said" : "Talk to the orchestrator"
        }
        title={
          recording
            ? "Click or let go to send"
            : "Talk to the orchestrator (click, or hold) · Mod+Shift+Space"
        }
        disabled={disabled || handsFree || busy}
        onPointerDown={micDown}
        onPointerUp={micUp}
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
        title="Hands-free: listen and read replies aloud until you stop · Mod+Shift+L"
        disabled={disabled}
        onClick={() => void toggleHandsFree()}
        data-voice-hands-free
      >
        <AudioLines size={18} />
      </button>
    </>
  );
}

import { useEffect, useRef, useState } from "react";
import { AlertCircle, Volume2, VolumeX } from "lucide-react";
import { errorMessage } from "./errors";
import type { DesktopAPI, Json } from "./types";

interface Speaker {
  device: string;
  live: boolean;
  paused: boolean;
  error?: string;
}

/** The existing desktop speaker's workspace live switch, independent of microphone use. */
export function ReadAloudControl({
  host,
  rpc,
  workspace,
  connected,
}: {
  host: DesktopAPI;
  rpc(method: string, params?: Json): Promise<Json>;
  workspace: string;
  connected: boolean;
}) {
  const [speaker, setSpeaker] = useState<Speaker>();
  const [problem, setProblem] = useState<string>();
  const [busy, setBusy] = useState(false);
  const toggle = useRef<() => Promise<void>>(async () => {});

  useEffect(() => {
    setSpeaker(undefined);
    setProblem(undefined);
    setBusy(false);
    toggle.current = async () => {};
    if (!connected || !host.voiceSpeaker) return;

    let active = true;
    let device: string | undefined;
    let current: Speaker | undefined;
    let changing = false;
    let revision = 0;
    let processError: string | undefined;
    let operationError: string | undefined;
    const refresh = async () => {
      if (!active || !device) return;
      const read = ++revision;
      try {
        // Status is from this workspace's hub, not the desktop's aggregate playback state.
        const status = await rpc("speech.status");
        if (!active || read !== revision) return;
        current = status.speakers?.find((s: Speaker) => s.device === device);
        setSpeaker(current);
        setProblem(
          operationError ||
            processError ||
            current?.error ||
            (!current ? "Waiting for this computer's speaker" : undefined),
        );
        return current;
      } catch (error) {
        if (!active || read !== revision) return;
        current = undefined;
        setSpeaker(undefined);
        setProblem(errorMessage(error));
      }
    };
    const start = async () => {
      // Starting attaches the existing speaker with its persisted live setting. It does
      // not turn live reading on, arm the microphone, or require a recognition model.
      const started = await host.voiceSpeaker!("start");
      if (!active) return;
      device = started.device || device;
      processError = started.error;
      if (processError) setProblem(processError);
      await refresh();
    };
    const change = async () => {
      if (!active || changing) return;
      changing = true;
      setBusy(true);
      operationError = undefined;
      try {
        if (!current) {
          await start();
          return;
        }
        const on = !current.live;
        // A standby desktop speaker shares its device with the local TUI's active one.
        // Never omit the target: that would change every device attached to this hub.
        const result = await rpc("speech.control", {
          op: "live",
          value: on,
          speaker: current.device,
        });
        if (!active) return;
        if (result.speakers !== 1)
          throw new Error("This computer's speaker did not accept the change");
        // Delivery is not acknowledgement. Show only the speaker's reported setting.
        for (let tries = 0; active && tries < 20; tries++) {
          if ((await refresh())?.live === on) return;
          await new Promise((resolve) => setTimeout(resolve, 250));
        }
        if (active)
          throw new Error(
            "This computer's speaker has not confirmed the change",
          );
      } catch (error) {
        operationError = errorMessage(error);
        if (active) setProblem(operationError);
      } finally {
        changing = false;
        if (active) setBusy(false);
      }
    };
    toggle.current = change;
    const unsubscribe = host.onEvent((event) => {
      if (event.type !== "voice-speaker" || !event.speaker) return;
      device = event.speaker.device || device;
      processError = event.speaker.error;
      void refresh();
    });
    // Poll too: CLI/TUI live controls and another local speaker's state do not emit
    // renderer events. Retry failed reads and missing attachments across reconnects.
    const timer = setInterval(() => void refresh(), 1000);
    void (async () => {
      setBusy(true);
      changing = true;
      try {
        await start();
      } catch (error) {
        if (active) setProblem(errorMessage(error));
      } finally {
        changing = false;
        if (active) setBusy(false);
      }
    })();
    return () => {
      active = false;
      unsubscribe();
      clearInterval(timer);
    };
  }, [host, rpc, workspace, connected]);

  // Remote desktops and phones do not expose this local speaker bridge. The mobile
  // speaker start operation also forces live on, so it cannot restore this setting.
  if (!host.voiceSpeaker) return null;
  const on = connected && speaker?.live;
  const status = !connected
    ? "Disconnected"
    : speaker
      ? on
        ? "On"
        : "Off"
      : problem
        ? "Unavailable"
        : "Connecting to this computer's speaker";
  return (
    <button
      type="button"
      className={`icon-button no-drag ${on ? "on" : ""}`}
      aria-label="Automatically read orchestrator replies aloud on this computer"
      aria-pressed={connected && speaker ? speaker.live : "mixed"}
      aria-busy={busy || undefined}
      title={`Automatic reading aloud: ${status}${problem ? " · " + problem : ""}${speaker?.paused ? " · playback paused" : ""}. Orchestrator replies in ${workspace}, including while viewing a deep dive.${connected && !speaker && !busy ? " Click to reconnect the speaker." : ""}`}
      disabled={!connected || busy}
      onClick={() => void toggle.current()}
    >
      {!connected || !speaker ? (
        <AlertCircle size={16} aria-hidden="true" />
      ) : on ? (
        <Volume2 size={16} aria-hidden="true" />
      ) : (
        <VolumeX size={16} aria-hidden="true" />
      )}
    </button>
  );
}

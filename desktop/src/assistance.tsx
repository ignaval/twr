import { useEffect, useMemo, useRef, useState } from "react";
import type { HubEvent, Json, Thread } from "./types";
import { routineRuns } from "./transcript";

export interface Summary {
  thread: string;
  covers?: number;
  covers_in?: string;
  summary?: string;
  stats?: { calls: number; failures: number; cost_usd: number };
}

export interface Pace {
  at: number;
  next: number;
  basis: string;
  unavailable?: boolean;
}
export function summaryDue(
  prior: Pace | undefined,
  basis: string,
  every: number,
  now: number,
): boolean {
  return (
    !prior?.unavailable &&
    (!prior ||
      (prior.basis !== basis &&
        now - prior.at >= Math.max(every, prior.next) * 1000))
  );
}
export function settledSummary(
  prior: Pace,
  basis: string,
  answer: Json,
  conversation: string,
): Pace {
  const complete =
    answer.covers_in === conversation &&
    answer.covers != null &&
    (!answer.reason || answer.reason === "none");
  return {
    ...prior,
    next: answer.next_after_secs || prior.next,
    basis: complete ? basis : "",
    unavailable: answer.reason?.startsWith("unavailable:"),
  };
}

/** The hub owns the models, eligibility, cache, rate limits and billing statistics.
 * This hook schedules visible presentation work and discards replies after a switch. */
export function useAssistance({
  workspace,
  connected,
  visible,
  target,
  events,
  draft,
  refresh,
  visibleRoutines,
  threads,
}: {
  workspace: string;
  connected: boolean;
  visible: boolean;
  target?: Thread;
  events: HubEvent[];
  draft: string;
  refresh: number;
  visibleRoutines: string[];
  threads: Thread[];
}) {
  const [settings, setSettings] = useState<Json>();
  const [settingsReady, setSettingsReady] = useState(false);
  const [acknowledged, setAcknowledged] = useState(
    () => localStorage.getItem("twr.assistance.notice") || "",
  );
  const [suggestion, setSuggestion] = useState("");
  const [summaries, setSummaries] = useState<Summary[]>([]);
  const [notice, setNotice] = useState("");
  const [stats, setStats] = useState<Json>();
  const asked = useRef(new Map<string, Promise<Json>>());
  const unavailableSuggest = useRef(false);
  const pace = useRef(
    new Map<
      string,
      { at: number; next: number; basis: string; unavailable?: boolean }
    >(),
  );
  const inFlight = useRef(new Set<string>());
  const retrySuggest = useRef(true);
  const retrySummary = useRef(true);
  const fallbackAttempts = useRef(new Map<string, number>());
  const [retryVersion, setRetryVersion] = useState(0);
  const scope = workspace + ":" + (target?.id || "");
  const current = useRef(scope);
  current.current = scope;
  const latest = useRef({
    settings,
    settingsReady,
    events,
    target,
    visible,
    visibleRoutines,
    threads,
  });
  latest.current = {
    settings,
    settingsReady,
    events,
    target,
    visible,
    visibleRoutines,
    threads,
  };
  useEffect(() => {
    let live = true;
    setSettingsReady(false);
    if (!connected) setSettings(undefined);
    if (connected)
      void window.twr
        .configRead()
        .then((value) => {
          if (live)
            setSettings((old: Json) =>
              JSON.stringify(old) === JSON.stringify(value.settings?.tui)
                ? old
                : value.settings?.tui,
            );
          if (live) setSettingsReady(true);
        })
        .catch(() => {});
    return () => {
      live = false;
    };
  }, [workspace, connected, refresh, visible]);
  useEffect(() => {
    setSuggestion("");
    setSummaries([]);
    setNotice("");
    setStats(undefined);
    asked.current.clear();
    unavailableSuggest.current = false;
    pace.current.clear();
    inFlight.current.clear();
  }, [workspace]);
  const disclosureKey = settings ? JSON.stringify(settings.suggest_model) : "";
  const disclosed = !!disclosureKey && acknowledged === disclosureKey;
  const enabled = settings?.suggest || settings?.summaries;
  useEffect(() => {
    unavailableSuggest.current = false;
    asked.current.clear();
    fallbackAttempts.current.clear();
    retrySuggest.current = true;
  }, [settings?.suggest, disclosureKey]);
  useEffect(() => {
    pace.current.clear();
    retrySummary.current = true;
  }, [settings?.summaries, disclosureKey]);
  const lastStart = events.findLast((event) => event.kind === "turn.started");
  const lastEnd = events.findLast((event) => event.kind === "turn.ended");
  const turn =
    lastStart?.data.from === "user" &&
    lastEnd?.turn === lastStart.turn &&
    lastEnd?.data.result !== "error" &&
    lastEnd?.data.result !== "interrupted"
      ? lastEnd?.turn
      : undefined;
  useEffect(() => {
    setSuggestion("");
    if (
      !connected ||
      !visible ||
      !settings?.suggest ||
      !settingsReady ||
      !disclosed ||
      !target ||
      !turn ||
      draft ||
      ["running", "background"].includes(target.status)
    )
      return;
    const key = scope + ":" + disclosureKey + ":" + turn;
    if (unavailableSuggest.current) return;
    let live = true;
    let retryTimer: ReturnType<typeof setTimeout> | undefined;
    const timer = setTimeout(() => {
      let request = asked.current.get(key);
      if (!request) {
        request = window.twr.rpc("suggest.next", {
          thread: target.id,
          turn,
          models: settings.suggest_model,
          retry: retrySuggest.current,
        });
        retrySuggest.current = false;
        asked.current.set(key, request);
      }
      void request
        .then((answer) => {
          if (!live || current.current !== scope) return;
          setSuggestion(answer.suggestion || "");
          setStats({ ...answer.stats, source: "Suggestions" });
          if (answer.reason?.startsWith("retry:")) {
            // The hub intentionally tries only one runtime per ask. Its next ask may
            // use the other runtime; never reset the hub's unavailable-runtime cache.
            const attempts = fallbackAttempts.current.get(key) || 0;
            asked.current.delete(key);
            if (attempts < 2) {
              fallbackAttempts.current.set(key, attempts + 1);
              retryTimer = setTimeout(() => {
                if (live) setRetryVersion((n) => n + 1);
              }, 3100);
            }
          } else if (answer.reason?.startsWith("failed:")) {
            asked.current.delete(key);
            setNotice(
              "Suggestion unavailable right now: " + answer.reason.slice(7),
            );
          }
          if (answer.reason?.startsWith("unavailable:")) {
            unavailableSuggest.current = true;
            setNotice("Suggestions unavailable: " + answer.reason.slice(12));
          }
        })
        .catch((error) => {
          if (asked.current.get(key) === request) asked.current.delete(key);
          if (live) setNotice(String(error));
        });
    }, 600);
    return () => {
      live = false;
      clearTimeout(timer);
      clearTimeout(retryTimer);
    };
  }, [
    scope,
    target?.status,
    connected,
    visible,
    settings,
    settingsReady,
    disclosed,
    turn,
    draft,
    retryVersion,
  ]);
  useEffect(() => {
    if (
      !connected ||
      !disclosed ||
      !settings?.summaries ||
      !settingsReady ||
      !target?.is_orchestrator
    )
      return;
    let live = true;
    const load = async () => {
      const value = latest.current;
      if (!value.visible || !value.settingsReady || current.current !== scope)
        return;
      const runs = routineRuns(value.events).filter(
        (run) => run.live && value.visibleRoutines.includes(run.thread),
      );
      for (const run of runs) {
        if (!live) return;
        const key = scope + ":" + run.thread;
        const worker = value.threads.find((thread) => thread.id === run.thread);
        const basis =
          run.events.at(-1)!.seq +
          ":" +
          (worker && ["running", "background"].includes(worker.status)
            ? worker.last_activity || ""
            : "");
        const prior = pace.current.get(key);
        const every = Math.max(60, settings.summary_every_secs || 300);
        if (
          inFlight.current.has(key) ||
          !summaryDue(prior, basis, every, Date.now())
        )
          continue;
        inFlight.current.add(key);
        const attempt = { at: Date.now(), next: every, basis: "" };
        pace.current.set(key, attempt);
        try {
          const retry = retrySummary.current;
          retrySummary.current = false;
          const answer = await window.twr.rpc("summary.thread", {
            thread: run.thread,
            models: settings.suggest_model,
            every_secs: every,
            retry,
          });
          if (!live || current.current !== scope) return;
          pace.current.set(
            key,
            settledSummary(attempt, basis, answer, target.id),
          );
          setStats({ ...answer.stats, source: "Summaries" });
          if (answer.covers_in === target.id && answer.covers != null)
            setSummaries((old) => [...old, answer].slice(-1000));
          if (answer.reason?.startsWith("unavailable:"))
            setNotice("Summaries unavailable: " + answer.reason.slice(12));
        } catch (error) {
          if (live) setNotice(String(error));
        } finally {
          inFlight.current.delete(key);
        }
      }
    };
    void load();
    const timer = setInterval(() => void load(), 5000);
    return () => {
      live = false;
      clearInterval(timer);
    };
  }, [
    scope,
    connected,
    disclosed,
    settings,
    settingsReady,
    target?.is_orchestrator,
  ]);
  const runs = useMemo(() => routineRuns(events), [events]);
  return {
    suggestion,
    summaries,
    settings,
    stats,
    notice,
    runs,
    needsDisclosure: !!enabled && !disclosed,
    acknowledge() {
      localStorage.setItem("twr.assistance.notice", disclosureKey);
      setAcknowledged(disclosureKey);
    },
    clearNotice() {
      setNotice("");
    },
    retry() {
      setNotice("");
      unavailableSuggest.current = false;
      asked.current.clear();
      pace.current.clear();
      retrySuggest.current = true;
      retrySummary.current = true;
      setRetryVersion((n) => n + 1);
    },
  };
}

export function AssistanceNotice({
  settings,
  acknowledge,
  disable,
}: {
  settings: Json;
  acknowledge: () => void;
  disable: () => void;
}) {
  return (
    <aside className="assistance-notice">
      <b>Small-model suggestions and summaries</b>
      <p>
        Suggestions send the completed turn’s prompt (2 KB) and reply ending (6
        KB). Summaries send a thread’s name, state, activity and routine updates
        (3 KB), never attention requests or your messages. They use{" "}
        {settings?.suggest_model?.["claude-code"]} through Claude Code
        (Anthropic) or {settings?.suggest_model?.codex} through Codex (OpenAI),
        with your login. Each call is about $0.0002 at list price and counts
        toward your plan. Summaries run at most once per{" "}
        {Math.round((settings?.summary_every_secs || 300) / 60)} minutes per
        thread. You can turn either off in Settings.
      </p>
      <button onClick={acknowledge}>Continue with configured features</button>
      <button onClick={disable}>Turn both off</button>
    </aside>
  );
}

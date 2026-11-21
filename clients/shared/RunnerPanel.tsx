import { useEffect, useRef, useState } from "react";
import type { Json } from "./types";
import { errorMessage } from "./errors";
import { CopyButton } from "./ui";
import "./runner-panel.css";

/** A command to run in a terminal on the other machine, copyable, after what it does. */
function CommandLine({ text, children }: { text: string; children: string }) {
  return (
    <div className="runner-command">
      <p>{children}</p>
      <div className="code-block">
        <pre>
          <code>{text}</code>
        </pre>
        <CopyButton
          className="copy-code"
          label={`Copy ${text}`}
          text={() => text}
        />
      </div>
    </div>
  );
}

type PairSession = { session: string; runner?: string };
function retireSession(
  rpc: (method: string, params?: Json) => Promise<Json>,
  pair: PairSession | null,
) {
  if (!pair) return;
  const call = pair.runner
    ? rpc("runner.manage", {
        name: pair.runner,
        admin: { action: "cancel", session: pair.session },
      })
    : rpc("desktop.runner.cancel", { session: pair.session });
  void call.catch(() => {});
}

export function RunnerPanel({
  rpc,
  refresh,
}: {
  rpc: (method: string, params?: Json) => Promise<Json>;
  refresh: () => void;
}) {
  const canHost = ["darwin", "linux"].includes(
    document.documentElement.dataset.platform || "",
  );
  const [mode, setMode] = useState<"closed" | "runner" | "connect" | "access">(
    "closed",
  );
  const [code, setCode] = useState("");
  const [name, setName] = useState("");
  const [expires, setExpires] = useState(0);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [paired, setPaired] = useState(false);
  const [replace, setReplace] = useState(false);
  const [selfMachine, setSelfMachine] = useState("");
  const [revokeTarget, setRevokeTarget] = useState("");
  const [hubs, setHubs] = useState<Json[]>([]);
  /** The hub's runner boxes (`hub.info` `boxes` over a link), for Manage runner access;
   * undefined until read, null when they can't be. */
  const [runners, setRunners] = useState<string[] | null>();
  const generation = useRef(0);
  const session = useRef<PairSession | null>(null);
  const rpcRef = useRef(rpc);
  rpcRef.current = rpc;
  const cancelSession = (pair: PairSession | null) =>
    retireSession(rpcRef.current, pair);
  useEffect(
    () => () => {
      generation.current++;
      const id = session.current;
      session.current = null;
      retireSession(rpcRef.current, id);
    },
    [],
  );
  useEffect(() => {
    if (!expires) return;
    const timer = window.setInterval(() => {
      if (Date.now() >= expires * 1000) {
        const id = session.current;
        session.current = null;
        retireSession(rpcRef.current, id);
        setCode("");
        setExpires(0);
        setError("This pairing code has expired. Start again for a new code.");
      }
    }, 1000);
    return () => window.clearInterval(timer);
  }, [expires]);
  const changeMode = (next: typeof mode) => {
    generation.current++;
    cancelSession(session.current);
    session.current = null;
    setMode(next);
    setCode("");
    setExpires(0);
    setError("");
    setPaired(false);
    setBusy(false);
    setHubs([]);
    setRevokeTarget("");
    setSelfMachine("");
    setReplace(false);
    if (next === "access") {
      const request = generation.current;
      setRunners(undefined);
      void rpcRef
        .current("hub.info")
        .then((info) => {
          if (generation.current !== request) return;
          const boxes = (Array.isArray(info?.boxes) ? info.boxes : []).filter(
            (b: Json) => typeof b?.name === "string",
          );
          const linked = boxes.filter((b: Json) => b.transport === "link");
          const names = (linked.length ? linked : boxes).map(
            (b: Json) => b.name as string,
          );
          setRunners(names.length ? names : null);
          if (names.length === 1) setName(names[0]);
        })
        .catch(() => {
          if (generation.current === request) setRunners(null);
        });
    }
    return generation.current;
  };
  const showRunner = async () => {
    const request = changeMode("runner");
    setBusy(true);
    try {
      const result = await rpc("desktop.runner.use");
      if (generation.current !== request) {
        cancelSession({ session: result.session });
        return;
      }
      session.current = { session: result.session };
      setCode(result.code);
      setExpires(result.expires_unix);
    } catch (e) {
      if (generation.current === request) setError(errorMessage(e));
    } finally {
      if (generation.current === request) setBusy(false);
    }
  };
  const connect = async (event: React.FormEvent) => {
    event.preventDefault();
    const request = generation.current;
    setBusy(true);
    setError("");
    try {
      await rpc("runner.connect", { name: name.trim(), code, replace });
      refresh();
      if (generation.current === request) {
        setCode("");
        setPaired(true);
      }
    } catch (e) {
      if (generation.current === request) setError(errorMessage(e));
    } finally {
      if (generation.current === request) setBusy(false);
    }
  };
  const access = async (action: string, machine?: string) => {
    const request = generation.current;
    setBusy(true);
    setError("");
    try {
      const result = await rpc("runner.manage", {
        name: name.trim(),
        admin: { action, ...(machine ? { machine } : {}) },
      });
      if (generation.current !== request) {
        if (result.session)
          retireSession(rpcRef.current, {
            session: result.session,
            runner: name.trim(),
          });
        return;
      }
      if (result.session)
        session.current = { session: result.session, runner: name.trim() };
      if (result.requesting_machine) setSelfMachine(result.requesting_machine);
      if (result.hubs) setHubs(result.hubs);
      if (result.code) {
        setCode(result.code);
        setExpires(result.expires_unix);
      }
      if (action === "revoke") {
        setHubs((current) =>
          current.map((hub) =>
            hub.machine === machine ? { ...hub, revoked: true } : hub,
          ),
        );
        setRevokeTarget("");
        if (machine === selfMachine)
          setError(
            "This hub’s access was revoked. Pair it again before managing this runner.",
          );
      }
    } catch (e) {
      if (generation.current === request) setError(errorMessage(e));
    } finally {
      if (generation.current === request) setBusy(false);
    }
  };
  return (
    <article className="resource-card">
      <h3>Runner pairing</h3>
      <p>
        Run agents on another machine using its existing agent logins. Paired
        hubs share that machine’s OS account and can manage its runner access.
      </p>
      <div className="card-actions">
        {canHost && (
          <button disabled={busy} onClick={() => void showRunner()}>
            Use this machine as a runner
          </button>
        )}
        <button disabled={busy} onClick={() => changeMode("connect")}>
          Connect a runner
        </button>
        <button disabled={busy} onClick={() => changeMode("access")}>
          Manage runner access
        </button>
      </div>
      {mode !== "closed" && (
        <section aria-label="Runner controls">
          {busy && (
            <p role="status">
              {mode === "runner"
                ? "Preparing a pairing code…"
                : "Contacting runner…"}
            </p>
          )}
          {error && <p role="alert">{error}</p>}
          {code && mode === "runner" && (
            <ol className="setup-steps runner-steps">
              <li>
                <h4>Give the other hub this code</h4>
                <p>
                  On the other hub machine, open Boxes, choose{" "}
                  <strong>Connect a runner</strong>, name this machine and
                  enter:
                </p>
                <p>
                  <code aria-label="Pairing code" className="runner-code">
                    {code}
                  </code>
                </p>
                <p>It works once and expires in 15 minutes.</p>
                <CommandLine text="twr box pair <name>">
                  Or, in a terminal on the other hub, run this with the name it
                  should use for this machine:
                </CommandLine>
              </li>
              <li>
                <h4>Wait for it to connect</h4>
                <p>
                  This machine connects once the other hub pairs. Agents keep
                  working when this window closes; this machine must stay awake.
                </p>
              </li>
            </ol>
          )}
          {code && mode === "access" && (
            <>
              <p>
                On the other hub machine, choose{" "}
                <strong>Connect a runner</strong> and enter:
              </p>
              <p>
                <code aria-label="Pairing code" className="runner-code">
                  {code}
                </code>
              </p>
              <p>This code works once and expires in 15 minutes.</p>
              <CommandLine text="twr box pair <name>">
                Or, in a terminal on the other hub:
              </CommandLine>
            </>
          )}
          {mode === "connect" && !paired && (
            <form onSubmit={connect} autoComplete="off">
              <ol className="setup-steps runner-steps">
                <li>
                  <h4>Name the box</h4>
                  <label>
                    Runner name
                    <input
                      value={name}
                      maxLength={64}
                      onChange={(e) => setName(e.target.value)}
                      required
                      disabled={busy}
                    />
                  </label>
                  <p>
                    On the runner machine, open Boxes and choose{" "}
                    <strong>Use this machine as a runner</strong> to get a code.
                  </p>
                  <CommandLine text="twr runner up">
                    Or, in a terminal on the runner machine:
                  </CommandLine>
                </li>
                <li>
                  <h4>Enter its code</h4>
                  <label>
                    Pairing code
                    <input
                      type="password"
                      autoComplete="off"
                      spellCheck={false}
                      value={code}
                      maxLength={64}
                      onChange={(e) => setCode(e.target.value)}
                      required
                      disabled={busy}
                    />
                  </label>
                  <button type="submit" className="primary" disabled={busy}>
                    Pair runner
                  </button>
                </li>
              </ol>
              <details className="runner-advanced">
                <summary>Advanced</summary>
                <label className="runner-replace">
                  <input
                    type="checkbox"
                    checked={replace}
                    disabled={busy}
                    onChange={(e) => setReplace(e.target.checked)}
                  />
                  Replace an existing pairing. Use it only when recovery is
                  unavailable: it stops the machine’s running workers.
                </label>
                <button
                  type="button"
                  disabled={busy || !name.trim()}
                  onClick={() => {
                    const request = generation.current;
                    setBusy(true);
                    setError("");
                    void rpc("runner.recover", { name: name.trim() })
                      .then(() => {
                        refresh();
                        if (generation.current === request) {
                          setCode("");
                          setPaired(true);
                        }
                      })
                      .catch((e) => {
                        if (generation.current === request)
                          setError(errorMessage(e));
                      })
                      .finally(() => {
                        if (generation.current === request) setBusy(false);
                      });
                  }}
                >
                  Recover a saved pairing
                </button>
              </details>
            </form>
          )}
          {mode === "access" && (
            <>
              <label>
                Connected runner
                {runners ? (
                  <select
                    value={name}
                    onChange={(e) => setName(e.target.value)}
                    disabled={busy}
                  >
                    <option value="">Choose…</option>
                    {runners.map((runner) => (
                      <option key={runner} value={runner}>
                        {runner}
                      </option>
                    ))}
                  </select>
                ) : (
                  <input
                    value={name}
                    maxLength={64}
                    onChange={(e) => setName(e.target.value)}
                    disabled={busy || runners === undefined}
                    placeholder={runners === undefined ? "Loading…" : ""}
                  />
                )}
              </label>
              <button
                disabled={busy || !name.trim()}
                onClick={() => void access("status")}
              >
                Show paired hubs
              </button>
              <button
                disabled={busy || !name.trim()}
                onClick={() => void access("invite")}
              >
                Pair another hub
              </button>
              <ul>
                {hubs.map((hub) => (
                  <li key={hub.machine}>
                    <code style={{ overflowWrap: "anywhere" }}>
                      {hub.machine}
                    </code>
                    {hub.machine === selfMachine ? " (this hub)" : ""}{" "}
                    {hub.revoked
                      ? "Revoked"
                      : hub.paired
                        ? "Paired"
                        : "Pending"}{" "}
                    {!hub.revoked && (
                      <button
                        disabled={busy}
                        onClick={() => setRevokeTarget(hub.machine)}
                      >
                        Revoke access
                      </button>
                    )}
                  </li>
                ))}
              </ul>
            </>
          )}
          {revokeTarget && (
            <div role="alertdialog" aria-label="Confirm runner revocation">
              <p>
                Revoke{" "}
                {revokeTarget === selfMachine ? "this hub’s" : revokeTarget}{" "}
                access? Its running workers will stop. Work is retained; a new
                pairing is required to connect again.
              </p>
              <button
                disabled={busy}
                onClick={() => void access("revoke", revokeTarget)}
              >
                Revoke access now
              </button>
              <button disabled={busy} onClick={() => setRevokeTarget("")}>
                Keep access
              </button>
            </div>
          )}
          {paired && (
            <p role="status">
              Runner paired. Its agents appear under Boxes once it connects.
            </p>
          )}
          <button onClick={() => changeMode("closed")}>Close</button>
        </section>
      )}
    </article>
  );
}

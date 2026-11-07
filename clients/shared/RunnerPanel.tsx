import { useEffect, useRef, useState } from "react";
import type { Json } from "./types";
import { errorMessage } from "./errors";

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
          {code && mode !== "connect" && (
            <>
              <p>
                On the other hub machine, choose{" "}
                <strong>Connect a runner</strong> and enter:
              </p>
              <p>
                <code aria-label="Pairing code">{code}</code>
              </p>
              <p>
                This code works once and expires in 15 minutes. From a terminal
                on the other hub, run <code>twr box pair &lt;name&gt;</code>.
              </p>
              <p>
                Agents keep working when this window closes. The runner machine
                must stay awake.
              </p>
            </>
          )}
          {mode === "connect" && !paired && (
            <form onSubmit={connect} autoComplete="off">
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
              <p>
                On the runner machine, choose Use this machine as a runner, or
                run <code>twr runner up</code>.
              </p>
              <label>
                <input
                  type="checkbox"
                  checked={replace}
                  disabled={busy}
                  onChange={(e) => setReplace(e.target.checked)}
                />
                Replace this machine’s existing access if recovery is
                unavailable. This stops its running workers.
              </label>
              <button type="submit" disabled={busy}>
                Pair runner
              </button>
            </form>
          )}
          {mode === "access" && (
            <>
              <label>
                Connected runner name
                <input
                  value={name}
                  maxLength={64}
                  onChange={(e) => setName(e.target.value)}
                  disabled={busy}
                />
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
          {mode === "connect" && !paired && name.trim() && (
            <button
              disabled={busy}
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
          )}
          {paired && <p role="status">Runner paired.</p>}
          <button onClick={() => changeMode("closed")}>Close</button>
        </section>
      )}
    </article>
  );
}

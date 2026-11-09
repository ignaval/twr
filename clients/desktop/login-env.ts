import { spawn } from "node:child_process";

const MARKER = "\0TWR_DESKTOP_ENV\0";
const SCRIPT = "printf '\\0TWR_DESKTOP_ENV\\0'; /usr/bin/env -0";

/**
 * Runs the user's login shell to print its environment. Its stdin is /dev/null,
 * so an rc file that prompts reads end-of-file instead of waiting for the
 * timeout. The shell gets its own process group, which is killed on timeout:
 * interactive shells ignore SIGTERM, and a background job of an rc file must
 * not keep the pipe open.
 */
export function runLoginShell(
  program: string,
  { timeout = 8000, maxBuffer = 4 * 1024 * 1024 } = {},
): Promise<string> {
  return new Promise((resolve, reject) => {
    const child = spawn(program, ["-ilc", SCRIPT], {
      stdio: ["ignore", "pipe", "ignore"],
      detached: true,
    });
    const chunks: Buffer[] = [];
    let size = 0;
    let settled = false;
    const finish = (error?: Error) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      if (error) {
        try {
          if (child.pid) process.kill(-child.pid, "SIGKILL");
        } catch {
          child.kill("SIGKILL");
        }
        reject(error);
      } else resolve(Buffer.concat(chunks).toString("utf8"));
    };
    const timer = setTimeout(
      () =>
        finish(
          new Error(`The login shell did not finish within ${timeout} ms`),
        ),
      timeout,
    );
    child.stdout!.on("data", (chunk: Buffer) => {
      size += chunk.length;
      if (size > maxBuffer)
        finish(new Error("The login shell printed too much output"));
      else chunks.push(chunk);
    });
    child.on("error", (error) => finish(error));
    child.on("close", (code, signal) =>
      code === 0
        ? finish()
        : finish(
            new Error(
              `The login shell exited with ${signal ? `signal ${signal}` : `code ${code}`}`,
            ),
          ),
    );
  });
}

/**
 * The variables to take from the login shell's output, or undefined when the
 * output has no environment. Identity and harness controls are never taken: an
 * agent-launched process must not turn itself into a user.
 */
export function parseLoginEnvironment(
  stdout: string,
  current: NodeJS.ProcessEnv,
): Record<string, string> | undefined {
  const start = stdout.indexOf(MARKER);
  if (start < 0) return undefined;
  const values: Record<string, string> = {};
  for (const entry of stdout.slice(start + MARKER.length).split("\0")) {
    const equals = entry.indexOf("=");
    if (equals <= 0) continue;
    const key = entry.slice(0, equals);
    if (
      /^(?:TWR_AGENT$|TWR_ORCH_TOKEN|TWR_TEST_USER_TOKEN_FILE$|TWR_DESKTOP_|ELECTRON_|NODE_OPTIONS$|NODE_PATH$)/.test(
        key,
      )
    )
      continue;
    if (
      ["TWR_HOME", "TWR_WORKSPACE"].includes(key) &&
      current[key] !== undefined
    )
      continue;
    values[key] = entry.slice(equals + 1);
  }
  return values;
}

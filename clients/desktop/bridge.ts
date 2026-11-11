import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import { createInterface } from "node:readline";
import type { DesktopEvent } from "../shared/types";
import { homedir } from "node:os";
export class Bridge {
  child: ChildProcessWithoutNullStreams;
  pending = new Map<
    number,
    {
      resolve: (value: any) => void;
      reject: (error: Error) => void;
      timer: ReturnType<typeof setTimeout>;
    }
  >();
  private next = 0;
  private closed = false;
  private restart?: ReturnType<typeof setTimeout>;
  private failures = 0;
  constructor(
    private binary: string,
    private workspace: string,
    private env: NodeJS.ProcessEnv,
    private emit: (event: DesktopEvent) => void,
  ) {
    this.child = this.start();
  }
  private start(): ChildProcessWithoutNullStreams {
    const { binary, workspace, env, emit } = this;
    const child = spawn(binary, [workspace], {
      env,
      cwd: homedir(),
      stdio: "pipe",
    });
    const lines = createInterface({ input: child.stdout });
    lines.on("line", (line) => {
      try {
        const message = JSON.parse(line);
        if (typeof message.id === "number") {
          const request = this.pending.get(message.id);
          if (!request) return;
          this.pending.delete(message.id);
          clearTimeout(request.timer);
          if (message.error)
            request.reject(
              Object.assign(new Error(message.error), { delivery: "rejected" }),
            );
          else request.resolve(message.result);
        } else {
          if (message.type === "connection" && message.connected)
            this.failures = 0;
          emit(message);
        }
      } catch {
        emit({
          type: "error",
          error: "The twr companion returned an invalid response",
        });
      }
    });
    let stderr = "";
    child.stderr.on("data", (data) => {
      stderr = (stderr + String(data)).slice(-8192);
    });
    let failed = false;
    const fail = (message: string) => {
      if (failed) return;
      failed = true;
      for (const request of this.pending.values()) {
        clearTimeout(request.timer);
        request.reject(new Error(message));
      }
      this.pending.clear();
      if (!this.closed) {
        emit({
          type: "connection",
          connected: false,
          error: message + " Reconnecting…",
        });
        this.restart = setTimeout(
          () => {
            this.restart = undefined;
            if (!this.closed) this.child = this.start();
          },
          Math.min(30000, 1000 * 2 ** Math.min(this.failures++, 5)),
        );
      }
    };
    child.on("error", (error) => fail(error.message));
    child.on("exit", (code) =>
      fail(`twr companion exited (${code ?? "signal"}). ${stderr}`),
    );
    child.stdin.on("error", (error) => {
      child.kill();
      fail(error.message);
    });
    return child;
  }
  call(method: string, params: any = {}, timeout = 660_000): Promise<any> {
    if (
      this.closed ||
      this.restart ||
      this.child.exitCode !== null ||
      this.child.signalCode !== null
    )
      return Promise.reject(
        Object.assign(new Error("Workspace connection is closed"), {
          delivery: "not-sent",
        }),
      );
    const id = ++this.next;
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(
          new Error(
            "No reply from the hub. The operation may have completed; check its state before retrying.",
          ),
        );
      }, timeout);
      this.pending.set(id, { resolve, reject, timer });
      this.child.stdin.write(
        JSON.stringify({ id, method, params }) + "\n",
        (error) => {
          if (error) {
            clearTimeout(timer);
            this.pending.delete(id);
            reject(error);
          }
        },
      );
    });
  }
  close() {
    this.closed = true;
    if (this.restart) clearTimeout(this.restart);
    for (const request of this.pending.values()) {
      clearTimeout(request.timer);
      request.reject(new Error("Workspace changed"));
    }
    this.pending.clear();
    this.child.stdin.end();
    const timer = setTimeout(() => this.child.kill(), 1500);
    timer.unref();
  }
}

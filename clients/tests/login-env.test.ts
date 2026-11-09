import { describe, expect, it } from "vitest";
import { chmodSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { parseLoginEnvironment, runLoginShell } from "../desktop/login-env";

function fakeShell(script: string) {
  const dir = mkdtempSync(join(tmpdir(), "twr-login-shell-"));
  const program = join(dir, "shell");
  writeFileSync(program, `#!/bin/sh\n${script}\n`);
  chmodSync(program, 0o755);
  return { program, done: () => rmSync(dir, { recursive: true, force: true }) };
}

describe("login environment", () => {
  it("takes the user's variables but never identity or harness controls", () => {
    const output =
      "motd noise\0TWR_DESKTOP_ENV\0" +
      [
        "PATH=/opt/tools/bin:/usr/bin",
        "EDITOR=vim",
        "MULTI=a=b",
        "TWR_AGENT=agent-1",
        "TWR_ORCH_TOKEN_FILE=/x",
        "TWR_DESKTOP_TEST_AUTOMATION=1",
        "ELECTRON_RUN_AS_NODE=1",
        "NODE_OPTIONS=--inspect",
        "TWR_HOME=/shell/home",
        "TWR_WORKSPACE=shell",
        "=broken",
      ].join("\0");
    expect(parseLoginEnvironment(output, { TWR_HOME: "/launch" })).toEqual({
      PATH: "/opt/tools/bin:/usr/bin",
      EDITOR: "vim",
      MULTI: "a=b",
      TWR_WORKSPACE: "shell",
    });
    expect(parseLoginEnvironment("no marker", {})).toBeUndefined();
  });

  it("gives a prompting rc file end-of-file instead of waiting", async () => {
    const shell = fakeShell(
      // An rc file that asks a question, then the real environment dump.
      'read answer; printf "\\0TWR_DESKTOP_ENV\\0ANSWER=[%s]\\0" "$answer"',
    );
    try {
      const started = Date.now();
      const output = await runLoginShell(shell.program, { timeout: 5000 });
      expect(Date.now() - started).toBeLessThan(4000);
      expect(parseLoginEnvironment(output, {})).toEqual({ ANSWER: "[]" });
    } finally {
      shell.done();
    }
  });

  it("reports a shell that hangs, even with a background job holding its output", async () => {
    const shell = fakeShell("sleep 30 & sleep 30");
    try {
      const started = Date.now();
      await expect(
        runLoginShell(shell.program, { timeout: 300 }),
      ).rejects.toThrow(/did not finish within 300 ms/);
      expect(Date.now() - started).toBeLessThan(2000);
    } finally {
      shell.done();
    }
  });

  it("reports a failing shell", async () => {
    const shell = fakeShell("exit 3");
    try {
      await expect(runLoginShell(shell.program)).rejects.toThrow(/code 3/);
    } finally {
      shell.done();
    }
  });
});

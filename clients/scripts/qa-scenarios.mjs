import assert from "node:assert/strict";
import { expect } from "@playwright/test";
import { mkdirSync, existsSync, readFileSync, realpathSync } from "node:fs";
import { join } from "node:path";

/** Broader QA against the actual packaged app. Only disposable hub data and fake agents. */
export async function runMacQA(page, { fixture, hubPids }) {
  const errors = [];
  page.on("pageerror", (error) => errors.push(error.message));
  const rpc = (method, params = {}) =>
    page.evaluate(({ method, params }) => window.twr.rpc(method, params), {
      method,
      params,
    });
  const nav = async (name) => {
    if (name === "Settings")
      return page
        .locator(".sidebar-footer")
        .getByRole("button", { name, exact: true })
        .click();
    const manage = page.locator(".manage-nav");
    if ((await manage.getAttribute("open")) === null)
      await manage.locator("summary").click();
    await manage.getByRole("button", { name, exact: true }).click();
  };
  const open = async (title, values = {}) => {
    await page
      .getByRole("button", { name: "Open command palette", exact: true })
      .click();
    await page.getByRole("textbox", { name: "Search commands" }).fill(title);
    await page
      .locator(".palette-list")
      .getByText(title, { exact: true })
      .click();
    const dialog = page.getByRole("dialog").last();
    for (const [key, value] of Object.entries(values)) {
      const field = dialog.locator("#field-" + key);
      if ((await field.getAttribute("type")) === "checkbox")
        await field.setChecked(value);
      else if (
        (await field.evaluate((element) => element.tagName)) === "SELECT"
      )
        await field.selectOption(value);
      else await field.fill(String(value));
    }
    return dialog;
  };
  const run = async (title, values = {}, confirm = false) => {
    const dialog = await open(title, values);
    await dialog.locator('button[type="submit"]').click();
    if (confirm)
      await page
        .getByRole("dialog")
        .last()
        .getByRole("button", { name: "Continue", exact: true })
        .click();
    await dialog.locator(".operation-result").waitFor();
    await dialog.getByRole("button", { name: "Close", exact: true }).click();
  };
  const talk = async (name = "packaged") => {
    await page
      .locator(".thread-list .thread-card")
      .filter({ hasText: name })
      .click();
    await page
      .getByRole("button", { name: "Talk to this thread", exact: true })
      .click();
  };

  // Open every manager on the release app, checking that the view renders successfully.
  for (const name of [
    "Accounts",
    "Boxes",
    "Jobs",
    "Usage",
    "Context & memory",
    "Read aloud",
    "Trash",
    "Hub & storage",
    "Settings",
    "Decisions",
  ]) {
    await nav(name);
    await page.locator(".content-column h1").waitFor();
    assert.equal(
      await page.locator(".manager .error-banner").count(),
      0,
      name + " should load",
    );
  }
  console.log("QA: all manager pages open");
  await page.keyboard.press("Meta+n");
  const keyboardNew = page.getByRole("dialog", {
    name: "New thread",
    exact: true,
  });
  await keyboardNew.waitFor();
  assert(
    (await keyboardNew.locator("#field-_cwd").inputValue()).startsWith("/"),
    "The native New menu uses the current working directory",
  );
  await keyboardNew
    .getByRole("button", { name: "Close dialog", exact: true })
    .click();
  const relative = await open("New thread", {
    project: "./repo",
    _cwd: fixture,
    name: "relative-path-qa",
    runtime: "fake",
  });
  await relative.locator('button[type="submit"]').click();
  await relative.waitFor({ state: "hidden" });
  assert.equal(
    (await rpc("thread.get", { thread: "relative-path-qa" })).repo,
    realpathSync(join(fixture, "repo")),
  );
  console.log("QA: native New shortcut and relative project directory");

  await run("Rename thread", { thread: "packaged", name: "qa-agent" });
  await run("Change permission mode", { thread: "qa-agent", mode: "plan" });
  await talk("qa-agent");
  assert.equal((await rpc("thread.get", { thread: "qa-agent" })).mode, "plan");

  await rpc("thread.send", {
    thread: "qa-agent",
    prompt: "!ask Continue with the QA test?|Proceed|Wait\nQUESTION_FINISHED",
  });
  await page
    .locator(".primary-nav")
    .getByRole("button", { name: /Needs you/ })
    .click();
  const question = page
    .locator(".manager .attention-card")
    .filter({ hasText: "Continue with the QA test?" });
  // Answers count once the options have been on screen for 600 ms (shared/arming.ts).
  await question.locator('.attention-options[data-armed="true"]').waitFor();
  await question.getByRole("button", { name: /Proceed/ }).click();
  await question.waitFor({ state: "hidden" });
  await talk("qa-agent");
  await page
    .locator(".block-assistant")
    .getByText("QUESTION_FINISHED", { exact: true })
    .waitFor();
  console.log("QA: rename, permission mode and question response");

  // Execute the TUI grammar through the desktop and inspect the concrete target/flags.
  const slash = async (line, title) => {
    const input = page.locator(".composer textarea");
    await input.fill(line);
    await input.press("Enter");
    const dialog = page.getByRole("dialog", { name: title, exact: true });
    await dialog.waitFor();
    return dialog;
  };
  const closeForm = (dialog) =>
    dialog.getByRole("button", { name: "Close dialog", exact: true }).click();
  let form = await slash(
    "/model --effort high",
    "Change model, runtime or account",
  );
  assert.equal(await form.locator("#field-effort").inputValue(), "high");
  assert.equal(await form.locator("#field-model").inputValue(), "");
  assert.equal(
    await form.locator("#field-thread").inputValue(),
    (await rpc("thread.get", { thread: "qa-agent" })).id,
  );
  await closeForm(form);
  form = await slash("/model qa-agent", "Current model, runtime and account");
  await closeForm(form);
  form = await slash("/account add claude-code work", "twr account add");
  assert.equal(
    await form.locator("#field-runtime").inputValue(),
    "claude-code",
  );
  assert.equal(await form.locator("#field-name").inputValue(), "work");
  await closeForm(form);
  form = await slash(
    "/account qa-agent work --outside-pin",
    "Change model, runtime or account",
  );
  assert.equal(await form.locator("#field-thread").inputValue(), "qa-agent");
  assert.equal(await form.locator("#field-account").inputValue(), "work");
  assert(await form.locator("#field-outside_pin").isChecked());
  await closeForm(form);
  form = await slash("/delete qa-agent second", "Move thread to trash");
  assert.equal(
    await form.locator("#field-threads").inputValue(),
    '"qa-agent" "second"',
  );
  await closeForm(form);
  form = await slash(
    "/new demo --outside-pin --runtime fake --account work --name from-slash",
    "New thread",
  );
  assert(await form.locator("#field-outside_pin").isChecked());
  assert.equal(await form.locator("#field-runtime").inputValue(), "fake");
  await closeForm(form);
  form = await slash("/ws non-existent-workspace", "Workspaces");
  assert.equal(
    (await page.evaluate(() => window.twr.bootstrap())).workspace,
    "default",
  );
  await closeForm(form);
  form = await slash("/read this", "Read messages aloud");
  assert(Number(await form.locator("#field-seq").inputValue()) > 0);
  await closeForm(form);
  form = await open("twr usage", { since: "7d" });
  await form
    .getByRole("button", { name: "Open terminal", exact: true })
    .click();
  await form.locator(".error-banner").waitFor();
  assert.equal(await form.locator("#field-since").inputValue(), "7d");
  await closeForm(form);
  console.log(
    "QA: slash targets, multi-delete, account creation, workspace typo safety and CLI preflight",
  );

  // Queued messages must be removable and their exact text restored to the composer.
  await rpc("thread.send", {
    thread: "qa-agent",
    prompt: "!sleep 5000\nQUEUE_FINISHED",
  });
  await rpc("thread.send", {
    thread: "qa-agent",
    prompt: "QUEUED_TEXT_TO_RESTORE",
  });
  const queued = page
    .locator(".queued-message")
    .filter({ hasText: "QUEUED_TEXT_TO_RESTORE" });
  await queued
    .getByRole("button", { name: "Take back", exact: true })
    .waitFor();
  await page.locator(".composer textarea").fill("/unsend");
  await page.locator(".composer textarea").press("Enter");
  const composer = page.getByRole("textbox", {
    name: "Message qa-agent directly",
  });
  await composer.waitFor();
  await page.waitForFunction(
    () =>
      document.querySelector(".composer textarea")?.value ===
      "QUEUED_TEXT_TO_RESTORE",
  );
  await composer.fill("");
  await page
    .locator(".block-assistant")
    .getByText("QUEUE_FINISHED", { exact: true })
    .waitFor();
  console.log("QA: queue take-back restores the original message");

  await nav("Jobs");
  await run("Run a job", {
    thread: "qa-agent",
    argv: "sh -c 'printf DESKTOP_JOB_OK; sleep 60'",
    name: "qa-job",
    wake: false,
  });
  const jobRow = page.getByRole("row").filter({ hasText: "qa-job" });
  await jobRow.waitFor();
  await jobRow.getByRole("button", { name: "Output", exact: true }).click();
  await page.getByText("DESKTOP_JOB_OK", { exact: true }).waitFor();
  const job = (await rpc("job.list")).find((value) => value.name === "qa-job");
  await run("Cancel job", { job: job.job }, true);
  console.log("QA: job launch, output and cancellation");

  await nav("Context & memory");
  await run("Write memory note", {
    name: "qa-note",
    type: "project",
    description: "QA note",
    body: "Original body",
  });
  const note = page.locator(".resource-card").filter({ hasText: "qa-note" });
  await note.getByRole("button", { name: "Read / edit", exact: true }).click();
  await page
    .getByLabel("Note", { exact: true })
    .fill("Unsaved body that must survive navigation");
  await nav("Jobs");
  await nav("Context & memory");
  await note.getByRole("button", { name: "Read / edit", exact: true }).click();
  assert.equal(
    await page.getByLabel("Note", { exact: true }).inputValue(),
    "Unsaved body that must survive navigation",
  );
  await page.getByRole("button", { name: "Save note", exact: true }).click();
  await page.locator(".editor-form").waitFor({ state: "hidden" });
  assert.equal(
    (await rpc("memory.read", { name: "qa-note" })).body,
    "Unsaved body that must survive navigation",
  );
  console.log("QA: memory creation, editing and draft retention");
  const context = await open("Read standing context", { project: "demo" });
  await context.locator('button[type="submit"]').click();
  await context
    .locator(".editor-form textarea")
    .fill("Standing context saved by packaged Mac QA");
  await context
    .getByRole("button", { name: "Save context", exact: true })
    .click();
  await context.getByText("Context saved.", { exact: true }).waitFor();
  assert.equal(
    (await rpc("context.read", { project: "demo" })).content,
    "Standing context saved by packaged Mac QA",
  );
  await context.getByRole("button", { name: "Close", exact: true }).click();
  console.log("QA: standing context edit and revision save");

  await run("Park thread", { thread: "qa-agent", keep: true }, true);
  assert.equal((await rpc("thread.get", { thread: "qa-agent" })).parked, true);
  await run("Resume thread", { thread: "qa-agent" });
  await talk("qa-agent");
  await composer.fill("WORKSPACE_DRAFT");
  const made = await page.evaluate(() =>
    window.twr.cli(["ws", "new", "qa-space"]),
  );
  assert.equal(made.code, 0, made.stderr);
  // Reconnect through the actual selector after refreshing its available names.
  await page.reload();
  await page
    .locator(".connection-status")
    .filter({ hasText: "Hub connected" })
    .waitFor();
  await page
    .locator(
      '.workspace-entry[data-workspace="qa-space"] .workspace-orchestrator',
    )
    .click();
  await expect
    .poll(
      () => page.evaluate(async () => (await window.twr.bootstrap()).workspace),
      { timeout: 30000 },
    )
    .toBe("qa-space");
  await page
    .locator(".connection-status")
    .filter({ hasText: "Hub connected" })
    .waitFor();
  hubPids.add((await rpc("hub.info")).pid);
  await page
    .locator(
      '.workspace-entry[data-workspace="default"] .workspace-orchestrator',
    )
    .click();
  await expect
    .poll(
      () => page.evaluate(async () => (await window.twr.bootstrap()).workspace),
      { timeout: 30000 },
    )
    .toBe("default");
  await page
    .locator(".connection-status")
    .filter({ hasText: "Hub connected" })
    .waitFor();
  await talk("qa-agent");
  // Renderer reload intentionally starts fresh chat drafts; switching workspaces must not.
  await composer.fill("PRESERVE_DURING_SWITCH");
  await page
    .locator(
      '.workspace-entry[data-workspace="qa-space"] .workspace-orchestrator',
    )
    .click();
  await page
    .locator(".connection-status")
    .filter({ hasText: "Hub connected" })
    .waitFor();
  await page
    .locator(
      '.workspace-entry[data-workspace="default"] .workspace-orchestrator',
    )
    .click();
  await page
    .locator(".connection-status")
    .filter({ hasText: "Hub connected" })
    .waitFor();
  await talk("qa-agent");
  assert.equal(await composer.inputValue(), "PRESERVE_DURING_SWITCH");
  await composer.fill("");
  console.log("QA: park/resume and workspace switching with drafts");

  const deletion = await open("Move thread to trash", { threads: "qa-agent" });
  await deletion.locator('button[type="submit"]').click();
  const deleteCard = page
    .locator(".manager .attention-card")
    .filter({ hasText: "qa-agent" });
  await deleteCard.locator('.attention-options[data-armed="true"]').waitFor();
  await deleteCard
    .locator(".attention-options button")
    .filter({ hasText: /^\s*(?:\d+\s*)?delete/i })
    .click();
  await deleteCard.waitFor({ state: "hidden" });
  await nav("Trash");
  const trashRow = page.getByRole("row").filter({ hasText: "qa-agent" });
  await trashRow.getByRole("button", { name: "Restore", exact: true }).click();
  const restoration = page.getByRole("dialog").last();
  await restoration.locator('button[type="submit"]').click();
  await restoration.locator(".operation-result").waitFor();
  await restoration.getByRole("button", { name: "Close", exact: true }).click();
  await run("Resume thread", { thread: "qa-agent" });
  await talk("qa-agent");
  console.log("QA: hub-confirmed deletion and trash restoration");

  await rpc("say", { message: "QA_ORCHESTRATOR_SEED", to: "orchestrator" });
  await page
    .locator(".workspace-entry.current .workspace-orchestrator")
    .click();
  const orchestrator = page.getByRole("textbox", {
    name: "Message orchestrator",
    exact: true,
  });
  await orchestrator.fill("QA_ORCHESTRATOR_FROM_UI");
  await page.getByRole("button", { name: "Send message", exact: true }).click();
  await page
    .locator(".block-assistant")
    .getByText("QA_ORCHESTRATOR_FROM_UI", { exact: true })
    .waitFor();
  const workerBefore = (
    await rpc("thread.transcript", {
      thread: "qa-agent",
      since_seq: 0,
      limit: 500,
    })
  ).events.length;
  const viewport = await page.evaluate(() => ({
    width: innerWidth,
    height: innerHeight,
  }));
  for (const width of [1000, 1360]) {
    await page.setViewportSize({ width, height: 800 });
    await page
      .locator(".thread-list .thread-card")
      .filter({ hasText: "qa-agent" })
      .click();
    assert(
      await orchestrator.isVisible(),
      "Selecting a thread must keep the orchestrator destination",
    );
    // On smaller displays details overlay the composer. Dismiss them through
    // the same control a user uses before returning to the conversation.
    await page
      .getByRole("button", { name: "Close thread details", exact: true })
      .click();
    const message = `QA_STILL_TO_ORCHESTRATOR_${width}`;
    await orchestrator.fill(message);
    await page
      .getByRole("button", { name: "Send message", exact: true })
      .click();
    await page
      .locator(".block-assistant")
      .getByText(message, { exact: true })
      .waitFor();
  }
  await page.setViewportSize(viewport);
  assert.equal(
    (
      await rpc("thread.transcript", {
        thread: "qa-agent",
        since_seq: 0,
        limit: 500,
      })
    ).events.length,
    workerBefore,
  );
  console.log(
    "QA: orchestrator sending and destination isolation at 1000px and 1360px",
  );

  const speakerForm = await open("twr speak", {
    name: "qa-speaker",
    engine: "command",
  });
  await speakerForm
    .getByRole("button", { name: "Open terminal", exact: true })
    .click();
  await speakerForm.waitFor({ state: "hidden" });
  await expect
    .poll(
      () =>
        page.evaluate(
          async () => (await window.twr.rpc("speech.status")).speakers.length,
        ),
      { timeout: 30000 },
    )
    .toBeGreaterThan(0);
  await page
    .locator(".block-assistant")
    .filter({ hasText: "QA_STILL_TO_ORCHESTRATOR_1360" })
    .getByRole("button", { name: "Read message aloud", exact: true })
    .click();
  const spoken = join(fixture, "spoken.txt");
  for (let attempt = 0; attempt < 100; attempt++) {
    if (
      existsSync(spoken) &&
      readFileSync(spoken, "utf8").includes("QA_STILL_TO_ORCHESTRATOR_1360")
    )
      break;
    if (attempt === 99)
      throw new Error("Selected message did not reach the speaker command");
    await new Promise((resolve) => setTimeout(resolve, 100));
  }
  assert(
    !readFileSync(spoken, "utf8").includes("QA_STILL_TO_ORCHESTRATOR_1000"),
    "Reading one selected message must not read the other viewport's message",
  );
  await page
    .getByRole("button", { name: "Close twr speak", exact: true })
    .click();
  await page
    .getByRole("dialog")
    .last()
    .getByRole("button", { name: "Continue", exact: true })
    .click();
  console.log(
    "QA: selecting one message reaches a real speaker process (silent command engine)",
  );

  await orchestrator.fill("/ws new slash-created");
  await orchestrator.evaluate((element) => {
    for (let index = 0; index < 2; index++)
      element.dispatchEvent(
        new KeyboardEvent("keydown", {
          key: "Enter",
          bubbles: true,
          cancelable: true,
        }),
      );
  });
  await expect
    .poll(
      () => page.evaluate(async () => (await window.twr.bootstrap()).workspace),
      { timeout: 30000 },
    )
    .toBe("slash-created");
  await page
    .locator(".connection-status")
    .filter({ hasText: "Hub connected" })
    .waitFor();
  hubPids.add((await rpc("hub.info")).pid);
  assert(
    await page.locator(".composer textarea").isEnabled(),
    "Slash commands must work before choosing an orchestrator",
  );
  await page.locator(".composer textarea").fill("/workspace default");
  // Hub-connected arrives before the first slash command finishes refreshing
  // workspace data. Text stays editable while Send indicates the pending work.
  await expect(
    page.getByRole("button", { name: "Send message", exact: true }),
  ).toBeEnabled({ timeout: 30000 });
  await page.locator(".composer textarea").press("Enter");
  await expect
    .poll(
      () => page.evaluate(async () => (await window.twr.bootstrap()).workspace),
      { timeout: 30000 },
    )
    .toBe("default");
  await page
    .locator(".connection-status")
    .filter({ hasText: "Hub connected" })
    .waitFor();
  console.log(
    "QA: slash workspace creation and switching from an empty workspace",
  );
  await page
    .locator('.workspace-entry.current[data-workspace="default"]')
    .waitFor();

  // Reserve terminal slots before asynchronous CLI planning/validation.
  const capacity = await page.evaluate(async () => {
    const shells = [];
    try {
      for (let n = 0; n < 7; n++)
        shells.push(
          await window.twr.terminalOpen({ shell: true, title: "Capacity QA" }),
        );
      const attempts = await Promise.allSettled(
        Array.from({ length: 8 }, () =>
          window.twr.terminalOpen({
            args: ["info", "--json"],
            title: "Concurrent capacity QA",
          }),
        ),
      );
      const opened = attempts.flatMap((result) =>
        result.status === "fulfilled" ? [result.value] : [],
      );
      const rejected = attempts.flatMap((result) =>
        result.status === "rejected" ? [String(result.reason)] : [],
      );
      for (const session of opened) await window.twr.terminalClose(session.id);
      return { opened: opened.length, rejected };
    } finally {
      for (const session of shells) await window.twr.terminalClose(session.id);
    }
  });
  assert.equal(capacity.opened, 1, JSON.stringify(capacity));
  assert.equal(capacity.rejected.length, 7);
  assert(capacity.rejected.every((reason) => reason.includes("maximum 8")));
  console.log("QA: concurrent CLI validation respects the eight-terminal cap");

  const completed = [];
  for (let index = 0; index < 9; index++) {
    const session = await page.evaluate(() =>
      window.twr.terminalOpen({
        args: ["info", "--json"],
        title: "Finished QA command",
      }),
    );
    completed.push(session.id);
    for (let attempt = 0; attempt < 100; attempt++) {
      const terminal = (
        await page.evaluate(() => window.twr.bootstrap())
      ).terminals.find((t) => t.id === session.id);
      if (terminal?.exited) break;
      assert(
        attempt < 99,
        "CLI process should exit before starting the next workflow",
      );
      await new Promise((resolve) => setTimeout(resolve, 100));
    }
  }
  for (const id of completed)
    await page.evaluate((id) => window.twr.terminalClose(id), id);
  console.log(
    "QA: more than eight completed CLI workflows do not block new terminals",
  );

  mkdirSync("test-results", { recursive: true });
  await page.screenshot({ path: "test-results/mac-qa-conversation.png" });
  assert.equal(errors.length, 0, errors.join("\n"));
  assert.equal(
    await page.locator(".toast[role=alert]").count(),
    0,
    "No unexpected errors during QA",
  );
}

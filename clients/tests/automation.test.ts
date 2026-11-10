import { describe, expect, it } from "vitest";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { QA_MARKER, testAutomation } from "../desktop/automation";

describe("test automation", () => {
  const on = { TWR_DESKTOP_TEST_AUTOMATION: "1" };
  const marker = `/app/resources/${QA_MARKER}`;
  const exists = (present: string[]) => (path: string) =>
    present.includes(path);

  it("is honoured by a development run on the variable alone", () => {
    expect(
      testAutomation({
        packaged: false,
        env: on,
        resourcesPath: "/app/resources",
        exists: exists([]),
      }),
    ).toBe(true);
  });

  it("is ignored by a packaged build without the QA marker", () => {
    expect(
      testAutomation({
        packaged: true,
        env: on,
        resourcesPath: "/app/resources",
        exists: exists([]),
      }),
    ).toBe(false);
    // The marker has to be in this build's own resources, not anywhere else.
    expect(
      testAutomation({
        packaged: true,
        env: on,
        resourcesPath: "/app/resources",
        exists: exists([`/elsewhere/${QA_MARKER}`]),
      }),
    ).toBe(false);
  });

  it("is honoured by a packaged QA build that carries the marker", () => {
    expect(
      testAutomation({
        packaged: true,
        env: on,
        resourcesPath: "/app/resources",
        exists: exists([marker]),
      }),
    ).toBe(true);
  });

  it("is the marker the QA scripts and the release checks name", () => {
    for (const path of [
      "scripts/packaged-smoke.mjs",
      "scripts/linux-runtime-qa.sh",
      "../.github/workflows/desktop.yml",
    ])
      expect(readFileSync(join(__dirname, "..", path), "utf8"), path).toContain(
        QA_MARKER,
      );
  });

  it("needs the variable even in a QA build", () => {
    for (const value of [undefined, "", "0", "true"])
      expect(
        testAutomation({
          packaged: true,
          env: { TWR_DESKTOP_TEST_AUTOMATION: value },
          resourcesPath: "/app/resources",
          exists: exists([marker]),
        }),
      ).toBe(false);
  });
});

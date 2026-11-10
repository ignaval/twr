import { existsSync } from "node:fs";
import { join } from "node:path";

/**
 * The file in the app's resources directory that lets a packaged build honour
 * TWR_DESKTOP_TEST_AUTOMATION (remote debugging, the fake microphone, recorded
 * notifications). Release packaging never writes it: the QA scripts add it to a
 * copy of the app (scripts/packaged-smoke.mjs) or, as root, to a test install
 * (desktop.yml), and the release jobs fail if a package carries it.
 */
export const QA_MARKER = "twr-qa-automation";

/**
 * Whether this run is driven by the test suites. A development run honours the
 * variable alone. A packaged build also needs the QA marker inside its own,
 * signed bundle, so that a process launching the installed app with the
 * variable set cannot reopen remote debugging (docs/desktop.md).
 */
export function testAutomation(input: {
  packaged: boolean;
  env: NodeJS.ProcessEnv;
  resourcesPath: string;
  exists?: (path: string) => boolean;
}): boolean {
  if (input.env.TWR_DESKTOP_TEST_AUTOMATION !== "1") return false;
  if (!input.packaged) return true;
  return (input.exists ?? existsSync)(join(input.resourcesPath, QA_MARKER));
}

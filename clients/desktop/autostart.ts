import {
  existsSync,
  mkdirSync,
  readFileSync,
  renameSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { dirname, join } from "node:path";
import { randomUUID } from "node:crypto";

/** The argument the login entry passes so a login launch can start in the background. */
export const AUTOSTART_ARG = "--autostart";
const MARKER = "X-twr-Autostart=true";

/** `$XDG_CONFIG_HOME/autostart/tools.twr.desktop.desktop` (freedesktop autostart spec). */
export function autostartPath(env: NodeJS.ProcessEnv, home: string) {
  return join(
    env.XDG_CONFIG_HOME || join(home, ".config"),
    "autostart",
    "tools.twr.desktop.desktop",
  );
}

/**
 * The executable a login session should start. An AppImage must be started
 * through its image (the mounted path disappears after exit); packages are
 * started through their stable launcher.
 */
export function linuxLaunchCommand(
  env: NodeJS.ProcessEnv,
  execPath: string,
  exists: (path: string) => boolean = existsSync,
) {
  if (env.APPIMAGE) return env.APPIMAGE;
  for (const launcher of ["/usr/bin/twr-desktop"])
    if (exists(launcher)) return launcher;
  return execPath;
}

/** Quote one Exec argument per the Desktop Entry specification. */
export function desktopExecArgument(value: string) {
  if (/[\0\n\r]/.test(value)) throw new Error("Unsupported launch path");
  const quoted = value.replace(/(["`$\\])/g, "\\$1");
  // Exec is also a `string` value, whose own escaping (`\\`) is undone before
  // the quoting rule; `%` introduces field codes and must be doubled.
  return `"${quoted}"`.replaceAll("\\", "\\\\").replaceAll("%", "%%");
}

export function autostartEntry(command: string) {
  return [
    "[Desktop Entry]",
    "Type=Application",
    "Name=twr",
    "Comment=Start twr when you log in",
    `Exec=${desktopExecArgument(command)} ${AUTOSTART_ARG}`,
    "Icon=tools.twr.desktop",
    "Terminal=false",
    "StartupWMClass=tools.twr.desktop",
    "X-GNOME-Autostart-enabled=true",
    MARKER,
    "",
  ].join("\n");
}

/** Only entries this app wrote count; a user's own entry is left alone. */
export function linuxAutostartEnabled(path: string) {
  try {
    const text = readFileSync(path, "utf8");
    return (
      text.includes(MARKER) &&
      !/^Hidden\s*=\s*true\s*$/m.test(text) &&
      !/^X-GNOME-Autostart-enabled\s*=\s*false\s*$/m.test(text)
    );
  } catch {
    return false;
  }
}

export function setLinuxAutostart(
  path: string,
  enabled: boolean,
  command: string,
) {
  if (!enabled) {
    if (existsSync(path)) {
      let text = "";
      try {
        text = readFileSync(path, "utf8");
      } catch {}
      if (!text.includes(MARKER))
        throw new Error(
          `${path} was not created by twr. Remove it yourself to stop launching at login.`,
        );
      rmSync(path, { force: true });
    }
    return;
  }
  mkdirSync(dirname(path), { recursive: true });
  const temporary = `${path}.${randomUUID()}.tmp`;
  writeFileSync(temporary, autostartEntry(command), { mode: 0o644 });
  renameSync(temporary, path);
}

/**
 * Rewrites an enabled twr entry whose command differs from `command` (an
 * AppImage that was moved or renamed by an update). Returns true if changed.
 */
export function refreshLinuxAutostart(path: string, command: string) {
  if (!linuxAutostartEnabled(path)) return false;
  const expected = autostartEntry(command);
  if (readFileSync(path, "utf8") === expected) return false;
  setLinuxAutostart(path, true, command);
  return true;
}

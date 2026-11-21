// The desktop QA scripts drive the app's wide layout. In a real Hyprland session (Omarchy)
// the compositor tiles a new window beside the windows already on the workspace, so on a
// laptop screen the app can get half of it (about 700 px) and show its phone layout, where
// the connection status the scripts wait for is hidden and the sidebar is a tab bar.
// floatTestWindow makes the app's window float at a fixed size in the middle of its monitor,
// as it would be alone on a workspace. Outside Hyprland it does nothing. A window hidden and
// shown again is a new one to Hyprland, tiled again: float it again after showing it.
import { execFileSync } from "node:child_process";
import { readFileSync } from "node:fs";

const hyprctl = (...args) =>
  execFileSync("hyprctl", args, { encoding: "utf8" }).trim();
const json = (what) => JSON.parse(hyprctl(what, "-j"));
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

/** Whether `pid` is `ancestor` or one of its descendants (a packaged app's launcher). */
function descends(pid, ancestor) {
  for (let n = 0; pid > 1 && n < 16; n++) {
    if (pid === ancestor) return true;
    try {
      // The parent is the 4th field after the parenthesised command, which may hold spaces.
      const stat = readFileSync(`/proc/${pid}/stat`, "utf8");
      pid = Number(stat.slice(stat.lastIndexOf(")") + 2).split(" ")[1]);
    } catch {
      return false;
    }
  }
  return false;
}

/** Floats the window of the app started as `pid` at width×height (logical pixels, shrunk to
 * fit its monitor) and centres it. Waits up to 15 s for the window to map. */
export async function floatTestWindow(
  pid,
  { width = 1280, height = 800 } = {},
) {
  if (process.platform !== "linux" || !process.env.HYPRLAND_INSTANCE_SIGNATURE)
    return;
  let client;
  for (let n = 0; n < 150; n++) {
    client = json("clients").find(
      (value) => value.mapped && !value.hidden && descends(value.pid, pid),
    );
    if (client) break;
    await sleep(100);
  }
  if (!client) throw new Error(`Hyprland never mapped a window for pid ${pid}`);
  const monitor = json("monitors").find((value) => value.id === client.monitor);
  if (monitor) {
    const rotated = monitor.transform % 2 === 1;
    const [reservedLeft, reservedTop, reservedRight, reservedBottom] =
      monitor.reserved ?? [0, 0, 0, 0];
    const available = [
      (rotated ? monitor.height : monitor.width) / monitor.scale -
        reservedLeft -
        reservedRight,
      (rotated ? monitor.width : monitor.height) / monitor.scale -
        reservedTop -
        reservedBottom,
    ];
    width = Math.min(width, Math.floor(available[0] - 40));
    height = Math.min(height, Math.floor(available[1] - 40));
  }
  const window = `address:${client.address}`;
  // Hyprland 0.56 and later take Lua expressions; older releases take dispatcher names.
  const lua = [
    `hl.dsp.window.float({ action = "on", window = "${window}" })`,
    `hl.dsp.window.resize({ x = ${width}, y = ${height}, window = "${window}" })`,
    `hl.dsp.window.center({ window = "${window}" })`,
  ];
  if (hyprctl("dispatch", lua[0]) === "ok")
    for (const expression of lua.slice(1)) hyprctl("dispatch", expression);
  else {
    hyprctl("dispatch", "setfloating", window);
    hyprctl(
      "dispatch",
      "resizewindowpixel",
      `exact ${width} ${height},${window}`,
    );
    hyprctl("dispatch", "focuswindow", window);
    hyprctl("dispatch", "centerwindow");
  }
  for (let n = 0; n < 50; n++) {
    const now = json("clients").find(
      (value) => value.address === client.address,
    );
    if (now?.floating && now.size[0] === width && now.size[1] === height)
      return;
    await sleep(100);
  }
  throw new Error(
    `Hyprland did not float the app's window at ${width}x${height}`,
  );
}

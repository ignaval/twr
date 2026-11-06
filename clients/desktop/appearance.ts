import type { DesktopAppearance } from "../shared/types";

// The renderer chooses presentation; only the trusted main-frame bridge may
// update native chrome. Theme/layout mutations otherwise stay in the renderer.
export function observeDesktopAppearance() {
  const setAppearance = window.twr.setAppearance;
  if (!setAppearance) return () => {};
  const root = document.documentElement;
  const scheme = matchMedia("(prefers-color-scheme: dark)");
  const canvas = document.createElement("canvas");
  canvas.width = canvas.height = 1;
  const context = canvas.getContext("2d", { willReadFrequently: true });
  let pendingFrame: number | undefined;
  let desired: DesktopAppearance;
  let applied = "";
  let applying = false;
  let disposed = false;
  const signature = (value: DesktopAppearance) =>
    `${value.preference}/${value.colorScheme}/${value.background}`;
  const synchronize = async () => {
    if (applying || disposed) return;
    applying = true;
    try {
      while (!disposed && signature(desired) !== applied) {
        const value = desired;
        await setAppearance(value);
        applied = signature(value);
      }
    } catch (error) {
      console.warn("Could not update native app appearance", error);
    } finally {
      applying = false;
    }
  };
  const update = () => {
    pendingFrame = undefined;
    const colors = getComputedStyle(root);
    const resolved = colors.colorScheme.trim();
    const colorScheme =
      resolved === "dark" || (resolved !== "light" && scheme.matches)
        ? "dark"
        : "light";
    const preference =
      root.dataset.theme === "light" || root.dataset.theme === "system"
        ? root.dataset.theme
        : "dark";
    let background = colors.getPropertyValue("--bg").trim().toLowerCase();
    if (!/^#[0-9a-f]{6}$/.test(background)) {
      if (!context) return;
      context.clearRect(0, 0, 1, 1);
      context.fillStyle = colors.backgroundColor;
      context.fillRect(0, 0, 1, 1);
      background =
        "#" +
        [...context.getImageData(0, 0, 1, 1).data]
          .slice(0, 3)
          .map((value) => value.toString(16).padStart(2, "0"))
          .join("");
    }
    desired = { preference, colorScheme, background };
    if (signature(desired) !== applied) void synchronize();
  };
  const schedule = () => {
    if (pendingFrame === undefined)
      pendingFrame = requestAnimationFrame(update);
  };
  const observer = new MutationObserver(schedule);
  observer.observe(root, {
    attributes: true,
    attributeFilter: ["data-theme", "data-omarchy"],
  });
  scheme.addEventListener("change", schedule);
  update();
  return () => {
    disposed = true;
    observer.disconnect();
    scheme.removeEventListener("change", schedule);
    if (pendingFrame !== undefined) cancelAnimationFrame(pendingFrame);
  };
}

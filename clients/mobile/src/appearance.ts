import { Capacitor, SystemBars, SystemBarsStyle } from "@capacitor/core";
import { Layout } from "./layout";
type NativeAppearance = { style: SystemBarsStyle; background: string };
/** How often, and how many times, the first sync looks for the stylesheet (5 s in all). */
const STYLE_WAIT_MS = 50;
const STYLE_WAIT_TRIES = 100;

// Match system-bar contrast to the rendered theme, including an explicit dark
// theme on a light OS. Layout mutations must not send native bridge commands.
export function observeNativeAppearance() {
  if (!Capacitor.isNativePlatform()) return () => {};
  const root = document.documentElement;
  const scheme = matchMedia("(prefers-color-scheme: dark)");
  const lightScheme = matchMedia("(prefers-color-scheme: light)");
  let pendingFrame: number | undefined;
  let settledUpdate: ReturnType<typeof setTimeout> | undefined;
  let desired: NativeAppearance;
  let applied = "";
  const colorCanvas = document.createElement("canvas");
  colorCanvas.width = colorCanvas.height = 1;
  const colorContext = colorCanvas.getContext("2d", {
    willReadFrequently: true,
  });
  const signature = (value: NativeAppearance) =>
    `${value.style}/${value.background}`;
  const backgroundColor = (colors: CSSStyleDeclaration) => {
    const value = colors.getPropertyValue("--bg").trim();
    if (/^#[0-9a-f]{6}$/i.test(value)) return value.toLowerCase();
    if (!colorContext)
      throw new Error("Cannot resolve the app background color");
    colorContext.clearRect(0, 0, 1, 1);
    colorContext.fillStyle = colors.backgroundColor;
    colorContext.fillRect(0, 0, 1, 1);
    return (
      "#" +
      [...colorContext.getImageData(0, 0, 1, 1).data]
        .slice(0, 3)
        .map((value) => value.toString(16).padStart(2, "0"))
        .join("")
    );
  };
  let applying = false;
  let disposed = false;
  const synchronize = async () => {
    if (applying || disposed) return;
    applying = true;
    try {
      // Serialize rapid theme changes so an older native reply cannot win.
      while (!disposed && signature(desired) !== applied) {
        const appearance = desired;
        await SystemBars.setStyle({ style: appearance.style });
        // Android's core bar API resets decor to the OS window background.
        // Apply the web theme afterwards, including the WebView's unpainted area.
        await Layout.setAppearance({ background: appearance.background });
        applied = signature(appearance);
      }
    } catch (error) {
      console.warn("Could not update system-bar appearance", error);
    } finally {
      applying = false;
    }
  };
  // The theme's colors come from the stylesheet. On a slow start this module can run before
  // it applies: `--bg` is empty then, and the computed colors are a bare page's (transparent,
  // the OS's scheme). Sending those would leave the bars wrong for good, since nothing this
  // observes changes when the stylesheet lands. So wait for it, checking again shortly; after
  // STYLE_WAIT_TRIES checks, use what is there (a page with no `--bg` at all).
  let styleWait: ReturnType<typeof setTimeout> | undefined;
  let styleTries = 0;
  const update = () => {
    if (disposed) return;
    const colors = getComputedStyle(root);
    if (
      colors.getPropertyValue("--bg").trim() === "" &&
      styleTries < STYLE_WAIT_TRIES
    ) {
      styleTries += 1;
      if (styleWait !== undefined) clearTimeout(styleWait);
      styleWait = setTimeout(() => {
        styleWait = undefined;
        update();
      }, STYLE_WAIT_MS);
      return;
    }
    const resolved = colors.colorScheme.trim();
    const dark =
      resolved === "dark" || (resolved !== "light" && scheme.matches);
    let background: string;
    try {
      background = backgroundColor(colors);
    } catch (error) {
      console.warn("Could not resolve native background appearance", error);
      return;
    }
    desired = {
      style: dark ? SystemBarsStyle.Dark : SystemBarsStyle.Light,
      background,
    };
    if (signature(desired) !== applied) void synchronize();
  };
  const schedule = () => {
    if (disposed) return;
    if (pendingFrame === undefined)
      pendingFrame = requestAnimationFrame(() => {
        pendingFrame = undefined;
        update();
      });
    // WebKit can notify one media query before the other query's CSS override
    // has invalidated. Re-read the rendered theme once that change settles.
    if (settledUpdate !== undefined) clearTimeout(settledUpdate);
    settledUpdate = setTimeout(() => {
      settledUpdate = undefined;
      update();
    }, 50);
  };
  const observer = new MutationObserver(schedule);
  observer.observe(root, {
    attributes: true,
    attributeFilter: ["data-theme", "data-omarchy"],
  });
  scheme.addEventListener("change", schedule);
  lightScheme.addEventListener("change", schedule);
  // Every stylesheet has applied by then, whatever the checks above saw.
  addEventListener("load", schedule);
  update();
  return () => {
    disposed = true;
    observer.disconnect();
    scheme.removeEventListener("change", schedule);
    lightScheme.removeEventListener("change", schedule);
    removeEventListener("load", schedule);
    if (styleWait !== undefined) clearTimeout(styleWait);
    if (settledUpdate !== undefined) clearTimeout(settledUpdate);
    if (pendingFrame !== undefined) cancelAnimationFrame(pendingFrame);
  };
}

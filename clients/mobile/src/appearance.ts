import {
  Capacitor,
  registerPlugin,
  SystemBars,
  SystemBarsStyle,
} from "@capacitor/core";
const Layout = registerPlugin<{
  setAppearance(options: { background: string }): Promise<void>;
}>("TwrLayout");
type NativeAppearance = { style: SystemBarsStyle; background: string };

// Match system-bar contrast to the rendered theme, including an explicit dark
// theme on a light OS. Layout mutations must not send native bridge commands.
export function observeNativeAppearance() {
  if (!Capacitor.isNativePlatform()) return () => {};
  const root = document.documentElement;
  const scheme = matchMedia("(prefers-color-scheme: dark)");
  let pendingFrame: number | undefined;
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
  const update = () => {
    pendingFrame = undefined;
    const colors = getComputedStyle(root);
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

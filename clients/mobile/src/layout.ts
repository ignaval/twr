import { Keyboard } from "@capacitor/keyboard";
import {
  Capacitor,
  registerPlugin,
  type PluginListenerHandle,
} from "@capacitor/core";
import { usableViewportHeight, type Posture } from "./geometry";
const Layout = registerPlugin<{
  current(): Promise<Posture>;
  addListener(
    name: "change",
    callback: (value: Posture) => void,
  ): Promise<PluginListenerHandle>;
}>("TwrLayout");
export async function observeLayout() {
  const root = document.documentElement;
  let lastPosture: Posture = { orientation: "none" };
  const updateViewport = () => apply(lastPosture);
  const apply = (posture: Posture) => {
    lastPosture = posture;
    const viewportHeight = usableViewportHeight(
      posture,
      window.visualViewport?.height ?? window.innerHeight,
    );
    root.style.setProperty("--viewport-height", `${viewportHeight}px`);
    root.style.setProperty("--occlusion-top", `${posture.occlusionTop || 0}px`);
    const style = getComputedStyle(document.body);
    const left = parseFloat(style.paddingLeft) || 0;
    const top = parseFloat(style.paddingTop) || 0;
    const availableHeight =
      viewportHeight - top - (parseFloat(style.paddingBottom) || 0);
    const availableWidth =
      innerWidth - left - (parseFloat(style.paddingRight) || 0);
    const visible =
      posture.orientation === "horizontal"
        ? (posture.top || 0) < availableHeight
        : (posture.left || 0) < availableWidth;
    root.dataset.fold =
      visible && (posture.separating || posture.state === "half-opened")
        ? posture.orientation
        : "none";

    root.style.setProperty(
      "--fold-top",
      `${Math.max(0, (posture.top || 0) - top)}px`,
    );
    root.style.setProperty(
      "--fold-left",
      `${Math.max(0, (posture.left || 0) - left)}px`,
    );
    for (const key of ["left", "top", "width", "height"] as const)
      root.style.setProperty(
        `--hinge-${key}`,
        `${Math.max(0, posture[key] || 0)}px`,
      );
  };
  // Observe the actual content box after safe-area/camera padding. Query containers
  // themselves alter fixed positioning in older supported WebKit (bug 284945).
  const content = document.getElementById("root");
  const setContentHeight = (height: number) => {
    root.classList.toggle("short-content", height <= 400);
    root.classList.toggle("tiny-content", height <= 300);
  };
  const contentSize = new ResizeObserver((entries) => {
    if (entries[0]) setContentHeight(entries[0].contentRect.height);
  });
  if (content) {
    contentSize.observe(content);
    setContentHeight(content.clientHeight);
  }
  updateViewport();
  window.visualViewport?.addEventListener("resize", updateViewport);
  window.addEventListener("resize", updateViewport);
  let listener: PluginListenerHandle | undefined;
  const keyboardListeners: PluginListenerHandle[] = [];
  let viewportFrame = 0;
  let settledViewport: ReturnType<typeof setTimeout> | undefined;
  const keyboardChanged = (visible: boolean) => {
    root.dataset.keyboard = String(visible);
    cancelAnimationFrame(viewportFrame);
    if (settledViewport) clearTimeout(settledViewport);
    // Native WebView resizing and visualViewport notifications need not arrive
    // in the same frame. Re-read after layout and once the IME animation settles.
    updateViewport();
    viewportFrame = requestAnimationFrame(() => {
      viewportFrame = requestAnimationFrame(updateViewport);
    });
    settledViewport = setTimeout(updateViewport, 250);
  };
  if (["android", "ios"].includes(Capacitor.getPlatform())) {
    apply(await Layout.current());
    listener = await Layout.addListener("change", apply);
    keyboardListeners.push(
      await Keyboard.addListener("keyboardDidShow", () =>
        keyboardChanged(true),
      ),
    );
    keyboardListeners.push(
      await Keyboard.addListener("keyboardDidHide", () =>
        keyboardChanged(false),
      ),
    );
  }
  return () => {
    contentSize.disconnect();
    cancelAnimationFrame(viewportFrame);
    if (settledViewport) clearTimeout(settledViewport);
    delete root.dataset.keyboard;
    for (const handle of keyboardListeners) void handle.remove();
    root.classList.remove("short-content", "tiny-content");
    window.visualViewport?.removeEventListener("resize", updateViewport);
    window.removeEventListener("resize", updateViewport);
    void listener?.remove();
  };
}

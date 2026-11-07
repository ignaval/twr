import { registerPlugin } from "@capacitor/core";
import type { PluginListenerHandle } from "@capacitor/core";
import {
  initializeLayout,
  setSystemTextScale,
} from "../../shared/layoutPreferences";

const Native = registerPlugin<{
  read(options: { key: string }): Promise<{ value: string | null }>;
  write(options: { key: string; value: string }): Promise<void>;
  remove(options: { key: string }): Promise<void>;
  textScale(): Promise<{ scale: number }>;
  addListener(
    name: "textScale",
    callback: (value: { scale: number }) => void,
  ): Promise<PluginListenerHandle>;
}>("TwrOffline");
export const secureRead = async (key: string) =>
  (await Native.read({ key })).value;
export const secureWrite = (key: string, value: string) =>
  Native.write({ key, value });
export const secureDelete = (key: string) => Native.remove({ key });
export async function observeTextScale() {
  initializeLayout();
  try {
    setSystemTextScale((await Native.textScale()).scale);
  } catch {
    return () => {};
  }
  const listener = await Native.addListener("textScale", ({ scale }) =>
    setSystemTextScale(scale),
  );
  const resume = () => {
    if (document.visibilityState === "visible")
      void Native.textScale()
        .then(({ scale }) => setSystemTextScale(scale))
        .catch(() => {});
  };
  document.addEventListener("visibilitychange", resume);
  return () => {
    document.removeEventListener("visibilitychange", resume);
    void listener.remove();
  };
}

import { useEffect, useRef, useState, type RefObject } from "react";
import { armed, armsIn, hidden, shown, type Arming } from "./arming";

/** Tracks how long an attention card's options have been on screen: from mount, or from
 * when they scroll into view, and again from zero after they leave it or the window is
 * hidden. `identity` restarts it for a new item or revision. */
export function useArming(
  target: RefObject<HTMLElement | null>,
  identity: string,
) {
  const state = useRef<Arming>({});
  const [isArmed, setArmed] = useState(false);
  useEffect(() => {
    state.current = hidden();
    setArmed(false);
    const node = target.current;
    const observable = !!node && typeof IntersectionObserver !== "undefined";
    let onScreen = !observable;
    let timer: ReturnType<typeof setTimeout> | undefined;
    const update = () => {
      clearTimeout(timer);
      const now = performance.now();
      state.current =
        onScreen && document.visibilityState !== "hidden"
          ? shown(state.current, now)
          : hidden();
      setArmed(armed(state.current, now));
      const wait = armsIn(state.current, now);
      if (wait) timer = setTimeout(update, wait);
    };
    let observer: IntersectionObserver | undefined;
    if (observable) {
      observer = new IntersectionObserver((entries) => {
        onScreen = entries[entries.length - 1].isIntersecting;
        update();
      });
      observer.observe(node);
    } else update();
    document.addEventListener("visibilitychange", update);
    return () => {
      clearTimeout(timer);
      observer?.disconnect();
      document.removeEventListener("visibilitychange", update);
    };
  }, [identity]);
  return { armed: isArmed, state: () => state.current };
}

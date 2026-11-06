import { useRef } from "react";

// A mounted view belongs to one hub. Delayed callbacks and unmount cleanup must
// keep that hub's fenced API even after another hub replaces the global bridge.
export function useHostAPI() {
  return useRef(window.twr).current;
}

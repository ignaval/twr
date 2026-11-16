/** Whether a scrolling area holds more than it shows below its bottom edge (a pixel's
 * rounding aside). */
export function moreBelow({
  scrollTop,
  clientHeight,
  scrollHeight,
}: {
  scrollTop: number;
  clientHeight: number;
  scrollHeight: number;
}) {
  return scrollTop + clientHeight < scrollHeight - 1;
}

/** A ref for a scrolling area: marks it `data-more="below"` while it holds more than it
 * shows below its bottom edge, for the style to draw a cue there (in a short window a
 * request's answers can sit below the fold of the area that holds it). Kept as the area
 * scrolls, resizes and as its contents change; the returned cleanup stops it. */
export function scrollCue(node: HTMLElement | null) {
  if (!node) return;
  let frame = 0;
  const update = () => {
    frame = 0;
    if (moreBelow(node)) node.dataset.more = "below";
    else delete node.dataset.more;
  };
  const schedule = () => {
    if (!frame) frame = requestAnimationFrame(update);
  };
  update();
  node.addEventListener("scroll", schedule, { passive: true });
  const resized =
    typeof ResizeObserver === "undefined"
      ? undefined
      : new ResizeObserver(schedule);
  // The area's own size, and each request's (a card that grows or shrinks).
  const observe = () => {
    if (!resized) return;
    resized.disconnect();
    resized.observe(node);
    for (const child of node.children) resized.observe(child);
  };
  observe();
  const changed =
    typeof MutationObserver === "undefined"
      ? undefined
      : new MutationObserver(() => {
          observe();
          schedule();
        });
  changed?.observe(node, { childList: true });
  return () => {
    if (frame) cancelAnimationFrame(frame);
    node.removeEventListener("scroll", schedule);
    resized?.disconnect();
    changed?.disconnect();
  };
}

// Polling that pauses while the window is hidden (minimized, closed to the tray or
// behind the overlay panel) and catches up as soon as it is shown again.

/** Calls `fn` now and every `ms` while the page is visible. Returns a stop function. */
export function pollWhileVisible(fn: () => void, ms: number): () => void {
  let timer: ReturnType<typeof setInterval> | undefined;
  const sync = () => {
    clearInterval(timer);
    timer = undefined;
    if (document.visibilityState === 'hidden') return;
    fn();
    timer = setInterval(fn, ms);
  };
  sync();
  document.addEventListener('visibilitychange', sync);
  return () => {
    clearInterval(timer);
    document.removeEventListener('visibilitychange', sync);
  };
}

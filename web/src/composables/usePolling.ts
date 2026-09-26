import { onMounted, onUnmounted } from "vue";
export function usePolling(
  refresh: () => Promise<unknown>,
  interval: () => number = () => 2500,
) {
  let timer: ReturnType<typeof setTimeout> | undefined,
    stopped = false,
    running = false;
  async function tick() {
    clearTimeout(timer);
    if (stopped || running || document.hidden) return;
    running = true;
    try {
      await refresh();
    } finally {
      running = false;
      if (!stopped && !document.hidden) timer = setTimeout(tick, interval());
    }
  }
  function visibility() {
    clearTimeout(timer);
    if (!document.hidden) void tick();
  }
  onMounted(() => {
    void tick();
    document.addEventListener("visibilitychange", visibility);
  });
  onUnmounted(() => {
    stopped = true;
    clearTimeout(timer);
    document.removeEventListener("visibilitychange", visibility);
  });
}

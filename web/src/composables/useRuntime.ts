import { reactive, onUnmounted } from "vue";
import { api, json, latestQuery, isCancelled } from "../lib/api";
import { errorText } from "../lib/format";
import type { Runtime, Catalog, ModelStorage, Preset } from "../lib/types";
import { usePolling } from "./usePolling";
export function useRuntime() {
  const state = reactive({
    runtime: { state: "unknown" } as Runtime,
    catalog: {
      models: [],
      available_bytes: 0,
      benchmark: { state: "idle", results: [] },
    } as Catalog,
    storage: null as ModelStorage | null,
    connected: false,
    catalogConnected: false,
    loaded: false,
    error: "",
    pending: {} as Record<string, boolean>,
    source: "auto",
    preset: "7b-fp8" as Preset,
  });
  const runtimeQuery = latestQuery(),
    catalogQuery = latestQuery(),
    storageQuery = latestQuery();
  let flight: Promise<void> | undefined;
  async function refresh() {
    if (flight) return flight;
    flight = (async () => {
      const r = runtimeQuery.begin(),
        c = catalogQuery.begin();
      await Promise.all([
        api<Runtime>("/api/runtime", { signal: r.signal })
          .then((data) => {
            if (r.current()) {
              state.runtime = data;
              state.connected = true;
            }
          })
          .catch((e) => {
            if (r.current() && !isCancelled(e)) state.connected = false;
          }),
        api<Catalog>("/api/models", { signal: c.signal })
          .then((data) => {
            if (c.current()) {
              state.catalog = data;
              state.catalogConnected = true;
            }
          })
          .catch((e) => {
            if (c.current() && !isCancelled(e)) state.catalogConnected = false;
          }),
      ]);
      state.loaded = true;
    })().finally(() => {
      flight = undefined;
    });
    return flight;
  }
  async function storage() {
    const ticket = storageQuery.begin();
    try {
      const data = await api<ModelStorage>("/api/storage", {
        signal: ticket.signal,
      });
      if (ticket.current()) state.storage = data;
    } catch (error) {
      if (ticket.current() && !isCancelled(error))
        state.error = errorText(error);
    }
  }
  async function mutate(key: string, path: string, options: RequestInit) {
    if (state.pending[key]) return false;
    state.pending[key] = true;
    state.error = "";
    try {
      await api(path, options);
      runtimeQuery.cancel();
      catalogQuery.cancel();
      if (flight) await flight;
      await refresh();
      await storage();
      return true;
    } catch (error) {
      state.error = errorText(error);
      return false;
    } finally {
      state.pending[key] = false;
    }
  }
  async function download(preset: Preset) {
    await api(`/api/models/${preset}/download`, json({ source: state.source }));
    await refresh();
  }
  usePolling(refresh, () =>
    state.runtime.state === "starting" ||
    state.catalog.models.some((m) =>
      ["benchmarking", "downloading", "verifying"].includes(m.state),
    )
      ? 1000
      : 2500,
  );
  onUnmounted(() => {
    runtimeQuery.cancel();
    catalogQuery.cancel();
    storageQuery.cancel();
  });
  return { state, refresh, storage, mutate, download };
}
export type RuntimeStore = ReturnType<typeof useRuntime>;

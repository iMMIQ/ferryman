import { computed, reactive, watch } from "vue";
import { useQuery, useQueryClient } from "@tanstack/vue-query";
import { api, json } from "../lib/api";
import { errorText } from "../lib/format";
import type { ModelsResponse } from "../lib/generated/ModelsResponse";
import type { CustomModelRequest } from "../lib/generated/CustomModelRequest";
import type { ProbeRequest } from "../lib/generated/ProbeRequest";
import type { ProbeResponse } from "../lib/generated/ProbeResponse";
export function useModels() {
  const client = useQueryClient();
  const query = useQuery({
    queryKey: ["models"],
    queryFn: ({ signal }) => api<ModelsResponse>("/api/models", { signal }),
  });
  const state = reactive({
    error: "",
    warnings: [] as string[],
    partial: false,
    pending: false,
  });
  const models = computed(() => query.data.value?.models ?? []);
  const discoveryEnabled = computed(
    () => query.data.value?.discovery_enabled === true,
  );
  async function discover() {
    if (!discoveryEnabled.value || state.pending) return;
    state.pending = true;
    state.error = "";
    try {
      const result = await api<ModelsResponse>(
        "/api/models/discover",
        json({}),
        240000,
      );
      state.warnings = result.warnings;
      state.partial = result.partial;
      await client.cancelQueries({ queryKey: ["models"] });
      await query.refetch();
    } catch (error) {
      state.error = errorText(error);
    } finally {
      state.pending = false;
    }
  }
  let attempted = false;
  watch(
    query.isSuccess,
    (success) => {
      if (success && !attempted) {
        attempted = true;
        if (query.data.value?.discovery_enabled) void discover();
      }
    },
    { immediate: true },
  );
  async function save(request: CustomModelRequest) {
    await api("/api/models", json(request));
    await client.cancelQueries({ queryKey: ["models"] });
    await query.refetch();
  }
  async function remove(id: string) {
    await api(`/api/models/${encodeURIComponent(id)}`, { method: "DELETE" });
    await client.cancelQueries({ queryKey: ["models"] });
    await query.refetch();
  }
  async function probe(request: ProbeRequest) {
    return api<ProbeResponse>("/api/models/probe", json(request), 45000);
  }
  return {
    state,
    models,
    discoveryEnabled,
    discover,
    save,
    remove,
    probe,
    connected: computed(() => query.isSuccess.value),
    loaded: computed(() => !query.isPending.value),
    error: computed(() =>
      query.error.value ? errorText(query.error.value) : "",
    ),
  };
}
export type ModelsStore = ReturnType<typeof useModels>;

import { reactive } from "vue";
import { useQuery, useMutation, useQueryClient } from "@tanstack/vue-query";
import { api, json } from "../lib/api";
import { errorText } from "../lib/format";
import type { Runtime, Catalog, ModelStorage, Preset } from "../lib/types";
export function useRuntime() {
  const client = useQueryClient();
  const runtime = useQuery({
    queryKey: ["runtime"],
    queryFn: ({ signal }) => api<Runtime>("/api/runtime", { signal }),
    refetchInterval: (query) =>
      query.state.data?.state === "starting" ? 1000 : 2500,
  });
  const catalog = useQuery({
    queryKey: ["models"],
    queryFn: ({ signal }) => api<Catalog>("/api/models", { signal }),
    refetchInterval: (query) =>
      query.state.data?.models.some((m) =>
        ["benchmarking", "downloading", "verifying"].includes(m.state),
      )
        ? 1000
        : 2500,
  });
  const storageQuery = useQuery({
    queryKey: ["model-storage"],
    queryFn: ({ signal }) => api<ModelStorage>("/api/storage", { signal }),
    enabled: false,
  });
  const state = reactive({
    get runtime(): Omit<Partial<Runtime>, "state"> & {
      state: Runtime["state"] | "unknown";
    } {
      return runtime.data.value ?? { state: "unknown" };
    },
    get catalog(): Catalog {
      return (
        catalog.data.value ?? {
          models: [],
          available_bytes: 0,
          benchmark: {
            state: "idle",
            results: [],
            recommended: null,
            tested_at: null,
          },
        }
      );
    },
    get storage() {
      return storageQuery.data.value ?? null;
    },
    get connected() {
      return runtime.isSuccess.value;
    },
    get catalogConnected() {
      return catalog.isSuccess.value;
    },
    get loaded() {
      return !runtime.isPending.value && !catalog.isPending.value;
    },
    error: "",
    pending: {} as Record<string, boolean>,
    source: "auto",
    preset: "7b-fp8" as Preset,
  });
  async function refresh() {
    await Promise.all([runtime.refetch(), catalog.refetch()]);
  }
  async function storage() {
    const result = await storageQuery.refetch();
    if (result.error) state.error = errorText(result.error);
  }
  const mutation = useMutation({
    mutationFn: ({ path, options }: { path: string; options: RequestInit }) =>
      api(path, options),
    onSuccess: async () => {
      await Promise.all(
        ["runtime", "models", "model-storage"].map((key) =>
          client.cancelQueries({ queryKey: [key] }),
        ),
      );
      await Promise.all([
        client.invalidateQueries({ queryKey: ["runtime"] }),
        client.invalidateQueries({ queryKey: ["models"] }),
        storage(),
      ]);
    },
  });
  async function mutate(key: string, path: string, options: RequestInit) {
    if (state.pending[key]) return false;
    state.pending[key] = true;
    state.error = "";
    try {
      await mutation.mutateAsync({ path, options });
      return true;
    } catch (error) {
      state.error = errorText(error);
      return false;
    } finally {
      state.pending[key] = false;
    }
  }
  async function download(preset: Preset) {
    await mutation.mutateAsync({
      path: `/api/models/${preset}/download`,
      options: json({ source: state.source }),
    });
  }
  return { state, refresh, storage, mutate, download };
}
export type RuntimeStore = ReturnType<typeof useRuntime>;

import { QueryClient } from "@tanstack/vue-query";
// One cache per mounted app/session. Writes never retry automatically.
export function createQueryClient() {
  return new QueryClient({
    defaultOptions: {
      queries: { retry: false, staleTime: 0, refetchOnWindowFocus: true },
      mutations: { retry: false },
    },
  });
}

export type Storage = "documents" | "remote_fs";
export type Preset = "7b-fp8" | "30b-fp8";
export type Mode = "bilingual" | "replace";
export type Phase =
  | "all"
  | "queued"
  | "in_progress"
  | "completed"
  | "partial"
  | "cancelled"
  | "failed";
export interface Job {
  id: string;
  filename: string;
  status: string;
  preset: Preset;
  target: string;
  mode: Mode;
  total: number;
  completed: number;
  translated: number;
  failed_segments: number;
  created_at: number;
  result_available?: boolean;
  error?: string | null;
  source_path?: string;
  source_storage?: Storage;
  save_path?: string;
  save_storage?: Storage;
}
export interface JobPage {
  jobs: Job[];
  total: number;
  next_cursor?: string;
}
export interface Source {
  storage: Storage;
  path: string;
}
export interface Entry {
  name: string;
  path: string;
  kind: "directory" | "file";
  supported: boolean;
  size?: number | null;
}
export interface Listing {
  path: string;
  parent: string | null;
  entries: Entry[];
}
export interface RequestData {
  sources: Source[];
  save_strategy: "sibling_suffix" | "sibling_overwrite" | "directory";
  save_storage?: Storage;
  save_path?: string;
  preset: Preset;
  target: string;
  mode: Mode;
  settings: {
    batch_size: number;
    context_segments: number;
    cache_enabled: boolean;
  };
}
export interface PreviewFile {
  source_path: string;
  source_storage?: Storage;
  save_path: string;
  save_storage?: Storage;
  overwrite?: boolean;
  skip_reason?: string | null;
}
export interface Preview {
  files: PreviewFile[];
  eligible_count: number;
}
export interface CreatedJobs {
  jobs: Job[];
  skipped_existing?: number;
  skipped_incompatible?: number;
  skipped_unsupported?: number;
  skipped_generated?: number;
}
export interface Runtime {
  state: string;
  preset?: Preset;
  last_error?: string | null;
  active_requests?: number;
  leases?: number;
  startup_stage?: string;
  startup_progress?: number;
  startup_elapsed_seconds?: number;
  estimated_remaining_seconds?: number;
  recent_logs?: string[];
}
export interface Model {
  preset: Preset;
  state: string;
  expected_bytes: number;
  downloaded_bytes: number;
  bytes_per_second?: number;
  last_error?: string | null;
}
export interface Benchmark {
  state: string;
  recommended?: string;
  results: {
    source: string;
    label: string;
    available: boolean;
    bytes_per_second: number;
    latency_ms: number;
  }[];
}
export interface Catalog {
  models: Model[];
  available_bytes: number;
  benchmark: Benchmark;
}
export interface ModelStorage {
  model_bytes: number;
  partial_bytes: number;
  cache_bytes: number;
  available_bytes: number;
}

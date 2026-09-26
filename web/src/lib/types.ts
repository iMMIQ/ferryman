export type { ClientConfig } from "./generated/ClientConfig";
// Wire contracts are generated from the Rust DTOs. Only UI draft/view types live here.
export type { StorageKind as Storage } from "./generated/StorageKind";
export type { Preset } from "./generated/Preset";
export type { OutputMode as Mode } from "./generated/OutputMode";
export type { JobRecord as Job } from "./generated/JobRecord";
export type { JobListResponse as JobPage } from "./generated/JobListResponse";
export type { SourceSelection as Source } from "./generated/SourceSelection";
export type { DocumentEntry as Entry } from "./generated/DocumentEntry";
export type { DocumentListing as Listing } from "./generated/DocumentListing";
export type { DirectoryJobsResponse as CreatedJobs } from "./generated/DirectoryJobsResponse";
export type { RuntimeStatus as Runtime } from "./generated/RuntimeStatus";
export type { ModelStatus as Model } from "./generated/ModelStatus";
export type { BenchmarkStatus as Benchmark } from "./generated/BenchmarkStatus";
export type { ModelCatalog as Catalog } from "./generated/ModelCatalog";
export type { StorageStatus as ModelStorage } from "./generated/StorageStatus";
import type { JobPhase } from "./generated/JobPhase";
import type { CreateDirectoryJobsRequest } from "./generated/CreateDirectoryJobsRequest";
import type { PreviewFile as ServerPreviewFile } from "./generated/PreviewFile";
import type { SelectionPreview } from "./generated/SelectionPreview";
export type Phase = "all" | JobPhase;
// UI submits the modern sources representation; the server still accepts legacy fields.
export type RequestData = Required<
  Pick<
    CreateDirectoryJobsRequest,
    "sources" | "save_strategy" | "preset" | "target" | "mode" | "settings"
  >
> &
  Partial<Pick<CreateDirectoryJobsRequest, "save_storage" | "save_path">>;
// Upload previews are local until files have reached the server.
export type PreviewFile = Pick<
  ServerPreviewFile,
  "source_path" | "save_path" | "skip_reason"
> &
  Partial<
    Pick<ServerPreviewFile, "source_storage" | "save_storage" | "overwrite">
  >;
export type Preview = Pick<SelectionPreview, "eligible_count"> & {
  files: PreviewFile[];
};

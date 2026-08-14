//! Document listing and directory creation for the mounted user storages.

use super::{
    json_error, normalize_relative_path, path_for_api, resolve_storage_path, user_identity,
    AppState, StorageKind,
};
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use ferryman::format::Format;
use serde::{Deserialize, Serialize};
use std::path::Path as FsPath;

#[derive(Deserialize)]
pub(super) struct DocumentQuery {
    storage: StorageKind,
    #[serde(default)]
    path: String,
}

#[derive(Deserialize)]
pub(super) struct CreateDirectoryRequest {
    storage: StorageKind,
    path: String,
}

#[derive(Serialize)]
struct DocumentEntry {
    name: String,
    path: String,
    kind: &'static str,
    supported: bool,
    size: Option<u64>,
}

#[derive(Serialize)]
struct DocumentListing {
    path: String,
    parent: Option<String>,
    entries: Vec<DocumentEntry>,
}

pub(super) async fn list_documents(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<DocumentQuery>,
) -> Response {
    let identity = match user_identity(&headers, state.config.allow_local_user) {
        Ok(identity) => identity,
        Err(status) => return json_error(status, "missing or invalid user identity"),
    };
    let (_, directory, relative) =
        match resolve_storage_path(&state.config, &identity, query.storage, &query.path).await {
            Ok(paths) => paths,
            Err(error) => return json_error(StatusCode::NOT_FOUND, format!("{error:#}")),
        };
    match tokio::fs::metadata(&directory).await {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => return json_error(StatusCode::BAD_REQUEST, "document path is not a directory"),
        Err(error) => return json_error(StatusCode::NOT_FOUND, error.to_string()),
    }

    let mut reader = match tokio::fs::read_dir(&directory).await {
        Ok(reader) => reader,
        Err(error) => return json_error(StatusCode::FORBIDDEN, error.to_string()),
    };
    let mut entries = Vec::new();
    loop {
        let entry = match reader.next_entry().await {
            Ok(Some(entry)) => entry,
            Ok(None) => break,
            Err(error) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
        };
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let metadata = match tokio::fs::symlink_metadata(entry.path()).await {
            Ok(metadata) if !metadata.file_type().is_symlink() => metadata,
            _ => continue,
        };
        let kind = if metadata.is_dir() {
            "directory"
        } else if metadata.is_file() {
            "file"
        } else {
            continue;
        };
        let entry_relative = relative.join(&name);
        entries.push(DocumentEntry {
            name,
            path: path_for_api(&entry_relative),
            kind,
            supported: metadata.is_file() && Format::from_path(&entry.path()).is_ok(),
            size: metadata.is_file().then_some(metadata.len()),
        });
    }
    entries.sort_by(|left, right| {
        (left.kind != "directory", left.name.to_lowercase())
            .cmp(&(right.kind != "directory", right.name.to_lowercase()))
    });
    let parent = relative
        .parent()
        .map(path_for_api)
        .filter(|_| !relative.as_os_str().is_empty());
    Json(DocumentListing {
        path: path_for_api(&relative),
        parent,
        entries,
    })
    .into_response()
}

pub(super) async fn create_document_directory(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<CreateDirectoryRequest>,
) -> Response {
    let identity = match user_identity(&headers, state.config.allow_local_user) {
        Ok(identity) => identity,
        Err(status) => return json_error(status, "missing or invalid user identity"),
    };
    let relative = match normalize_relative_path(&request.path) {
        Ok(path) if !path.as_os_str().is_empty() => path,
        _ => return json_error(StatusCode::BAD_REQUEST, "folder name is required"),
    };
    let Some(name) = relative.file_name() else {
        return json_error(StatusCode::BAD_REQUEST, "invalid folder name");
    };
    let parent_relative = relative.parent().unwrap_or_else(|| FsPath::new(""));
    let (root, parent, _) = match resolve_storage_path(
        &state.config,
        &identity,
        request.storage,
        &path_for_api(parent_relative),
    )
    .await
    {
        Ok(paths) => paths,
        Err(error) => return json_error(StatusCode::NOT_FOUND, format!("{error:#}")),
    };
    let destination = parent.join(name);
    match tokio::fs::create_dir(&destination).await {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            return json_error(StatusCode::CONFLICT, "folder already exists")
        }
        Err(error) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    }
    match tokio::fs::canonicalize(&destination).await {
        Ok(path) if path.starts_with(&root) => {}
        _ => {
            let _ = tokio::fs::remove_dir(&destination).await;
            return json_error(StatusCode::BAD_REQUEST, "folder escapes the user directory");
        }
    }
    (
        StatusCode::CREATED,
        Json(serde_json::json!({"path": path_for_api(&relative)})),
    )
        .into_response()
}

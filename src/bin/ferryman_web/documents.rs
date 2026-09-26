//! Document listing and directory creation for the mounted user storages.

use super::{
    json_error, normalize_relative_path, path_for_api, resolve_storage_path, AppState, StorageKind,
    UserIdentity,
};
use axum::extract::{Query, State};
use axum::http::StatusCode;
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

#[derive(Serialize, ts_rs::TS, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum DocumentKind {
    Directory,
    File,
}

#[derive(Serialize, ts_rs::TS)]
struct DocumentEntry {
    name: String,
    path: String,
    kind: DocumentKind,
    supported: bool,
    size: Option<u64>,
}

#[derive(Serialize, ts_rs::TS)]
struct DocumentListing {
    path: String,
    parent: Option<String>,
    entries: Vec<DocumentEntry>,
}

pub(super) async fn list_documents(
    State(state): State<AppState>,
    identity: UserIdentity,
    Query(query): Query<DocumentQuery>,
) -> Response {
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
            DocumentKind::Directory
        } else if metadata.is_file() {
            DocumentKind::File
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
        (
            left.kind != DocumentKind::Directory,
            left.name.to_lowercase(),
        )
            .cmp(&(
                right.kind != DocumentKind::Directory,
                right.name.to_lowercase(),
            ))
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
    identity: UserIdentity,
    Json(request): Json<CreateDirectoryRequest>,
) -> Response {
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

#[cfg(test)]
#[test]
#[ignore = "run via npm run types:generate or types:check"]
fn export_frontend_types() {
    use ts_rs::TS;
    let config = ts_rs::Config::new().with_large_int("number").with_out_dir(
        std::env::var_os("FERRYMAN_TYPES_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| {
                std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("web/src/lib/generated")
            }),
    );
    DocumentListing::export_all(&config).unwrap();
}

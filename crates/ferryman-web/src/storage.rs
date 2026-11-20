//! User storage roots, path normalization, and subtree confinement.

use super::{Config, UserIdentity};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Component, Path as FsPath, PathBuf};

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case")]
#[derive(ts_rs::TS)]
pub(crate) enum StorageKind {
    Documents,
    RemoteFs,
}

impl StorageKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Documents => "documents",
            Self::RemoteFs => "remote_fs",
        }
    }

    pub(crate) fn from_str(value: &str) -> std::result::Result<Self, String> {
        match value {
            "documents" => Ok(Self::Documents),
            "remote_fs" => Ok(Self::RemoteFs),
            _ => Err(format!("invalid storage kind: {value}")),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[derive(ts_rs::TS)]
pub(crate) enum SaveStrategy {
    SiblingSuffix,
    SiblingOverwrite,
    #[default]
    Directory,
}

pub(crate) fn normalize_relative_path(value: &str) -> Result<PathBuf> {
    let mut normalized = PathBuf::new();
    for component in FsPath::new(value).components() {
        match component {
            Component::Normal(part) if !part.is_empty() => normalized.push(part),
            Component::CurDir => {}
            _ => anyhow::bail!("invalid document path"),
        }
    }
    Ok(normalized)
}

pub(crate) fn path_for_api(path: &FsPath) -> String {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(part) => Some(part.to_string_lossy()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

pub(crate) fn storage_root(config: &Config, storage: StorageKind) -> &FsPath {
    match storage {
        StorageKind::Documents => &config.user_documents_dir,
        StorageKind::RemoteFs => &config.remote_fs_dir,
    }
}

pub(crate) async fn user_storage_root(
    config: &Config,
    identity: &UserIdentity,
    storage: StorageKind,
) -> Result<PathBuf> {
    let configured_root = storage_root(config, storage);
    let mount_root = tokio::fs::canonicalize(configured_root)
        .await
        .with_context(|| {
            format!(
                "storage mount is unavailable: {}",
                configured_root.display()
            )
        })?;
    let user_root = tokio::fs::canonicalize(mount_root.join(&identity.uid))
        .await
        .with_context(|| format!("storage is unavailable for user {}", identity.uid))?;
    if !user_root.starts_with(&mount_root) {
        anyhow::bail!("user storage escapes its mounted directory");
    }
    Ok(user_root)
}

pub(crate) async fn resolve_storage_path(
    config: &Config,
    identity: &UserIdentity,
    storage: StorageKind,
    value: &str,
) -> Result<(PathBuf, PathBuf, PathBuf)> {
    let relative = normalize_relative_path(value)?;
    let root = user_storage_root(config, identity, storage).await?;
    let candidate = tokio::fs::canonicalize(root.join(&relative))
        .await
        .with_context(|| format!("open storage path {}", path_for_api(&relative)))?;
    if !candidate.starts_with(&root) {
        anyhow::bail!("storage path escapes its mounted directory");
    }
    Ok((root, candidate, relative))
}

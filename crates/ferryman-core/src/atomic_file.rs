//! Same-directory temporary files with automatic cleanup and atomic publication.
use std::io::Write;
use std::path::PathBuf;

/// Replacement is atomic; no power-loss durability guarantee (callers may sync if needed).
pub async fn write(path: PathBuf, bytes: Vec<u8>) -> std::io::Result<()> {
    tokio::task::spawn_blocking(move || {
        let parent = path.parent().unwrap_or_else(|| std::path::Path::new("."));
        std::fs::create_dir_all(parent)?;
        let mut file = tempfile::NamedTempFile::new_in(parent)?;
        file.write_all(&bytes)?;
        file.persist(path).map_err(|error| error.error)?;
        Ok(())
    })
    .await
    .map_err(std::io::Error::other)?
}

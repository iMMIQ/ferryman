//! Job execution: model lease heartbeat, translation pipeline, and saving
//! results back into user storage.

use super::{mutate_job, AppState, JobEntry, JobStatus};
use crate::agent_proxy::{acquire_agent, acquire_agent_with_retry, release_agent, wait_for_agent};
use anyhow::{Context, Result};
use ferryman::batch::{run_batch_controlled, BatchOpts, BatchProgress, ProgressCallback};
use ferryman::cache::Cache;
use ferryman::engine::Engine;

use std::path::{Component, Path as FsPath, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::warn;
use uuid::Uuid;

async fn ensure_safe_output_parent(root: &FsPath, target: &FsPath) -> Result<PathBuf> {
    let root = tokio::fs::canonicalize(root).await?;
    let parent = target
        .parent()
        .ok_or_else(|| anyhow::anyhow!("output path has no parent"))?;
    let relative = parent
        .strip_prefix(&root)
        .context("output path escapes the user directory")?;
    let mut current = root.clone();
    for component in relative.components() {
        let Component::Normal(part) = component else {
            anyhow::bail!("invalid output directory");
        };
        let next = current.join(part);
        match tokio::fs::symlink_metadata(&next).await {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                anyhow::bail!("output directory contains a symbolic link")
            }
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => anyhow::bail!("output parent is not a directory"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                tokio::fs::create_dir(&next).await?;
            }
            Err(error) => return Err(error.into()),
        }
        current = tokio::fs::canonicalize(&next).await?;
        if !current.starts_with(&root) {
            anyhow::bail!("output directory escapes the user directory");
        }
    }
    Ok(current)
}

async fn save_job_result(entry: &JobEntry) -> Result<()> {
    let (Some(target), Some(root)) = (&entry.save_to, &entry.save_root) else {
        return Ok(());
    };
    let parent = ensure_safe_output_parent(root, target).await?;
    let filename = target
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("output path has no filename"))?;
    let target = parent.join(filename);
    match tokio::fs::symlink_metadata(&target).await {
        Ok(metadata)
            if entry.overwrite && metadata.is_file() && !metadata.file_type().is_symlink() => {}
        Ok(_) if entry.overwrite => anyhow::bail!("overwrite target is not a regular file"),
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
            if files_are_identical(&entry.output, &target).await? {
                return Ok(());
            }
            anyhow::bail!("output file already exists")
        }
        Ok(_) => anyhow::bail!("output file already exists"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && !entry.overwrite => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            anyhow::bail!("overwrite target no longer exists")
        }
        Err(error) => return Err(error.into()),
    }
    let temp = parent.join(format!(".ferryman-{}.tmp", Uuid::new_v4()));
    let copy_result = async {
        let mut source = tokio::fs::File::open(&entry.output).await?;
        let mut destination = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .await?;
        tokio::io::copy(&mut source, &mut destination).await?;
        destination.sync_all().await?;
        if entry.overwrite {
            tokio::fs::rename(&temp, &target).await?;
        } else {
            tokio::fs::hard_link(&temp, &target).await?;
        }
        Ok::<_, anyhow::Error>(())
    }
    .await;
    let _ = tokio::fs::remove_file(&temp).await;
    copy_result.with_context(|| format!("save result to {}", target.display()))
}

pub(crate) async fn files_are_identical(left: &FsPath, right: &FsPath) -> Result<bool> {
    let left_metadata = tokio::fs::metadata(left).await?;
    let right_metadata = tokio::fs::metadata(right).await?;
    if left_metadata.len() != right_metadata.len() {
        return Ok(false);
    }
    let mut left = tokio::fs::File::open(left).await?;
    let mut right = tokio::fs::File::open(right).await?;
    let mut left_buffer = [0_u8; 64 * 1024];
    let mut right_buffer = [0_u8; 64 * 1024];
    loop {
        let left_read = left.read(&mut left_buffer).await?;
        let right_read = right.read(&mut right_buffer).await?;
        if left_read != right_read || left_buffer[..left_read] != right_buffer[..right_read] {
            return Ok(false);
        }
        if left_read == 0 {
            return Ok(true);
        }
    }
}

pub(crate) async fn run_job(state: &AppState, entry: JobEntry) -> Result<()> {
    let id = entry.record.id;
    let preset = entry.record.preset;
    let lease_id = format!("job-{id}");
    let cancel = CancellationToken::new();
    state.cancellations.lock().await.insert(id, cancel.clone());
    if state
        .active_jobs
        .read()
        .await
        .get(&id)
        .is_none_or(|entry| entry.record.status != JobStatus::StartingModel)
    {
        state.cancellations.lock().await.remove(&id);
        return Ok(());
    }

    release_agent(&state.config, &format!("manual-{}", entry.owner)).await;
    if let Err(error) = acquire_agent_with_retry(&state.config, preset, &lease_id, &cancel).await {
        state.cancellations.lock().await.remove(&id);
        return Err(error);
    }
    let heartbeat_cancel = CancellationToken::new();
    let heartbeat = {
        let state = state.clone();
        let lease_id = lease_id.clone();
        let heartbeat_cancel = heartbeat_cancel.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = heartbeat_cancel.cancelled() => break,
                    _ = tokio::time::sleep(Duration::from_secs(30)) => {
                        if let Err(error) = acquire_agent(&state.config, preset, &lease_id).await {
                            warn!(%error, "renew model lease");
                        }
                    }
                }
            }
        })
    };

    let result = async {
        wait_for_agent(&state.config, preset, &lease_id, &cancel).await?;
        mutate_job(state, id, |job| job.status = JobStatus::Translating).await;

        let cache_dir = state
            .config
            .data_dir
            .join("users")
            .join(&entry.owner)
            .join("cache");
        let request_limiter = state
            .request_limiters
            .get(&preset)
            .cloned()
            .context("missing request limiter for model preset")?;
        let cache = if entry.record.settings.cache_enabled {
            Cache::open(Some(cache_dir))
        } else {
            None
        };
        let engine = Engine::new(
            state.translation_client.clone(),
            state.config.agent_url.clone(),
            preset.api_model().to_string(),
            entry.record.target.clone(),
            preset.config().concurrency,
            cache,
        )
        .with_request_limiter(request_limiter);

        let (progress_tx, mut progress_rx) = watch::channel(BatchProgress::default());
        let callback: ProgressCallback = Arc::new(move |progress| {
            progress_tx.send_replace(progress);
        });
        let progress_state = state.clone();
        let progress_task = tokio::spawn(async move {
            while progress_rx.changed().await.is_ok() {
                tokio::time::sleep(Duration::from_millis(350)).await;
                let progress = progress_rx.borrow_and_update().clone();
                mutate_job(&progress_state, id, |job| {
                    job.total = progress.total;
                    job.completed = progress.completed;
                    job.translated = progress.translated;
                    job.failed_segments = progress.failed;
                })
                .await;
            }
        });
        let summary = run_batch_controlled(
            &engine,
            vec![entry.input.clone()],
            BatchOpts {
                mode: entry.record.mode,
                in_place: false,
                output: Some(entry.output.clone()),
                batch_size: entry.record.settings.batch_size,
                context: entry.record.settings.context_segments,
                limit: None,
                prompt_char_budget: entry.record.preset.prompt_char_budget(),
            },
            cancel.clone(),
            callback,
        )
        .await;
        let _ = progress_task.await;

        if cancel.is_cancelled() || summary.cancelled {
            let result_available = tokio::fs::metadata(&entry.output)
                .await
                .is_ok_and(|metadata| metadata.is_file());
            mutate_job(state, id, |job| {
                job.status = JobStatus::Cancelled;
                job.result_available = result_available;
            })
            .await;
        } else if !summary.failed_files.is_empty() {
            anyhow::bail!("{}", summary.failed_files[0].1);
        } else {
            if entry.save_to.is_some() {
                mutate_job(state, id, |job| job.status = JobStatus::Writing).await;
                save_job_result(&entry).await?;
            }
            mutate_job(state, id, |job| {
                job.status = JobStatus::Completed;
                job.total = summary.translated + summary.failed;
                job.completed = job.total;
                job.translated = summary.translated;
                job.failed_segments = summary.failed;
                job.error = None;
                job.result_available = true;
            })
            .await;
        }
        Ok::<_, anyhow::Error>(())
    }
    .await;

    heartbeat_cancel.cancel();
    heartbeat.abort();
    release_agent(&state.config, &lease_id).await;
    state.cancellations.lock().await.remove(&id);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{now_epoch_seconds, JobRecord, StorageKind};
    use ferryman::format::OutputMode;
    use ferryman::preset::Preset;
    use ferryman::settings::TranslationSettings;
    use std::env;

    #[tokio::test]
    async fn safe_output_parent_stays_under_document_root() {
        let base = env::temp_dir().join(format!("ferryman-path-test-{}", Uuid::new_v4()));
        let root = base.join("documents");
        tokio::fs::create_dir_all(&root).await.unwrap();
        let target = root.join("Translations/Books/book.bilingual.epub");
        let parent = ensure_safe_output_parent(&root, &target).await.unwrap();
        assert!(parent.starts_with(tokio::fs::canonicalize(&root).await.unwrap()));
        assert!(parent.ends_with("Translations/Books"));
        tokio::fs::remove_dir_all(&base).await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn safe_output_parent_rejects_symbolic_links() {
        use std::os::unix::fs::symlink;

        let base = env::temp_dir().join(format!("ferryman-link-test-{}", Uuid::new_v4()));
        let root = base.join("documents");
        let outside = base.join("outside");
        tokio::fs::create_dir_all(&root).await.unwrap();
        tokio::fs::create_dir_all(&outside).await.unwrap();
        symlink(&outside, root.join("escape")).unwrap();
        let target = root.join("escape/result.epub");
        assert!(ensure_safe_output_parent(&root, &target).await.is_err());
        tokio::fs::remove_dir_all(&base).await.unwrap();
    }

    #[tokio::test]
    async fn saved_results_are_copied_without_overwriting() {
        let base = env::temp_dir().join(format!("ferryman-save-test-{}", Uuid::new_v4()));
        let root = base.join("documents");
        let job_dir = base.join("job");
        let output = job_dir.join("result.txt");
        let target = root.join("Translations/notes.bilingual.txt");
        tokio::fs::create_dir_all(&root).await.unwrap();
        tokio::fs::create_dir_all(&job_dir).await.unwrap();
        tokio::fs::write(&output, b"translated result")
            .await
            .unwrap();
        let now = now_epoch_seconds();
        let entry = JobEntry {
            owner: "test".to_string(),
            dir: job_dir,
            input: base.join("input.txt"),
            output,
            save_to: Some(target.clone()),
            save_root: Some(root),
            overwrite: false,
            record: JobRecord {
                id: Uuid::new_v4(),
                filename: "notes.txt".to_string(),
                preset: Preset::SevenBFp8,
                target: "中文".to_string(),
                mode: OutputMode::Bilingual,
                status: JobStatus::Writing,
                total: 0,
                completed: 0,
                translated: 0,
                failed_segments: 0,
                error: None,
                settings: TranslationSettings::default(),
                result_available: false,
                source_path: None,
                source_storage: None,
                save_path: Some("Translations/notes.bilingual.txt".to_string()),
                save_storage: Some(StorageKind::Documents),
                created_at: now,
                updated_at: now,
            },
        };
        save_job_result(&entry).await.unwrap();
        assert_eq!(
            tokio::fs::read(&target).await.unwrap(),
            b"translated result"
        );
        save_job_result(&entry).await.unwrap();
        tokio::fs::write(&target, b"different result")
            .await
            .unwrap();
        assert!(save_job_result(&entry).await.is_err());
        tokio::fs::remove_dir_all(&base).await.unwrap();
    }

    #[tokio::test]
    async fn saved_results_atomically_overwrite_regular_files_when_requested() {
        let base = env::temp_dir().join(format!("ferryman-overwrite-test-{}", Uuid::new_v4()));
        let root = base.join("documents");
        let job_dir = base.join("job");
        let output = job_dir.join("result.txt");
        let target = root.join("notes.txt");
        tokio::fs::create_dir_all(&root).await.unwrap();
        tokio::fs::create_dir_all(&job_dir).await.unwrap();
        tokio::fs::write(&target, b"original").await.unwrap();
        tokio::fs::write(&output, b"translated result")
            .await
            .unwrap();
        let now = now_epoch_seconds();
        let entry = JobEntry {
            owner: "test".to_string(),
            dir: job_dir,
            input: base.join("input.txt"),
            output,
            save_to: Some(target.clone()),
            save_root: Some(root),
            overwrite: true,
            record: JobRecord {
                id: Uuid::new_v4(),
                filename: "notes.txt".to_string(),
                preset: Preset::SevenBFp8,
                target: "中文".to_string(),
                mode: OutputMode::Bilingual,
                status: JobStatus::Writing,
                total: 0,
                completed: 0,
                translated: 0,
                failed_segments: 0,
                error: None,
                settings: TranslationSettings::default(),
                result_available: false,
                source_path: Some("notes.txt".to_string()),
                source_storage: Some(StorageKind::Documents),
                save_path: Some("notes.txt".to_string()),
                save_storage: Some(StorageKind::Documents),
                created_at: now,
                updated_at: now,
            },
        };

        save_job_result(&entry).await.unwrap();
        assert_eq!(
            tokio::fs::read(&target).await.unwrap(),
            b"translated result"
        );
        tokio::fs::remove_dir_all(&base).await.unwrap();
    }
}

//! Background retention sweeps: expired terminal jobs, idempotency receipts,
//! and bounded per-user translation caches.

use super::{lock_job, now_epoch_seconds, AppState, JOB_SWEEP_BATCH};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use tracing::{error, info, warn};

/// Default per-user shared translation cache budget (2 GiB).
pub(crate) const DEFAULT_CACHE_MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// Default per-user shared translation cache TTL (30 days).
pub(crate) const DEFAULT_CACHE_TTL_SECONDS: u64 = 30 * 24 * 3600;

/// One retention pass: remove the job directories of expired terminal jobs,
/// then their DB rows, plus expired submission receipts.
pub(crate) async fn sweep_terminal_jobs(state: &AppState, retention: Duration) {
    sweep_terminal_jobs_at(state, retention, now_epoch_seconds()).await;
}

pub(crate) async fn sweep_terminal_jobs_at(state: &AppState, retention: Duration, now: u64) {
    let cutoff = now.saturating_sub(retention.as_secs());
    let mut swept = 0usize;
    loop {
        let expired = match state.store.expired_terminal(cutoff, JOB_SWEEP_BATCH).await {
            Ok(expired) => expired,
            Err(error) => {
                error!(%error, "list expired terminal jobs");
                return;
            }
        };
        if expired.is_empty() {
            break;
        }
        let mut removed = Vec::with_capacity(expired.len());
        for candidate in &expired {
            let _transition = lock_job(state, candidate.record.id).await;
            // Re-read under the same lock used by retry/delete. A terminal
            // snapshot from the initial query is not permission to remove files.
            let entry = match state
                .store
                .get(candidate.owner.clone(), candidate.record.id)
                .await
            {
                Ok(Some(entry))
                    if entry.record.status.is_terminal() && entry.record.updated_at < cutoff =>
                {
                    entry
                }
                _ => continue,
            };
            if state
                .active_jobs
                .read()
                .await
                .contains_key(&entry.record.id)
            {
                continue;
            }
            match tokio::fs::remove_dir_all(&entry.dir).await {
                Ok(()) => {
                    if state
                        .store
                        .delete_terminal_ids(vec![entry.record.id])
                        .await
                        .is_ok_and(|deleted| deleted > 0)
                    {
                        removed.push(entry.record.id);
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    if state
                        .store
                        .delete_terminal_ids(vec![entry.record.id])
                        .await
                        .is_ok_and(|deleted| deleted > 0)
                    {
                        removed.push(entry.record.id);
                    }
                }
                Err(error) => {
                    warn!(
                        id = %entry.record.id,
                        dir = %entry.dir.display(),
                        %error,
                        "remove expired job directory"
                    );
                }
            }
        }
        if removed.is_empty() {
            // Every dir in this batch failed; deleting nothing and looping
            // again would spin forever on the same rows.
            break;
        }
        swept += removed.len();
        if expired.len() < JOB_SWEEP_BATCH {
            break;
        }
    }
    if swept > 0 {
        info!(jobs = swept, "swept expired terminal jobs");
    }

    match state.store.delete_expired_submissions(cutoff).await {
        Ok(deleted) if deleted > 0 => {
            info!(submissions = deleted, "swept expired submission receipts");
        }
        Ok(_) => {}
        Err(error) => error!(%error, "sweep expired submission receipts"),
    }
}

/// Evict stale or over-budget entries from every user's shared translation cache.
pub(crate) async fn evict_user_caches(data_dir: &Path, max_bytes: u64, ttl: Duration) {
    let users_dir = data_dir.join("users");
    let _ = tokio::task::spawn_blocking(move || {
        let Ok(users) = std::fs::read_dir(&users_dir) else {
            return;
        };
        let now = SystemTime::now();
        for user in users.flatten() {
            let cache_dir = user.path().join("cache");
            if cache_dir.is_dir() {
                let evicted = evict_cache_dir(&cache_dir, max_bytes, ttl, now);
                if evicted > 0 {
                    info!(dir = %cache_dir.display(), files = evicted, "evicted user cache entries");
                }
            }
        }
    })
    .await;
}

pub(crate) fn evict_cache_dir(
    cache_dir: &Path,
    max_bytes: u64,
    ttl: Duration,
    now: SystemTime,
) -> usize {
    let mut entries: Vec<(PathBuf, u64, SystemTime)> = Vec::new();
    let mut removed = 0usize;
    let cutoff = now.checked_sub(ttl);

    for entry in walkdir::WalkDir::new(cache_dir)
        .min_depth(1)
        .max_depth(3)
        .follow_links(false)
        .into_iter()
        .flatten()
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        if let Some(cutoff) = cutoff {
            if !ttl.is_zero() && mtime < cutoff {
                if std::fs::remove_file(entry.path()).is_ok() {
                    removed += 1;
                }
                continue;
            }
        }
        entries.push((entry.into_path(), meta.len(), mtime));
    }

    if max_bytes > 0 {
        let mut total: u64 = entries.iter().map(|(_, len, _)| *len).sum();
        if total > max_bytes {
            // Evict oldest modified entries first until within budget.
            entries.sort_by_key(|(_, _, mtime)| *mtime);
            for (path, len, _) in entries {
                if total <= max_bytes {
                    break;
                }
                if std::fs::remove_file(&path).is_ok() {
                    total = total.saturating_sub(len);
                    removed += 1;
                }
            }
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evict_cache_dir_enforces_size_budget_oldest_first() {
        let dir = tempfile::tempdir().unwrap();
        let shard = dir.path().join("ab");
        std::fs::create_dir_all(&shard).unwrap();
        let older = shard.join("old");
        let newer = shard.join("new");
        std::fs::write(&older, vec![b'a'; 60]).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(&newer, vec![b'b'; 60]).unwrap();

        let removed = evict_cache_dir(dir.path(), 80, Duration::from_secs(3600), SystemTime::now());
        assert_eq!(removed, 1);
        assert!(!older.exists());
        assert!(newer.exists());
    }
}

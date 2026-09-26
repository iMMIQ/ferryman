//! On-disk translation cache.
//!
//! Content-addressed: each versioned translation request hashes to a SHA-256
//! key whose translation is stored as a UTF-8 file under a sharded directory.
//! This makes translation runs **resumable** — re-running ferryman on the same
//! book (same model + target language) skips already-translated blocks
//! instantly, and a Ctrl-C'd run keeps everything that finished.
//!
//! All operations are best-effort: the cache is purely an optimization, so any
//! IO error disables that one entry (or logs a warning) and never fails the run.

use sha2::{Digest, Sha256};
use std::fs;
use std::path::PathBuf;

pub struct Cache {
    root: PathBuf,
}

impl Cache {
    /// `Some(dir)` → open (creating the root) a cache at `dir`; `None` → cache
    /// disabled. If the root can't be created the cache is disabled with a
    /// warning rather than aborting — translation works fine without it.
    pub fn open(dir: Option<PathBuf>) -> Option<Self> {
        let root = dir?;
        let cache = Cache { root };
        if fs::create_dir_all(&cache.root).is_err() {
            eprintln!(
                "warn: cache dir {:?} unusable, caching disabled",
                cache.root
            );
            return None;
        }
        Some(cache)
    }

    /// Versioned, unambiguous key. Bump TRANSLATION_CACHE_VERSION whenever
    /// prompts, decoding parameters or recovery semantics change.
    /// `scope` identifies the endpoint, strategy and complete batch context.
    pub fn key(&self, model: &str, target: &str, text: &str, scope: &str) -> String {
        let bytes = serde_json::to_vec(&(
            crate::translate::TRANSLATION_CACHE_VERSION,
            model,
            target,
            text,
            scope,
        ))
        .expect("serialize cache key strings");
        hex::encode(Sha256::digest(bytes))
    }

    /// Returns the cached translation for `key`, or `None` on miss / read
    /// error. Async (`tokio::fs`): a 10k-block book issues ~20k of these per
    /// run, and the sync version blocked the async workers between HTTP polls.
    pub async fn get(&self, key: &str) -> Option<String> {
        tokio::fs::read_to_string(self.path_of(key)).await.ok()
    }

    /// Write `val` under `key`, best-effort. Atomic via tmp-file + rename within
    /// the same shard directory, so an interrupt can never leave a half-written
    /// file that `get` would later serve as a truncated hit. Errors are logged,
    /// never bubbled — a failed write just means a re-translate later. No
    /// `fsync`: the goal is surviving Ctrl-C (page cache survives process
    /// exit), not power loss, and fsync per block would dominate a large book.
    pub async fn put(&self, key: &str, val: &str) {
        let final_path = self.path_of(key);
        if let Err(e) = crate::atomic_file::write(final_path.clone(), val.as_bytes().to_vec()).await
        {
            eprintln!("warn: cache write {:?} failed: {}", final_path, e);
        }
    }

    /// Sharded path: `root / <first 2 hex> / <remaining 62 hex>`. Sharding keeps
    /// any single directory small even for huge books (10k+ blocks), avoiding
    /// slow directory scans on some filesystems.
    fn path_of(&self, key: &str) -> PathBuf {
        let (prefix, rest) = key.split_at(2.min(key.len()));
        self.root.join(prefix).join(rest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_keys_are_unambiguous_and_versioned() {
        let cache = Cache {
            root: PathBuf::new(),
        };
        assert_ne!(
            cache.key("a\u{1f}b", "c", "text", "scope"),
            cache.key("a", "b\u{1f}c", "text", "scope")
        );
        assert_ne!(
            cache.key("model", "zh", "text", "context-one"),
            cache.key("model", "zh", "text", "context-two")
        );
        let legacy: String = Sha256::digest(b"model\x1fzh\x1ftext")
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_ne!(cache.key("model", "zh", "text", "scope"), legacy);
    }

    #[tokio::test]
    async fn independent_instances_publish_only_complete_values() {
        let root = std::env::temp_dir().join(format!("ferryman-cache-{}", uuid::Uuid::new_v4()));
        let reader = Cache::open(Some(root.clone())).unwrap();
        let key = reader.key("model", "zh", "text", "scope");
        let mut tasks = tokio::task::JoinSet::new();
        for i in 0..24 {
            let cache = Cache::open(Some(root.clone())).unwrap();
            let key = key.clone();
            tasks.spawn(async move {
                cache.put(&key, &i.to_string().repeat(65536)).await;
            });
        }
        while let Some(result) = tasks.join_next().await {
            result.unwrap();
            if let Some(value) = reader.get(&key).await {
                assert!((0..24).any(|i| value == i.to_string().repeat(65536)));
            }
        }
        assert!(reader.get(&key).await.is_some());
        let mut entries = tokio::fs::read_dir(reader.path_of(&key).parent().unwrap())
            .await
            .unwrap();
        let mut count = 0;
        while entries.next_entry().await.unwrap().is_some() {
            count += 1;
        }
        assert_eq!(count, 1);
        tokio::fs::remove_dir_all(root).await.unwrap();
    }
}

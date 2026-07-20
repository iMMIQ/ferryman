//! On-disk translation cache.
//!
//! Content-addressed: each `(model, target, text)` triple hashes to a SHA-256
//! key whose translation is stored as a UTF-8 file under a sharded directory.
//! This makes translation runs **resumable** — re-running ferryman on the same
//! book (same model + target language) skips already-translated blocks
//! instantly, and a Ctrl-C'd run keeps everything that finished.
//!
//! All operations are best-effort: the cache is purely an optimization, so any
//! IO error disables that one entry (or logs a warning) and never fails the run.

use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

pub struct Cache {
    root: PathBuf,
    /// Monotonic counter making concurrent tmp-file names unique within this
    /// process (two segments with identical text would otherwise collide).
    counter: AtomicU64,
}

impl Cache {
    /// `Some(dir)` → open (creating the root) a cache at `dir`; `None` → cache
    /// disabled. If the root can't be created the cache is disabled with a
    /// warning rather than aborting — translation works fine without it.
    pub fn open(dir: Option<PathBuf>) -> Option<Self> {
        let root = dir?;
        let cache = Cache {
            root,
            counter: AtomicU64::new(0),
        };
        if fs::create_dir_all(&cache.root).is_err() {
            eprintln!(
                "warn: cache dir {:?} unusable, caching disabled",
                cache.root
            );
            return None;
        }
        Some(cache)
    }

    /// Stable content key: lowercase hex of SHA-256 over
    /// `model ‖ 0x1f ‖ target ‖ 0x1f ‖ text`. The `0x1f` (ASCII unit
    /// separator) prevents concatenation ambiguity between triples such as
    /// `("ab","c")` and `("a","bc")`.
    ///
    /// NOTE: model + target + input text fully determine the cached value only
    /// because the sampling params (temperature/top_p/top_k/repetition_penalty)
    /// are hardcoded in `translate.rs`. If you ever make any of those a CLI
    /// flag or preset-derived, fold them into this key — otherwise the cache
    /// will silently serve translations produced under a different decoding
    /// config.
    pub fn key(&self, model: &str, target: &str, text: &str) -> String {
        let mut h = Sha256::new();
        h.update(model.as_bytes());
        h.update([0x1f]);
        h.update(target.as_bytes());
        h.update([0x1f]);
        h.update(text.as_bytes());
        h.finalize().iter().map(|b| format!("{:02x}", b)).collect()
    }

    /// Returns the cached translation for `key`, or `None` on miss / read error.
    pub fn get(&self, key: &str) -> Option<String> {
        fs::read_to_string(self.path_of(key)).ok()
    }

    /// Write `val` under `key`, best-effort. Atomic via tmp-file + rename within
    /// the same shard directory, so an interrupt can never leave a half-written
    /// file that `get` would later serve as a truncated hit. Errors are logged,
    /// never bubbled — a failed write just means a re-translate later. No
    /// `fsync`: the goal is surviving Ctrl-C (page cache survives process
    /// exit), not power loss, and fsync per block would dominate a large book.
    pub fn put(&self, key: &str, val: &str) {
        let final_path = self.path_of(key);
        let shard = final_path.parent().unwrap_or_else(|| Path::new("."));
        if let Err(e) = fs::create_dir_all(shard) {
            eprintln!("warn: cache mkdir {:?} failed: {}", shard, e);
            return;
        }
        let ctr = self.counter.fetch_add(1, Ordering::Relaxed);
        let tmp = shard.join(format!(".{}.{}.tmp", key, ctr));
        if let Err(e) = fs::write(&tmp, val).and_then(|_| fs::rename(&tmp, &final_path)) {
            eprintln!("warn: cache write {:?} failed: {}", final_path, e);
            let _ = fs::remove_file(&tmp);
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

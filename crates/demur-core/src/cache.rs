//! The resume cache: completed pass outputs, stored so a retried run does
//! not pay twice for work an earlier attempt finished.
//!
//! The cache is cost-bearing and nothing else. Every failure path in this
//! module falls back to running the pass, so an absent, unreadable, or
//! corrupt cache produces exactly the review a cold run produces. Nothing
//! here is ever consulted for review scope, dismissal, or a verdict, all
//! of which continue to come from the bot's own review markers.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::config::ModelDef;
use crate::provider::{CompletionRequest, TokenUsage};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Layout version of a stored entry. An entry written under a different
/// version is discarded rather than deserialized loosely.
const ENTRY_VERSION: u32 = 1;

/// The digest identifying one pass request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheKey(String);

impl CacheKey {
    /// Digest everything that determines what the pass would send. Two
    /// runs share a key only when they would ask the provider the same
    /// question of the same model, so a cached answer can never stand in
    /// for a question nobody asked.
    ///
    /// The head commit is covered because it is rendered into the prompt,
    /// and a degraded pass is covered because it renders the context it
    /// actually sent.
    pub fn new(request: &CompletionRequest, model: &ModelDef) -> CacheKey {
        let mut hasher = Sha256::new();
        for part in [
            request.system.as_str(),
            request.user.as_str(),
            request.schema_name.as_str(),
            &request.schema.to_string(),
            &request.max_output_tokens.to_string(),
            model.provider.as_str(),
            model.name.as_str(),
            &format!("{:?}", model.reasoning_effort),
            &format!("{:?}", model.thinking_budget),
            &format!("{:?}", model.extra_body),
        ] {
            hasher.update(part.as_bytes());
            // Length prefixing keeps two different splits of the same
            // bytes from colliding.
            hasher.update(part.len().to_le_bytes());
        }
        let digest = hasher.finalize();
        let mut hex = String::with_capacity(digest.len() * 2);
        for byte in digest {
            use std::fmt::Write;
            let _ = write!(hex, "{byte:02x}");
        }
        CacheKey(hex)
    }

    /// The key as a filename-safe string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A completed pass, stored. It holds the output and what the original
/// call cost, and deliberately nothing else: no verdict, no configuration,
/// no spend decision.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    version: u32,
    /// Parsed pass output, revalidated against the pass schema on use.
    pub content: serde_json::Value,
    /// Input tokens the original call was billed for.
    pub input_tokens: u64,
    /// Cached input tokens the original call reported.
    pub cached_input_tokens: u64,
    /// Output tokens the original call was billed for.
    pub output_tokens: u64,
    /// What the original call said it cost, when the transport reports a
    /// figure of its own. A resumed pass is priced as the first one was.
    #[serde(default)]
    pub reported_cost: Option<f64>,
    /// The model that actually answered, when the transport names one.
    /// The configured name is already in the key; this is who showed up.
    #[serde(default)]
    pub observed_model: Option<String>,
}

impl Entry {
    /// Build an entry for a completed pass.
    pub fn new(
        content: serde_json::Value,
        usage: &TokenUsage,
        reported_cost: Option<f64>,
        observed_model: Option<String>,
    ) -> Entry {
        Entry {
            version: ENTRY_VERSION,
            content,
            input_tokens: usage.input_tokens,
            cached_input_tokens: usage.cached_input_tokens,
            output_tokens: usage.output_tokens,
            reported_cost,
            observed_model,
        }
    }

    /// Usage the original call reported, so a resumed run can say what the
    /// work cost when it was actually paid for.
    pub fn usage(&self) -> TokenUsage {
        TokenUsage {
            input_tokens: self.input_tokens,
            cached_input_tokens: self.cached_input_tokens,
            output_tokens: self.output_tokens,
        }
    }
}

/// Somewhere completed passes can be kept between attempts.
pub trait Store: Send + Sync {
    /// Retrieve an entry. Any failure reads as absent, because the caller
    /// can always run the pass.
    fn get(&self, key: &CacheKey) -> Option<Entry>;
    /// Store an entry. Any failure is ignored for the same reason.
    fn put(&self, key: &CacheKey, entry: &Entry);
}

/// A cache on disk, bounded by age and by total size.
pub struct FsStore {
    dir: PathBuf,
    max_age: Duration,
    max_bytes: u64,
}

impl FsStore {
    /// Open a cache directory, creating it when it does not exist. A
    /// directory that cannot be created yields no store rather than an
    /// error, because a cache is never worth failing a run over.
    pub fn open(dir: &Path, max_age: Duration, max_bytes: u64) -> Option<FsStore> {
        std::fs::create_dir_all(dir).ok()?;
        Some(FsStore {
            dir: dir.to_path_buf(),
            max_age,
            max_bytes,
        })
    }

    fn path(&self, key: &CacheKey) -> PathBuf {
        self.dir.join(format!("{}.json", key.as_str()))
    }

    /// Drop entries past the age bound, then oldest-first until the size
    /// bound is met. Eviction is invisible in every respect except cost.
    fn evict(&self) {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return;
        };
        let now = SystemTime::now();
        let mut kept: Vec<(SystemTime, u64, PathBuf)> = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = entry.metadata() else { continue };
            if !meta.is_file() {
                continue;
            }
            let modified = meta.modified().unwrap_or(now);
            if now
                .duration_since(modified)
                .is_ok_and(|age| age > self.max_age)
            {
                let _ = std::fs::remove_file(&path);
                continue;
            }
            kept.push((modified, meta.len(), path));
        }
        let mut total: u64 = kept.iter().map(|(_, size, _)| size).sum();
        if total <= self.max_bytes {
            return;
        }
        kept.sort_by_key(|(modified, _, _)| *modified);
        for (_, size, path) in kept {
            if total <= self.max_bytes {
                break;
            }
            if std::fs::remove_file(&path).is_ok() {
                total = total.saturating_sub(size);
            }
        }
    }
}

impl Store for FsStore {
    fn get(&self, key: &CacheKey) -> Option<Entry> {
        let text = std::fs::read_to_string(self.path(key)).ok()?;
        let entry: Entry = serde_json::from_str(&text).ok()?;
        if entry.version != ENTRY_VERSION {
            return None;
        }
        Some(entry)
    }

    fn put(&self, key: &CacheKey, entry: &Entry) {
        let Ok(text) = serde_json::to_string(entry) else {
            return;
        };
        // Write beside the target and rename, so a run killed mid-write
        // leaves no half-written entry for the next one to read.
        let target = self.path(key);
        let temporary = target.with_extension("tmp");
        if std::fs::write(&temporary, text).is_ok() {
            let _ = std::fs::rename(&temporary, &target);
        }
        self.evict();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ModelDef;
    use serde_json::json;

    fn model() -> ModelDef {
        ModelDef {
            provider: "p".to_string(),
            name: "m".to_string(),
            input_price: 1.0,
            output_price: 2.0,
            reasoning_effort: None,
            thinking_budget: None,
            effort: None,
            extra_body: None,
            extra_headers: None,
            cached_input_price: None,
        }
    }

    fn request() -> CompletionRequest {
        CompletionRequest {
            system: "rules".to_string(),
            user: "Head commit: abc\n<pull_request_data>diff</pull_request_data>".to_string(),
            schema: json!({"type": "object"}),
            schema_name: "deep dive".to_string(),
            max_output_tokens: 2000,
        }
    }

    fn usage() -> TokenUsage {
        TokenUsage {
            input_tokens: 100,
            cached_input_tokens: 10,
            output_tokens: 20,
        }
    }

    fn entry(content: serde_json::Value) -> Entry {
        Entry::new(content, &usage(), None, None)
    }

    #[test]
    fn an_unchanged_request_reproduces_its_key() {
        assert_eq!(
            CacheKey::new(&request(), &model()),
            CacheKey::new(&request(), &model())
        );
    }

    #[test]
    fn a_changed_commit_changes_the_key() {
        // The head commit is rendered into the prompt, so a new commit
        // cannot reuse the old commit's answers.
        let mut other = request();
        other.user = other.user.replace("abc", "def");
        assert_ne!(
            CacheKey::new(&request(), &model()),
            CacheKey::new(&other, &model())
        );
    }

    #[test]
    fn a_changed_model_changes_the_key() {
        let mut other = model();
        other.name = "other-model".to_string();
        assert_ne!(
            CacheKey::new(&request(), &model()),
            CacheKey::new(&request(), &other)
        );
    }

    #[test]
    fn changed_model_parameters_change_the_key() {
        for mutate in [
            |m: &mut ModelDef| m.thinking_budget = Some(8000),
            |m: &mut ModelDef| m.reasoning_effort = Some("high".to_string()),
            |m: &mut ModelDef| m.provider = "other".to_string(),
        ] {
            let mut other = model();
            mutate(&mut other);
            assert_ne!(
                CacheKey::new(&request(), &model()),
                CacheKey::new(&request(), &other)
            );
        }
    }

    #[test]
    fn a_changed_pass_or_ceiling_changes_the_key() {
        let mut lens = request();
        lens.schema_name = "cross-examination".to_string();
        assert_ne!(
            CacheKey::new(&request(), &model()),
            CacheKey::new(&lens, &model())
        );
        let mut ceiling = request();
        ceiling.max_output_tokens = 8000;
        assert_ne!(
            CacheKey::new(&request(), &model()),
            CacheKey::new(&ceiling, &model())
        );
    }

    #[test]
    fn a_degraded_context_changes_the_key() {
        // A shrunk pass answers a smaller question, so a full-budget run
        // must not reuse it as if it were full coverage.
        let mut shrunk = request();
        shrunk.user =
            "Head commit: abc\n<pull_request_data>one hunk</pull_request_data>".to_string();
        assert_ne!(
            CacheKey::new(&request(), &model()),
            CacheKey::new(&shrunk, &model())
        );
    }

    #[test]
    fn entries_round_trip_through_the_store() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsStore::open(dir.path(), Duration::from_secs(3600), 1 << 20).unwrap();
        let key = CacheKey::new(&request(), &model());
        assert!(store.get(&key).is_none());
        store.put(
            &key,
            &Entry::new(
                json!({"findings": []}),
                &usage(),
                Some(0.0123),
                Some("claude-haiku".to_string()),
            ),
        );
        let found = store.get(&key).expect("entry round trips");
        assert_eq!(found.content, json!({"findings": []}));
        assert_eq!(found.usage(), usage());
        assert_eq!(found.reported_cost, Some(0.0123));
        assert_eq!(found.observed_model.as_deref(), Some("claude-haiku"));
    }

    #[test]
    fn an_entry_without_attribution_still_reads() {
        // Entries written before the cost and model fields existed must
        // keep resuming; they simply report no agent price.
        let dir = tempfile::tempdir().unwrap();
        let store = FsStore::open(dir.path(), Duration::from_secs(3600), 1 << 20).unwrap();
        let key = CacheKey::new(&request(), &model());
        std::fs::write(
            dir.path().join(format!("{}.json", key.as_str())),
            json!({"version": 1, "content": {}, "input_tokens": 1,
                   "cached_input_tokens": 0, "output_tokens": 2})
            .to_string(),
        )
        .unwrap();
        let found = store.get(&key).expect("an old entry still resumes");
        assert_eq!(found.reported_cost, None);
        assert_eq!(found.observed_model, None);
    }

    #[test]
    fn a_corrupt_entry_reads_as_absent() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsStore::open(dir.path(), Duration::from_secs(3600), 1 << 20).unwrap();
        let key = CacheKey::new(&request(), &model());
        std::fs::write(
            dir.path().join(format!("{}.json", key.as_str())),
            "{not json",
        )
        .unwrap();
        assert!(store.get(&key).is_none());
    }

    #[test]
    fn an_entry_of_another_layout_reads_as_absent() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsStore::open(dir.path(), Duration::from_secs(3600), 1 << 20).unwrap();
        let key = CacheKey::new(&request(), &model());
        std::fs::write(
            dir.path().join(format!("{}.json", key.as_str())),
            json!({"version": 999, "content": {}, "input_tokens": 0,
                   "cached_input_tokens": 0, "output_tokens": 0})
            .to_string(),
        )
        .unwrap();
        assert!(store.get(&key).is_none());
    }

    #[test]
    fn an_unwritable_location_yields_no_store() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a-file");
        std::fs::write(&file, "not a directory").unwrap();
        assert!(FsStore::open(&file.join("under"), Duration::from_secs(1), 1).is_none());
    }

    #[test]
    fn entries_past_the_age_bound_are_evicted() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsStore::open(dir.path(), Duration::from_secs(0), 1 << 20).unwrap();
        let key = CacheKey::new(&request(), &model());
        store.put(&key, &entry(json!({})));
        // Writing again evicts everything already past the age bound.
        let mut other = request();
        other.user = "different".to_string();
        store.put(&CacheKey::new(&other, &model()), &entry(json!({})));
        assert!(store.get(&key).is_none());
    }

    #[test]
    fn the_size_bound_evicts_oldest_first() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsStore::open(dir.path(), Duration::from_secs(3600), 1).unwrap();
        let first = CacheKey::new(&request(), &model());
        store.put(&first, &entry(json!({"a": 1})));
        let mut other = request();
        other.user = "different".to_string();
        store.put(&CacheKey::new(&other, &model()), &entry(json!({"b": 2})));
        assert!(store.get(&first).is_none());
    }
}

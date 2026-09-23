//! Heuristic risk scoring and hunk clustering over parsed file diffs.

use crate::config::Config;
use crate::diff::{FileDiff, is_ignored};

/// A review cluster: the surviving hunks of one file with a risk score.
#[derive(Debug, Clone)]
pub struct Cluster {
    /// Repository-relative path of the file.
    pub path: String,
    /// Hunks in file order.
    pub hunks: Vec<crate::diff::Hunk>,
    /// Heuristic risk in 0.0 to 1.0.
    pub risk: f64,
    /// True when the diff adds this file.
    pub is_new_file: bool,
    /// True when the diff deletes this file.
    pub is_deleted: bool,
    /// Path before the change, when renamed.
    pub old_path: Option<String>,
}

/// Why a file was excluded from review.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExclusionReason {
    /// The path matches a configured ignore glob.
    IgnoredPath,
    /// The file is a dependency lockfile.
    Lockfile,
    /// The file looks machine generated.
    Generated,
    /// Every changed line is whitespace only.
    WhitespaceOnly,
    /// The file is binary and cannot be reviewed.
    Binary,
}

/// A file excluded from review.
#[derive(Debug, Clone)]
pub struct ExcludedFile {
    /// Repository-relative path of the file.
    pub path: String,
    /// Why the file was excluded.
    pub reason: ExclusionReason,
}

/// The result of ingestion: clusters in risk order, then exclusions.
#[derive(Debug, Clone)]
pub struct Ingestion {
    /// Clusters sorted by risk, highest first.
    pub clusters: Vec<Cluster>,
    /// Files that will not be reviewed, in input order.
    pub excluded: Vec<ExcludedFile>,
}

impl Ingestion {
    /// The highest risk in the ingestion, or 0.0 when nothing survived.
    pub fn max_risk(&self) -> f64 {
        self.clusters.first().map_or(0.0, |cluster| cluster.risk)
    }
}

const SECURITY_MARKERS: &[&str] = &[
    "auth",
    "token",
    "secret",
    "credential",
    "password",
    "crypto",
    "jwt",
    "oauth",
    "session",
    "permission",
    "login",
    "security",
    "keyring",
    "vault",
];

const MANIFESTS: &[&str] = &[
    "Cargo.toml",
    "package.json",
    "pyproject.toml",
    "requirements.txt",
    "go.mod",
    "Gemfile",
    "pom.xml",
    "build.gradle",
    "build.gradle.kts",
    "composer.json",
    "mix.exs",
    "pubspec.yaml",
    "deno.json",
];

const LOCKFILES: &[&str] = &[
    "Cargo.lock",
    "package-lock.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "bun.lockb",
    "go.sum",
    "poetry.lock",
    "Gemfile.lock",
    "Pipfile.lock",
    "composer.lock",
    "deno.lock",
];

const GENERATED_MARKERS: &[&str] = &[
    "/generated/",
    "/vendor/",
    "/node_modules/",
    "/dist/",
    "/target/",
    ".min.",
    "_pb2.py",
    ".pb.go",
];

/// Ingest file diffs: filter to reviewable clusters in risk order.
pub fn ingest(files: &[FileDiff], config: &Config) -> Ingestion {
    let ignore_set = crate::diff::build_ignore_set(&config.ignore.paths);
    let mut clusters = Vec::new();
    let mut excluded = Vec::new();
    for file in files {
        if is_ignored(&ignore_set, &file.path) {
            excluded.push(ExcludedFile {
                path: file.path.clone(),
                reason: ExclusionReason::IgnoredPath,
            });
            continue;
        }
        let name = file
            .path
            .rsplit('/')
            .next()
            .unwrap_or(&file.path)
            .to_string();
        if LOCKFILES.contains(&name.as_str()) {
            excluded.push(ExcludedFile {
                path: file.path.clone(),
                reason: ExclusionReason::Lockfile,
            });
            continue;
        }
        if GENERATED_MARKERS
            .iter()
            .any(|marker| file.path.contains(marker))
        {
            excluded.push(ExcludedFile {
                path: file.path.clone(),
                reason: ExclusionReason::Generated,
            });
            continue;
        }
        if file.is_binary {
            excluded.push(ExcludedFile {
                path: file.path.clone(),
                reason: ExclusionReason::Binary,
            });
            continue;
        }
        let hunks: Vec<_> = file
            .hunks
            .iter()
            .filter(|hunk| !hunk.is_whitespace_only())
            .cloned()
            .collect();
        if hunks.is_empty() {
            excluded.push(ExcludedFile {
                path: file.path.clone(),
                reason: ExclusionReason::WhitespaceOnly,
            });
            continue;
        }
        clusters.push(Cluster {
            path: file.path.clone(),
            hunks,
            risk: risk_score(file, &name),
            is_new_file: file.is_new,
            is_deleted: file.is_deleted,
            old_path: file.old_path.clone(),
        });
    }
    clusters.sort_by(|a, b| {
        b.risk
            .partial_cmp(&a.risk)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Ingestion { clusters, excluded }
}

fn risk_score(file: &FileDiff, name: &str) -> f64 {
    if file.is_new {
        return 0.9;
    }
    if MANIFESTS.contains(&name) {
        return 0.8;
    }
    let lowered = file.path.to_lowercase();
    let security = SECURITY_MARKERS
        .iter()
        .any(|marker| lowered.contains(marker));
    let churn: u32 = file
        .hunks
        .iter()
        .map(|hunk| hunk.added() + hunk.removed())
        .sum();
    let churn_score = (churn as f64 / 200.0).min(0.4);
    let base = if is_test_path(&lowered) { 0.25 } else { 0.4 };
    (base + churn_score + if security { 0.3 } else { 0.0 }).min(1.0)
}

fn is_test_path(path: &str) -> bool {
    path.contains("/tests/")
        || path.contains("/test/")
        || path.ends_with("_test.go")
        || path.ends_with("_test.rs")
        || path.ends_with(".test.ts")
        || path.ends_with(".test.tsx")
        || path.ends_with(".spec.js")
        || path.contains("test_")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::parse_unified_diff;

    fn config() -> Config {
        Config::from_toml(crate::config::MINIMAL_EXAMPLE).unwrap()
    }

    fn diff_entry(header: &str, body: &str) -> String {
        format!(
            "diff --git a/{header} b/{header}\n--- a/{header}\n+++ b/{header}\n@@ -1,2 +1,3 @@\n context\n{body}"
        )
    }

    fn new_file_entry(header: &str, body: &str) -> String {
        format!(
            "diff --git a/{header} b/{header}\nnew file mode 100644\n--- /dev/null\n+++ b/{header}\n@@ -0,0 +1,1 @@\n{body}"
        )
    }

    #[test]
    fn lockfiles_drop_and_manifests_stay() {
        let diff = format!(
            "{}\n{}",
            diff_entry("Cargo.lock", "+name = \"x\""),
            diff_entry("Cargo.toml", "+demur = \"0.1\"")
        );
        let result = ingest(&parse_unified_diff(&diff), &config());
        assert_eq!(result.clusters.len(), 1);
        assert_eq!(result.clusters[0].path, "Cargo.toml");
        assert_eq!(result.excluded.len(), 1);
        assert_eq!(result.excluded[0].reason, ExclusionReason::Lockfile);
    }

    #[test]
    fn new_files_rank_above_edited_tests() {
        let diff = format!(
            "{}\n{}",
            diff_entry("src/tests/util.rs", "+fn helper() {}"),
            new_file_entry("src/new_feature.rs", "+fn feature() {}")
        );
        let files = parse_unified_diff(&diff);
        let result = ingest(&files, &config());
        assert_eq!(result.clusters.len(), 2);
        assert_eq!(result.clusters[0].path, "src/new_feature.rs");
        assert_eq!(result.clusters[0].risk, 0.9);
        assert!(result.clusters[0].is_new_file);
        assert!(result.clusters[1].risk < 0.9);
    }

    #[test]
    fn security_paths_outrank_plain_paths() {
        let diff = format!(
            "{}\n{}",
            diff_entry("src/handler.rs", "+let x = 1;"),
            diff_entry("src/session.rs", "+let y = 2;")
        );
        let result = ingest(&parse_unified_diff(&diff), &config());
        assert_eq!(result.clusters[0].path, "src/session.rs");
        assert!(result.clusters[0].risk > result.clusters[1].risk);
    }

    #[test]
    fn generated_and_whitespace_only_content_is_triaged_out() {
        let diff = format!(
            "{}\n{}",
            diff_entry("src/generated/api.rs", "+fn gen() {}"),
            diff_entry("src/spacing.rs", "+   \n-   ")
        );
        let result = ingest(&parse_unified_diff(&diff), &config());
        assert!(result.clusters.is_empty());
        assert_eq!(result.excluded[0].reason, ExclusionReason::Generated);
        assert_eq!(result.excluded[1].reason, ExclusionReason::WhitespaceOnly);
    }

    #[test]
    fn configured_ignore_paths_exclude_before_scoring() {
        let text = format!(
            r#"[ignore]
paths = ["vendor/**"]

{}"#,
            crate::config::MINIMAL_EXAMPLE
        );
        let config = Config::from_toml(&text).unwrap();
        let diff = diff_entry("vendor/lib/lib.rs", "+fn vendored() {}");
        let result = ingest(&parse_unified_diff(&diff), &config);
        assert_eq!(result.excluded[0].reason, ExclusionReason::IgnoredPath);
    }

    #[test]
    fn clusters_are_sorted_by_risk_descending() {
        let diff = format!(
            "{}\n{}\n{}",
            diff_entry("src/plain.rs", "+let a = 1;"),
            diff_entry("src/auth/token.rs", "+let b = 2;"),
            new_file_entry("brand_new.rs", "+let c = 3;")
        );
        let result = ingest(&parse_unified_diff(&diff), &config());
        let risks: Vec<f64> = result.clusters.iter().map(|c| c.risk).collect();
        assert_eq!(result.clusters[0].path, "brand_new.rs");
        assert!(risks[0] >= risks[1] && risks[1] >= risks[2]);
    }
}

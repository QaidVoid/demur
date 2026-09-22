//! GitHub Action binary for demur, a BYOK code review bot.
//!
//! Reads the job environment, runs the review pipeline over the pull
//! request, and publishes one review event plus one check run.

use demur_core::config::CONFIG_FILE_NAME;
use demur_core::config::Config;
use demur_core::github::{FlowOutcome, GitHubClient, review_pull_request};
use demur_core::provider::ProviderRegistry;
use serde::Deserialize;
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

/// Exit status for a job that ran to completion, whatever the verdict.
const EXIT_OK: u8 = 0;
/// Exit status for a job that failed before it could complete a review.
const EXIT_FAILED: u8 = 1;

/// The pull request fields of the job's event payload.
#[derive(Debug, Deserialize)]
struct EventPayload {
    /// The action name of the event, like opened or synchronize.
    action: Option<String>,
    /// Present on pull request events, absent on push events.
    pull_request: Option<PullRequestPayload>,
}

/// Pull request data from the event payload.
#[derive(Debug, Deserialize)]
struct PullRequestPayload {
    /// Pull request number.
    number: u64,
    /// True while the pull request is a draft.
    draft: bool,
    /// Head commit info.
    head: HeadPayload,
}

/// Head commit info from the event payload.
#[derive(Debug, Deserialize)]
struct HeadPayload {
    /// Head commit SHA, informational: the flow reads the SHA from the
    /// API so a mid-run push is detected against fetched state.
    #[serde(default)]
    #[allow(dead_code)]
    sha: String,
    /// The head repository, whose fork flag decides the no-key notice path.
    #[serde(default)]
    repo: Option<HeadRepo>,
}

/// The head repository.
#[derive(Debug, Deserialize)]
struct HeadRepo {
    /// True when the head is a fork of the base repository.
    #[serde(default)]
    fork: bool,
}

/// Decide whether the event triggers a review.
pub fn should_review(action: Option<&str>, draft: bool, has_pull_request: bool) -> bool {
    if !has_pull_request || draft {
        return false;
    }
    matches!(action, Some("opened" | "synchronize" | "ready_for_review"))
}

#[tokio::main]
async fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_secs()
        .init();
    match run().await {
        Ok(()) => ExitCode::from(EXIT_OK),
        Err(err) => {
            eprintln!("{err}");
            ExitCode::from(EXIT_FAILED)
        }
    }
}

async fn run() -> Result<(), String> {
    let event_path = std::env::var("GITHUB_EVENT_PATH").map_err(|_| {
        "GITHUB_EVENT_PATH is not set: this binary runs inside a GitHub Actions job".to_string()
    })?;
    let payload: EventPayload = serde_json::from_str(
        &std::fs::read_to_string(&event_path).map_err(|err| format!("{event_path}: {err}"))?,
    )
    .map_err(|err| format!("{event_path}: {err}"))?;

    let pr = payload.pull_request.as_ref();
    if !should_review(
        payload.action.as_deref(),
        pr.is_some_and(|pr| pr.draft),
        pr.is_some(),
    ) {
        return Ok(());
    }
    let pr = pr.expect("checked above");

    let repository = std::env::var("GITHUB_REPOSITORY").unwrap_or_default();
    let (owner, repo) = repository
        .split_once('/')
        .ok_or_else(|| format!("GITHUB_REPOSITORY must be owner/repo, got `{repository}`"))?;
    let token = std::env::var("GITHUB_TOKEN")
        .or_else(|_| std::env::var("INPUT_GITHUB_TOKEN"))
        .map_err(|_| "no GitHub token: set the github_token input".to_string())?;

    let workspace = std::env::var("GITHUB_WORKSPACE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."));
    let mut config =
        Config::load(&workspace.join(CONFIG_FILE_NAME)).map_err(|err| err.to_string())?;
    if let Ok(profile) = std::env::var("DEMUR_PROFILE")
        && !profile.is_empty()
    {
        config.profile = Some(
            profile
                .parse()
                .map_err(|err| format!("DEMUR_PROFILE: invalid profile `{profile}`: {err}"))?,
        );
    }

    // The workflow decides whether the runner keeps a cache between job
    // attempts, and points the binary at it. Nothing is cached unless it
    // does.
    if let Ok(dir) = std::env::var("DEMUR_CACHE_DIR")
        && !dir.trim().is_empty()
    {
        config.cache.enabled = true;
        config.cache.dir = Some(std::path::PathBuf::from(dir.trim()));
    }

    // The fork path: without a provider key there is nothing this job can
    // do, so it explains itself in the job summary and exits successfully
    // without any provider or review API call. A head whose origin cannot
    // be classified counts as untrusted for every guard below, never as
    // trusted.
    let head_fork = pr.head.repo.as_ref().is_some_and(|head| head.fork);
    let untrusted_head = pr.head.repo.as_ref().is_none_or(|head| head.fork);

    if untrusted_head && config.selects_agent_family() {
        // The keyless family needs no key to be dangerous: on an untrusted
        // head its subprocess would act on input written outside the
        // repository, so it is refused here before the registry is even
        // built.
        write_summary(&fork_family_notice())?;
        return Ok(());
    }

    if untrusted_head && config.cache.disable_for_untrusted_head() {
        eprintln!("untrusted pull request head: the resume cache is not read");
    }
    // What a pass asks to retrieve is shaped by the diff it read, and on an
    // untrusted head that diff was written by someone outside the
    // repository.
    if untrusted_head && config.retrieval.disable_for_untrusted_head() {
        eprintln!("untrusted pull request head: context retrieval is not performed");
    }
    let registry = match ProviderRegistry::from_config(&config) {
        Ok(registry) => registry,
        Err(key_error) => {
            if head_fork {
                write_summary(&fork_notice())?;
                return Ok(());
            }
            return Err(key_error.to_string());
        }
    };

    let client = GitHubClient::new(&github_api_url(), token, owner, repo);
    let outcome: FlowOutcome = review_pull_request(&client, &registry, &config, pr.number)
        .await
        .map_err(|err| err.to_string())?;
    write_summary(&outcome.summary)?;
    Ok(())
}

fn github_api_url() -> String {
    std::env::var("GITHUB_API_URL").unwrap_or_else(|_| "https://api.github.com".to_string())
}

/// The notice for fork pull requests that cannot read the provider key.
pub fn fork_notice() -> String {
    "## demur: review skipped\n\n\
This pull request comes from a fork, so the job cannot read the provider \
key and no review was attempted.\n\n\
To review fork pull requests, a maintainer can run demur locally against \
the pull request, or configure a self-hosted runner where the key is \
available. The `pull_request_target` workaround is not recommended: \
checking out the pull request head under that event executes untrusted \
code with repository secrets in scope."
        .to_string()
}

/// The notice for the keyless agent family on an untrusted head. No key
/// is involved, so the reason is different: the subprocess carries its
/// own login.
pub fn fork_family_notice() -> String {
    "## demur: review skipped\n\n\
The head of this pull request is a fork, or its origin cannot be \
identified, and this configuration names the claude-code family. The \
review would run a subprocess holding its own credentials on untrusted \
input, which the family refuses regardless of keys, so no review was \
attempted.\n\n\
To review such pull requests, a maintainer can run demur locally \
against the pull request, or configure a self-hosted runner. The \
`pull_request_target` workaround is not recommended: checking out the \
pull request head under that event executes untrusted code with \
repository secrets in scope."
        .to_string()
}

fn write_summary(text: &str) -> Result<(), String> {
    if let Ok(path) = std::env::var("GITHUB_STEP_SUMMARY") {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|err| format!("{path}: {err}"))?;
        file.write_all(format!("{text}\n").as_bytes())
            .map_err(|err| format!("{path}: {err}"))?;
    }
    println!("{text}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trigger_matrix_matches_the_spec() {
        // Opened, synchronize, and ready_for_review trigger; drafts do not.
        assert!(should_review(Some("opened"), false, true));
        assert!(should_review(Some("synchronize"), false, true));
        assert!(should_review(Some("ready_for_review"), false, true));
        assert!(!should_review(Some("opened"), true, true));
        assert!(!should_review(Some("synchronize"), true, true));
        // Everything else skips.
        assert!(!should_review(Some("closed"), false, true));
        assert!(!should_review(Some("labeled"), false, true));
        assert!(!should_review(None, false, true));
        // Non pull request events skip.
        assert!(!should_review(Some("opened"), false, false));
    }

    #[test]
    fn fork_notice_explains_the_limitation_and_the_danger() {
        let notice = fork_notice();
        assert!(notice.contains("review skipped"));
        assert!(notice.contains("fork"));
        assert!(notice.contains("untrusted code"));
        assert!(notice.contains("not recommended"));
    }
}

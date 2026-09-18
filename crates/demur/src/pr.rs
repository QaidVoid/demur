//! Pull request review support for the CLI: fetch a GitHub pull request by
//! number or URL, review it with the same pipeline, and publish only when
//! explicitly asked.

use demur_core::config::CONFIG_FILE_NAME;
use demur_core::config::Config;
use demur_core::diff::parse_unified_diff;
use demur_core::github::{GitHubClient, publish_review};
use demur_core::ingest::ingest;
use demur_core::pipeline::prompt::PullRequestMeta;
use demur_core::pipeline::synthesis::Verdict;
use demur_core::pipeline::{PipelineInput, RunOutcome};
use demur_core::provider::ProviderRegistry;
use std::path::{Path, PathBuf};

/// Where a review came from: the owner, repo name, and pull request
/// number.
struct Target {
    owner: String,
    repo: String,
    number: u64,
}

/// Parse a pull request number or URL. A bare number resolves the
/// repository from the checkout's git remote.
fn parse_target(target: &str, repo: &Path) -> Result<Target, String> {
    if let Some(rest) = target
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .strip_prefix("github.com/")
    {
        let mut parts = rest.trim_end_matches(".git").split('/');
        let owner = parts.next().unwrap_or_default().to_string();
        let name = parts.next().unwrap_or_default().to_string();
        let number = parts
            .nth(1)
            .and_then(|n| n.parse::<u64>().ok())
            .ok_or_else(|| format!("not a pull request URL: {target}"))?;
        if owner.is_empty() || name.is_empty() {
            return Err(format!("not a pull request URL: {target}"));
        }
        return Ok(Target {
            owner,
            repo: name,
            number,
        });
    }
    let number: u64 = target
        .parse()
        .map_err(|_| format!("target must be a pull request number or URL, got `{target}`"))?;
    let remote = git_remote_url(repo)?;
    let cleaned = remote
        .replace("https://", "")
        .replace("git@", "")
        .trim_end_matches(".git")
        .to_string();
    let segments: Vec<&str> = cleaned
        .split(['/', ':'])
        .filter(|part| !part.is_empty())
        .collect();
    if segments.len() < 2 {
        return Err(format!("cannot read owner and repo from remote `{remote}`"));
    }
    Ok(Target {
        owner: segments[segments.len() - 2].to_string(),
        repo: segments[segments.len() - 1].to_string(),
        number,
    })
}

fn github_token() -> Result<String, String> {
    const GUIDANCE: &str =
        "no GitHub token: set GITHUB_TOKEN or GH_TOKEN, or authenticate with `gh auth login`";
    for name in ["GITHUB_TOKEN", "GH_TOKEN"] {
        if let Ok(token) = std::env::var(name) {
            let trimmed = token.trim();
            if !trimmed.is_empty() {
                return Ok(trimmed.to_string());
            }
        }
    }
    let output = std::process::Command::new("gh")
        .args(["auth", "token"])
        .output()
        .map_err(|_| GUIDANCE.to_string())?;
    if !output.status.success() {
        return Err(GUIDANCE.to_string());
    }
    let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if token.is_empty() {
        return Err(GUIDANCE.to_string());
    }
    Ok(token)
}

fn git_remote_url(repo: &Path) -> Result<String, String> {
    let output = std::process::Command::new("git")
        .args(["config", "--get", "remote.origin.url"])
        .current_dir(repo)
        .output()
        .map_err(|err| format!("cannot run git: {err}"))?;
    if !output.status.success() {
        return Err("no git remote `origin`: use a pull request URL instead".to_string());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Review a pull request. Read-only by default; publishing requires the
/// explicit flag and posts under the token's human identity.
pub async fn review_pr(
    target: String,
    repo: Option<PathBuf>,
    format: crate::Format,
    publish: bool,
) -> Result<u8, String> {
    let repo =
        repo.unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let parsed = parse_target(&target, &repo)?;
    let token = github_token()?;
    let api =
        std::env::var("GITHUB_API_URL").unwrap_or_else(|_| "https://api.github.com".to_string());
    let client = GitHubClient::new(&api, token, &parsed.owner, &parsed.repo);

    let config = Config::load(&repo.join(CONFIG_FILE_NAME)).map_err(|err| err.to_string())?;
    let registry = ProviderRegistry::from_config(&config).map_err(|err| err.to_string())?;

    let pr = client
        .pull_request(parsed.number)
        .await
        .map_err(|e| e.to_string())?;
    let diff = client
        .pull_request_diff(parsed.number)
        .await
        .map_err(|e| e.to_string())?;
    let files = parse_unified_diff(&diff);
    let pipeline_input = PipelineInput {
        meta: PullRequestMeta {
            title: format!("pull request #{}", parsed.number),
            description: String::new(),
            head_sha: pr.head_sha().to_string(),
        },
        ingestion: ingest(&files, &config),
        diff_text: diff,
        prior_spend: 0.0,
        carried_findings: Vec::new(),
        suppress_fingerprints: std::collections::HashSet::new(),
    };

    match demur_core::pipeline::run(&registry, &config, &pipeline_input).await {
        Ok(RunOutcome::Review(review)) => {
            match format {
                crate::Format::Markdown => println!("{}", review.body),
                crate::Format::Json => println!(
                    "{}",
                    serde_json::to_string_pretty(&crate::output::json_review(&review))
                        .map_err(|err| err.to_string())?
                ),
            }
            if publish {
                let login = client
                    .authenticated_login()
                    .await
                    .map_err(|err| err.to_string())?;
                println!(
                    "Publishing under your identity @{login}: findings will appear under \
your name, not under a bot identity."
                );
                publish_review(
                    &client,
                    parsed.number,
                    pr.head_sha(),
                    review.verdict,
                    &review.body,
                    None,
                    &[],
                )
                .await
                .map_err(|err| err.to_string())?;
            }
            Ok(match review.verdict {
                Verdict::Approve => crate::EXIT_APPROVE,
                Verdict::RequestChanges => crate::EXIT_REQUEST_CHANGES,
            })
        }
        Ok(RunOutcome::Skipped(notice)) => {
            eprintln!("{notice}");
            Ok(crate::EXIT_FAILED)
        }
        Err(err) => {
            eprintln!("{err}");
            Ok(crate::EXIT_FAILED)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pull_request_urls() {
        let target = parse_target(
            "https://github.com/owner/repo/pull/42",
            Path::new("/nonexistent"),
        )
        .unwrap();
        assert_eq!(target.owner, "owner");
        assert_eq!(target.repo, "repo");
        assert_eq!(target.number, 42);
    }

    #[test]
    fn rejects_unparseable_targets_without_network() {
        assert!(parse_target("not-a-number", Path::new("/nonexistent")).is_err());
    }

    #[test]
    fn environment_variables_win_over_the_gh_cli() {
        unsafe {
            std::env::set_var("GITHUB_TOKEN", "env-github-token");
            std::env::remove_var("GH_TOKEN");
        }
        assert_eq!(github_token().unwrap(), "env-github-token");

        unsafe {
            std::env::remove_var("GITHUB_TOKEN");
            std::env::set_var("GH_TOKEN", "env-gh-token");
        }
        assert_eq!(github_token().unwrap(), "env-gh-token");

        unsafe {
            std::env::remove_var("GITHUB_TOKEN");
            std::env::remove_var("GH_TOKEN");
        }
    }
}

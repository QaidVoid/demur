//! Pull request review support for the CLI: fetch a GitHub pull request by
//! number or URL, review it with the same pipeline, and publish only when
//! explicitly asked.

use demur_core::config::CONFIG_FILE_NAME;
use demur_core::config::Config;
use demur_core::diff::parse_unified_diff;
use demur_core::github::{GitHubClient, publish_review};
use demur_core::ingest::ingest;
use demur_core::pipeline::prompt::MetaOrigin;
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

/// Which credential a publication will use, and whether a review published
/// with it carries demur's mark.
struct Identity {
    token: String,
    badged: bool,
}

/// Resolve the identity a publication would use. An authorization demur
/// holds is preferred; without one, whatever token the user already has is
/// used, unbadged. Failing to obtain one costs the mark, never the review.
async fn resolve_identity(
    app: &demur_core::config::App,
    publish: bool,
) -> Result<Identity, String> {
    use demur_core::app::{Authorization, Authorizer};

    let Some(client_id) = app.client_id.as_deref().filter(|_| publish) else {
        return Ok(Identity {
            token: github_token()?,
            badged: false,
        });
    };
    let authorizer = Authorizer::new(client_id);
    let kept = app.token_file.as_deref().and_then(Authorization::read);

    // A kept authorization, renewed first if it is close to expiring.
    if let Some(existing) = kept {
        if !existing.needs_renewal() {
            return Ok(Identity {
                token: existing.token,
                badged: true,
            });
        }
        match authorizer.renew(&existing).await {
            Ok(renewed) => {
                keep(app, &renewed);
                return Ok(Identity {
                    token: renewed.token,
                    badged: true,
                });
            }
            Err(err) => {
                // Revoked, or no longer renewable. Ask again rather than
                // failing on a credential that simply aged out.
                eprintln!("the kept authorization could not be renewed ({err}); authorizing again");
            }
        }
    }

    match authorize(&authorizer).await {
        Ok(granted) => {
            keep(app, &granted);
            Ok(Identity {
                token: granted.token,
                badged: true,
            })
        }
        Err(err) => match github_token() {
            Ok(token) => {
                eprintln!(
                    "could not authorize demur ({err}); publishing unbadged under your own token"
                );
                Ok(Identity {
                    token,
                    badged: false,
                })
            }
            Err(_) => Err(err.to_string()),
        },
    }
}

async fn authorize(
    authorizer: &demur_core::app::Authorizer,
) -> Result<demur_core::app::Authorization, demur_core::app::AppError> {
    let (prompt, pending) = authorizer.begin().await?;
    println!(
        "To publish as yourself marked as demur's work, open {} and enter: {}",
        prompt.verification_uri, prompt.user_code
    );
    authorizer.wait(&pending).await
}

/// Keep an authorization only where the user named a place for it. A
/// failure to keep it is not a failure to publish.
fn keep(app: &demur_core::config::App, authorization: &demur_core::app::Authorization) {
    let Some(path) = app.token_file.as_deref() else {
        return;
    };
    if let Err(err) = authorization.write(path) {
        eprintln!(
            "the authorization could not be kept at {}: {err}",
            path.display()
        );
    }
}

/// Review a pull request. Read-only by default; publishing requires the
/// explicit flag and posts under the token's human identity.
pub async fn review_pr(
    target: String,
    repo: Option<PathBuf>,
    format: crate::Format,
    publish: bool,
    cache_dir: Option<PathBuf>,
) -> Result<u8, String> {
    let repo =
        repo.unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let parsed = parse_target(&target, &repo)?;
    let mut config = Config::load(&repo.join(CONFIG_FILE_NAME)).map_err(|err| err.to_string())?;
    // The identity is resolved before anything is fetched, so the run knows
    // how a review would be attributed before it spends anything on one.
    let identity = resolve_identity(&config.app, publish).await?;
    let api =
        std::env::var("GITHUB_API_URL").unwrap_or_else(|_| "https://api.github.com".to_string());
    let client = GitHubClient::new(&api, identity.token.clone(), &parsed.owner, &parsed.repo);

    if let Some(dir) = cache_dir {
        config.cache.enabled = true;
        config.cache.dir = Some(dir);
    }
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
            title: if pr.title.is_empty() {
                format!("pull request #{}", parsed.number)
            } else {
                pr.title.clone()
            },
            description: pr.body.clone().unwrap_or_default(),
            head_sha: pr.head_sha().to_string(),
            origin: MetaOrigin::PullRequest,
        },
        ingestion: ingest(&files, &config),
        diff_text: diff,
        prior_spend: 0.0,
        carried_findings: Vec::new(),
        suppress_fingerprints: std::collections::HashSet::new(),
        repo_root: Some(repo.clone()),
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
                // Naming the identity is courtesy; failing to look it up
                // must not cost a review that is already paid for.
                let login = client.authenticated_login().await.ok();
                let whose = login
                    .as_deref()
                    .map(|login| format!("@{login}"))
                    .unwrap_or_else(|| "your account".to_string());
                if identity.badged {
                    println!(
                        "Publishing as {whose}, marked as demur's work: the review is yours \
and carries demur's mark beside your name."
                    );
                } else {
                    println!(
                        "Publishing under your identity {whose}: findings will appear under \
your name, not under a bot identity, and without demur's mark."
                    );
                }
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
        Ok(RunOutcome::Skipped { notice, .. }) => {
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

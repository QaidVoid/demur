//! Local CLI for demur, a BYOK code review bot.
//!
//! Reviews a pull request or a local revision range with the same core
//! the GitHub Action uses. Read-only by default.

mod input;
mod output;
mod pr;

use clap::Parser;
use clap::Subcommand;
use demur_core::config::CONFIG_FILE_NAME;
use demur_core::config::Config;
use demur_core::diff::parse_unified_diff;
use demur_core::ingest::ingest;
use demur_core::pipeline::prompt::MetaOrigin;
use demur_core::pipeline::prompt::PullRequestMeta;
use demur_core::pipeline::synthesis::Verdict;
use demur_core::pipeline::{PipelineInput, RunOutcome};
use demur_core::provider::ProviderRegistry;
use std::path::PathBuf;
use std::process::ExitCode;

/// Exit status for APPROVE.
pub(crate) const EXIT_APPROVE: u8 = 0;
/// Exit status for REQUEST_CHANGES.
pub(crate) const EXIT_REQUEST_CHANGES: u8 = 1;
/// Exit status for a run that failed to complete.
pub(crate) const EXIT_FAILED: u8 = 2;

#[derive(Parser)]
#[command(
    name = "demur",
    about = "Review changes with your own LLM key and argue why they should not merge",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Review local changes: the working copy or a revision range.
    Review {
        /// Revision range FROM..TO. Omit to review the working copy.
        range: Option<String>,
        /// Repository root. Defaults to the current directory.
        #[arg(long)]
        repo: Option<PathBuf>,
        /// Output format.
        #[arg(long, default_value = "markdown")]
        format: Format,
        /// Reuse completed passes from this directory when a run is
        /// retried. Nothing is cached unless this names one.
        #[arg(long)]
        cache_dir: Option<PathBuf>,
    },
    /// Print the JSON schema for the configuration file.
    Schema,
    /// Review a GitHub pull request by number or URL. Read-only unless
    /// --publish is passed.
    #[command(visible_alias = "pr")]
    ReviewPr {
        /// Pull request number or full URL.
        target: String,
        /// Repository root. Defaults to the current directory.
        #[arg(long)]
        repo: Option<PathBuf>,
        /// Output format.
        #[arg(long, default_value = "markdown")]
        format: Format,
        /// Publish the review under your own identity.
        #[arg(long)]
        publish: bool,
        /// Reuse completed passes from this directory when a run is
        /// retried. Nothing is cached unless this names one.
        #[arg(long)]
        cache_dir: Option<PathBuf>,
    },
}

#[derive(Clone, Copy, clap::ValueEnum)]
pub(crate) enum Format {
    /// Human-readable markdown.
    Markdown,
    /// Machine-readable JSON.
    Json,
}

#[tokio::main]
async fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_secs()
        .init();
    let cli = Cli::parse();
    match cli.command {
        Command::Review {
            range,
            repo,
            format,
            cache_dir,
        } => match review(range, repo, format, cache_dir).await {
            Ok(status) => ExitCode::from(status),
            Err(err) => {
                eprintln!("{err}");
                ExitCode::from(EXIT_FAILED)
            }
        },
        Command::Schema => {
            print!("{}", demur_core::config::json_schema_text());
            ExitCode::from(EXIT_APPROVE)
        }
        Command::ReviewPr {
            target,
            repo,
            format,
            publish,
            cache_dir,
        } => match pr::review_pr(target, repo, format, publish, cache_dir).await {
            Ok(status) => ExitCode::from(status),
            Err(err) => {
                eprintln!("{err}");
                ExitCode::from(EXIT_FAILED)
            }
        },
    }
}

async fn review(
    range: Option<String>,
    repo: Option<PathBuf>,
    format: Format,
    cache_dir: Option<PathBuf>,
) -> Result<u8, String> {
    let repo =
        repo.unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let config_path = repo.join(CONFIG_FILE_NAME);
    let mut config = Config::load(&config_path).map_err(|err| err.to_string())?;

    let target = match range {
        Some(range) => {
            let (from, to) = range
                .split_once("..")
                .ok_or_else(|| format!("range must look like FROM..TO, got `{range}`"))?;
            input::Target::Range(from.to_string(), to.to_string())
        }
        None => input::Target::WorkingCopy,
    };
    if let Some(dir) = cache_dir {
        // A working copy has no commit to key an entry to, and it changes
        // without anything recording that it did. Caching one would risk
        // serving an answer about code that is no longer there.
        if matches!(target, input::Target::WorkingCopy) {
            return Err(
                "--cache-dir needs a revision range: a working copy has no commit to key \
cache entries to, so caching it could reuse an answer about code you have since changed"
                    .to_string(),
            );
        }
        config.cache.enabled = true;
        config.cache.dir = Some(dir);
    }
    let diff = input::diff_text(&repo, &target)?;
    let files = parse_unified_diff(&diff);
    let ingestion = ingest(&files, &config);

    let registry = ProviderRegistry::from_config(&config).map_err(|err| err.to_string())?;
    let range_label = match &target {
        input::Target::WorkingCopy => "working copy".to_string(),
        input::Target::Range(from, to) => format!("{from}..{to}"),
    };
    let pipeline_input = PipelineInput {
        meta: PullRequestMeta {
            // demur wrote this label, so nothing in it is a claim the
            // author made and no rule judges it.
            title: format!("local review: {range_label}"),
            description: input::description(&repo, &target),
            head_sha: "local".to_string(),
            origin: MetaOrigin::Composed,
        },
        ingestion,
        diff_text: diff,
        prior_spend: 0.0,
        carried_findings: Vec::new(),
        suppress_fingerprints: std::collections::HashSet::new(),
    };

    match demur_core::pipeline::run(&registry, &config, &pipeline_input).await {
        Ok(RunOutcome::Review(review)) => {
            match format {
                Format::Markdown => println!("{}", review.body),
                Format::Json => println!(
                    "{}",
                    serde_json::to_string_pretty(&output::json_review(&review))
                        .map_err(|err| err.to_string())?
                ),
            }
            match review.verdict {
                Verdict::Approve => Ok(EXIT_APPROVE),
                Verdict::RequestChanges => Ok(EXIT_REQUEST_CHANGES),
            }
        }
        Ok(RunOutcome::Skipped { notice, .. }) => {
            eprintln!("{notice}");
            Ok(EXIT_FAILED)
        }
        Err(err) => {
            eprintln!("{err}");
            Ok(EXIT_FAILED)
        }
    }
}

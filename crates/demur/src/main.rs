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
    },
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
    let cli = Cli::parse();
    match cli.command {
        Command::Review {
            range,
            repo,
            format,
        } => match review(range, repo, format).await {
            Ok(status) => ExitCode::from(status),
            Err(err) => {
                eprintln!("{err}");
                ExitCode::from(EXIT_FAILED)
            }
        },
        Command::ReviewPr {
            target,
            repo,
            format,
            publish,
        } => match pr::review_pr(target, repo, format, publish).await {
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
) -> Result<u8, String> {
    let repo =
        repo.unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let config_path = repo.join(CONFIG_FILE_NAME);
    let config = Config::load(&config_path).map_err(|err| err.to_string())?;

    let target = match range {
        Some(range) => {
            let (from, to) = range
                .split_once("..")
                .ok_or_else(|| format!("range must look like FROM..TO, got `{range}`"))?;
            input::Target::Range(from.to_string(), to.to_string())
        }
        None => input::Target::WorkingCopy,
    };
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
            title: format!("local review: {range_label}"),
            description: String::new(),
            head_sha: "local".to_string(),
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
        Ok(RunOutcome::Skipped(notice)) => {
            eprintln!("{notice}");
            Ok(EXIT_FAILED)
        }
        Err(err) => {
            eprintln!("{err}");
            Ok(EXIT_FAILED)
        }
    }
}

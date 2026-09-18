//! The end-to-end pull request review flow: delta scope from prior
//! markers, the pipeline, carry-forward, publication, and the check run.

use crate::pipeline::prompt::MetaOrigin;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use super::{GitHubClient, GitHubError, InlineComment};
use crate::config::Config;
use crate::delta::{self, CarriedState, CarryDecision, Marker, ReviewScope};
use crate::diff::parse_unified_diff;
use crate::ingest::ingest;
use crate::pipeline::findings::Finding;
use crate::pipeline::prompt::PullRequestMeta;
use crate::pipeline::{PipelineError, PipelineInput, RunOutcome};
use crate::provider::ProviderRegistry;

/// Failures of the whole review flow.
#[derive(Debug, thiserror::Error)]
pub enum FlowError {
    /// A GitHub call failed.
    #[error(transparent)]
    GitHub(#[from] GitHubError),
    /// The pipeline failed after bounded retries. Nothing is published.
    #[error(transparent)]
    Pipeline(#[from] PipelineError),
}

/// What the flow did, for the distribution to surface.
#[derive(Debug)]
pub struct FlowOutcome {
    /// True when a review event and check run were published.
    pub published: bool,
    /// The check run conclusion: success, failure, or neutral.
    pub check_conclusion: &'static str,
    /// The review body or the explanatory notice, for job summaries.
    pub summary: String,
    /// True when a newer head existed and was not reviewed.
    pub head_moved: bool,
}

/// Review one pull request end to end.
#[allow(clippy::too_many_arguments)]
pub async fn review_pull_request(
    client: &GitHubClient,
    registry: &ProviderRegistry,
    config: &Config,
    number: u64,
) -> Result<FlowOutcome, FlowError> {
    let pr = client.pull_request(number).await?;
    let head_sha = pr.head_sha().to_string();
    log::info!(
        "pull request #{number}: head {head_sha}, draft={}",
        pr.draft
    );

    // Prior state.
    let prior_marker = client.prior_marker(number).await?;
    let prior_is_ancestor = match &prior_marker {
        Some(marker) => client.is_ancestor(&marker.head_sha, &head_sha).await?,
        None => false,
    };
    let scope = delta::derive_scope(prior_marker.as_ref(), prior_is_ancestor);
    match &scope {
        ReviewScope::Full => log::info!("scope: full review"),
        ReviewScope::Delta { since_sha } => {
            log::info!("scope: delta since {since_sha}")
        }
    }
    let diff_started = std::time::Instant::now();
    let diff_text = match &scope {
        ReviewScope::Full => client.pull_request_diff(number).await?,
        ReviewScope::Delta { since_sha } => {
            client.compare_diff(number, since_sha, &head_sha).await?
        }
    };
    log::info!(
        "diff: {} lines fetched in {:.1?}",
        diff_text.lines().count(),
        diff_started.elapsed()
    );
    let dismissed = client.dismissed_fingerprints(number).await;
    if !dismissed.is_empty() {
        log::info!("dismissed fingerprints: {}", dismissed.len());
    }

    // Ingest and prepare the pipeline input.
    let files = parse_unified_diff(&diff_text);
    let ingestion = ingest(&files, config);
    let changed = delta::changed_lines(&files);
    let cluster_hunks: HashMap<String, Vec<crate::diff::Hunk>> = ingestion
        .clusters
        .iter()
        .map(|cluster| (cluster.path.clone(), cluster.hunks.clone()))
        .collect();

    // Carry unresolved prior findings into synthesis, minus those a human
    // dismissed. Dismissed fingerprints also suppress fresh findings with
    // the same fingerprint.
    let mut carried = Vec::new();
    let mut suppress = dismissed.clone();
    let suppress_for_fingerprints = suppress.clone();
    if let Some(marker) = &prior_marker {
        let current: Vec<(Finding, String)> = Vec::new();
        let decisions = delta::carry_forward(marker, &dismissed, &current, &changed);
        for (finding, decision) in decisions {
            if decision == CarryDecision::Carried
                && let Some(record) = marker
                    .findings
                    .iter()
                    .find(|record| record.message == finding.message && record.path == finding.file)
            {
                suppress.insert(record.fingerprint.clone());
                carried.push((finding, record.fingerprint.clone()));
            }
        }
    }
    let carried_findings: Vec<Finding> =
        carried.iter().map(|(finding, _)| finding.clone()).collect();

    let input = PipelineInput {
        meta: PullRequestMeta {
            title: if pr.title.is_empty() {
                format!("pull request #{number}")
            } else {
                pr.title.clone()
            },
            description: pr.body.clone().unwrap_or_default(),
            head_sha: head_sha.clone(),
            origin: MetaOrigin::PullRequest,
        },
        ingestion,
        diff_text,
        prior_spend: prior_marker
            .as_ref()
            .map(|marker| marker.spend.values().sum())
            .unwrap_or(0.0),
        carried_findings,
        suppress_fingerprints: suppress,
    };

    // Run the pipeline.
    let outcome = crate::pipeline::run(registry, config, &input).await?;
    let review = match outcome {
        RunOutcome::Review(review) => *review,
        RunOutcome::Skipped { notice, violations } => {
            // The check run must not report success as if a review
            // happened. A carried blocker keeps it failed, and so does a
            // rule violation at a blocking severity, which costs nothing
            // to establish and is therefore known even here.
            let blocking_rank = config
                .block_on
                .severities
                .iter()
                .map(|severity| severity.rank())
                .min()
                .unwrap_or_else(|| crate::config::Severity::Blocker.rank());
            let blocking_violation = violations
                .iter()
                .any(|violation| violation.severity.rank() >= blocking_rank);
            let carried_blocker = blocking_violation
                || prior_marker.as_ref().is_some_and(|marker| {
                    marker.findings.iter().any(|record| {
                        record.state == CarriedState::Unresolved
                            && record.severity == crate::config::Severity::Blocker
                    })
                });
            return Ok(FlowOutcome {
                published: false,
                check_conclusion: if carried_blocker {
                    "failure"
                } else {
                    "neutral"
                },
                summary: notice,
                head_moved: false,
            });
        }
    };

    // Fingerprints for the surviving findings, for suppression of fresh
    // duplicates of dismissed findings, inline markers, and the next
    // marker.
    let mut fingerprints: Vec<(Finding, String)> = Vec::new();
    for finding in &review.published {
        if let Some(hunks) = cluster_hunks.get(&finding.file) {
            let fp = delta::fingerprint(finding, hunks);
            if suppress_for_fingerprints.contains(&fp) {
                continue;
            }
            fingerprints.push((finding.clone(), fp));
        }
    }

    // Inline comments only for findings anchored in the current diff.
    let mut comments = Vec::new();
    for (finding, fingerprint) in &fingerprints {
        if let Some(lines) = anchored_lines(&input.ingestion, &finding.file)
            && lines.contains(&finding.start_line)
        {
            comments.push(InlineComment {
                path: finding.file.clone(),
                line: finding.start_line,
                body: render_comment(finding, fingerprint),
            });
        }
    }

    // Did the head move while we worked?
    let current_head = client.pull_request(number).await?.head_sha().to_string();
    let head_moved = current_head != head_sha;
    let mut body = review.body.clone();
    if head_moved {
        body.push_str(&format!(
            "\n\nNote: the pull request head advanced to {current_head} while this \
review ran against {head_sha}. The newer commit was not reviewed.\n"
        ));
    }

    let spend: BTreeMap<String, f64> = review
        .spend
        .passes
        .iter()
        .map(|pass| (pass.pass.clone(), pass.cost))
        .collect();
    let marker = delta::build_marker(
        &head_sha,
        prior_marker.as_ref(),
        &spend,
        &carried_states(prior_marker.as_ref(), &dismissed, &changed),
        &fingerprints,
    );

    log::info!(
        "publishing: {} inline comment(s), {} finding(s) in body",
        comments.len(),
        fingerprints.len()
    );
    super::publish::publish_review(
        client,
        number,
        &head_sha,
        review.verdict,
        &body,
        Some(&marker),
        &comments,
    )
    .await?;
    log::info!("published: check run {}", review.verdict.check_conclusion());

    Ok(FlowOutcome {
        published: true,
        check_conclusion: review.verdict.check_conclusion(),
        summary: body,
        head_moved,
    })
}

fn carried_states(
    prior: Option<&Marker>,
    dismissed: &HashSet<String>,
    changed: &HashMap<String, BTreeSet<u32>>,
) -> Vec<(Finding, CarryDecision, CarriedState)> {
    let Some(prior) = prior else {
        return Vec::new();
    };
    let decisions = delta::carry_forward(prior, dismissed, &[], changed);
    decisions
        .into_iter()
        .map(|(finding, decision)| {
            let state = match decision {
                CarryDecision::Carried | CarryDecision::Reproduced => CarriedState::Unresolved,
                CarryDecision::Dismissed | CarryDecision::ResolvedByChanges => {
                    CarriedState::Resolved
                }
            };
            (finding, decision, state)
        })
        .collect()
}

/// The set of new-side lines the diff touches for one file, when the file
/// is part of the ingestion.
fn anchored_lines(ingestion: &crate::ingest::Ingestion, path: &str) -> Option<BTreeSet<u32>> {
    let cluster = ingestion
        .clusters
        .iter()
        .find(|cluster| cluster.path == path)?;
    let mut lines = BTreeSet::new();
    for hunk in &cluster.hunks {
        for line in &hunk.lines {
            if line.kind == crate::diff::LineKind::Add
                && let Some(new_line) = line.new_line
            {
                lines.insert(new_line);
            }
            if line.kind == crate::diff::LineKind::Context
                && let Some(new_line) = line.new_line
            {
                lines.insert(new_line);
            }
        }
    }
    Some(lines)
}

fn render_comment(finding: &Finding, fingerprint: &str) -> String {
    let severity = match finding.severity {
        crate::config::Severity::Blocker => "blocker",
        crate::config::Severity::Warning => "warning",
        crate::config::Severity::Note => "note",
    };
    let mut body = format!("**[{severity}]** {}\n\n{}\n", finding.message, finding.harm);
    if let Some(suggestion) = &finding.suggestion {
        body.push_str("```suggestion\n");
        body.push_str(suggestion);
        body.push('\n');
        body.push_str("```\n");
    }
    body.push_str(&format!("<!-- demur:fp {fingerprint} -->"));
    body
}

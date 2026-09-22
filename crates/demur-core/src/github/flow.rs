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

    let state = pull_request_state(client, number, &head_sha, config).await?;

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
        ingestion: state.ingestion.clone(),
        diff_text: state.diff_text.clone(),
        prior_spend: state.prior_spend,
        carried_findings: state.carried_findings.clone(),
        suppress_fingerprints: state.suppress.clone(),
        // The Action checks the repository out before running, so the
        // working directory is the checkout retrieval may read.
        repo_root: std::env::current_dir().ok(),
    };

    // Run the pipeline.
    let outcome = crate::pipeline::run(registry, config, &input).await;
    let review = match outcome {
        Ok(RunOutcome::Review(review)) => *review,
        Ok(RunOutcome::Skipped { notice, violations }) => {
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
                || state.prior_marker.as_ref().is_some_and(|marker| {
                    marker.findings.iter().any(|record| {
                        record.state == CarriedState::Unresolved
                            && record.severity == crate::config::Severity::Blocker
                    })
                });
            // Nothing was reviewed, so the reviewed head, the run count,
            // and every carried finding stay exactly as they were; only
            // dismissals read from GitHub are recorded.
            let marker = state_only_marker(
                state.prior_marker.as_ref(),
                &state.dismissed,
                &BTreeMap::new(),
            );
            let conclusion = if carried_blocker {
                "failure"
            } else {
                "neutral"
            };
            super::publish::publish_notice(
                client,
                number,
                &head_sha,
                &notice,
                marker.as_ref(),
                conclusion,
            )
            .await?;
            return Ok(FlowOutcome {
                published: true,
                check_conclusion: conclusion,
                summary: notice,
                head_moved: false,
            });
        }
        Ok(RunOutcome::Failed { error, spend }) => {
            // The failed run's billed passes count against the cap, so the
            // notice carries a marker that preserves prior state plus the
            // spend recorded before the failure.
            let run_spend: BTreeMap<String, f64> = spend
                .iter()
                .map(|pass| (pass.pass.clone(), pass.cost))
                .collect();
            let marker =
                state_only_marker(state.prior_marker.as_ref(), &state.dismissed, &run_spend);
            let total: f64 = run_spend.values().sum();
            let lines: String = spend
                .iter()
                .map(|pass| format!("- {}: ${:.4}\n", pass.pass, pass.cost))
                .collect();
            let notice = format!(
                "## demur: review failed\n\n\
The review ran but failed before a verdict was reached: {error}\n\n\
Spend recorded before the failure ({total:.4} USD):\n{lines}\n\
The next run counts this spend against the pull request's budget."
            );
            if let Err(publish_err) = super::publish::publish_notice(
                client,
                number,
                &head_sha,
                &notice,
                marker.as_ref(),
                "failure",
            )
            .await
            {
                log::warn!("could not publish the failure notice: {publish_err}");
                log::warn!("the {total:.4} USD recorded by this run could not be persisted");
            }
            return Err(error.into());
        }
        Err(err) => return Err(err.into()),
    };

    let (fingerprints, comments) = anchor_findings(
        &review.published,
        &state.ingestion,
        &state.cluster_hunks,
        &state.suppress,
    );

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
    let marker = continuation_marker(&head_sha, &state, &spend, &fingerprints);

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

/// Fingerprint the surviving findings and build the inline comments for
/// those anchored in the current diff. Shared so every path that publishes
/// a review publishes the same one: a body-only review is a review with no
/// resolvable threads, and a review with no fingerprints is a review no
/// later run can carry forward or silence.
pub fn anchor_findings(
    published: &[Finding],
    ingestion: &crate::ingest::Ingestion,
    cluster_hunks: &HashMap<String, Vec<crate::diff::Hunk>>,
    suppress: &HashSet<String>,
) -> (Vec<(Finding, String)>, Vec<InlineComment>) {
    let mut fingerprints: Vec<(Finding, String)> = Vec::new();
    for finding in published {
        if let Some(hunks) = cluster_hunks.get(&finding.file) {
            let fingerprint = delta::fingerprint(finding, hunks);
            if suppress.contains(&fingerprint) {
                continue;
            }
            fingerprints.push((finding.clone(), fingerprint));
        }
    }
    let mut comments = Vec::new();
    for (finding, fingerprint) in &fingerprints {
        if let Some(lines) = anchored_lines(ingestion, &finding.file)
            && lines.contains(&finding.start_line)
        {
            comments.push(InlineComment {
                path: finding.file.clone(),
                line: finding.start_line,
                body: render_comment(finding, fingerprint),
            });
        }
    }
    (fingerprints, comments)
}

/// Everything a pull request review derives from GitHub before the
/// pipeline runs: the prior state, the scope, the diff, and the findings
/// carried into synthesis. Shared by every distribution that reviews a
/// pull request through GitHub, so a review covers the same scope and
/// continues the same history wherever it runs from.
pub struct PullRequestState {
    /// The prior marker, when any earlier run published one.
    pub prior_marker: Option<Marker>,
    /// Fingerprints of threads a human resolved.
    pub dismissed: HashSet<String>,
    /// Every fingerprint a fresh finding must be silent under: dismissed
    /// fingerprints plus those of every carried finding.
    pub suppress: HashSet<String>,
    /// Findings carried from earlier runs into synthesis.
    pub carried_findings: Vec<Finding>,
    /// Everything earlier runs spent, in USD.
    pub prior_spend: f64,
    /// The diff text this run reviews.
    pub diff_text: String,
    /// The ingested clusters for that diff.
    pub ingestion: crate::ingest::Ingestion,
    /// Per-cluster hunks, for anchoring.
    pub cluster_hunks: HashMap<String, Vec<crate::diff::Hunk>>,
    /// The new-side lines each file's diff touches.
    pub changed_lines: HashMap<String, BTreeSet<u32>>,
}

/// Derive the review state for one pull request head: prior marker and
/// dismissed threads from GitHub, the scope from the marker's head, the
/// diff that scope covers, and the carried findings and suppress set that
/// state implies.
pub async fn pull_request_state(
    client: &GitHubClient,
    number: u64,
    head_sha: &str,
    config: &Config,
) -> Result<PullRequestState, FlowError> {
    let prior_marker = client.prior_marker(number).await?;
    let prior_is_ancestor = match &prior_marker {
        Some(marker) => client.is_ancestor(&marker.head_sha, head_sha).await?,
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
            client.compare_diff(number, since_sha, head_sha).await?
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

    let files = parse_unified_diff(&diff_text);
    let ingestion = ingest(&files, config);
    let changed_lines = delta::changed_lines(&files);
    let cluster_hunks: HashMap<String, Vec<crate::diff::Hunk>> = ingestion
        .clusters
        .iter()
        .map(|cluster| (cluster.path.clone(), cluster.hunks.clone()))
        .collect();

    // Carry unresolved prior findings into synthesis, minus those a human
    // dismissed. Every fingerprint a carried record holds suppresses fresh
    // findings with the same fingerprint, both in the pipeline and at
    // anchoring, so a carried finding is never re-threaded.
    let mut carried_findings = Vec::new();
    let mut suppress = dismissed.clone();
    if let Some(marker) = &prior_marker {
        let decisions = delta::carry_forward(marker, &dismissed, &[], &changed_lines);
        for (finding, decision, fingerprints) in decisions {
            if decision == CarryDecision::Carried {
                suppress.extend(fingerprints.iter().cloned());
                carried_findings.push(finding);
            }
        }
    }

    Ok(PullRequestState {
        prior_spend: prior_marker
            .as_ref()
            .map(|marker| marker.cumulative_spend())
            .unwrap_or(0.0),
        prior_marker,
        dismissed,
        suppress,
        carried_findings,
        diff_text,
        ingestion,
        cluster_hunks,
        changed_lines,
    })
}

/// The marker for a published review: prior state continued with this
/// run's published findings, the new resolution states, and the run's
/// spend on top of everything earlier runs spent.
pub fn continuation_marker(
    head_sha: &str,
    state: &PullRequestState,
    run_spend: &BTreeMap<String, f64>,
    published: &[(Finding, String)],
) -> Marker {
    delta::build_marker(
        head_sha,
        state.prior_marker.as_ref(),
        run_spend,
        &carried_states(
            state.prior_marker.as_ref(),
            &state.dismissed,
            &state.changed_lines,
        ),
        published,
    )
}

fn carried_states(
    prior: Option<&Marker>,
    dismissed: &HashSet<String>,
    changed: &HashMap<String, BTreeSet<u32>>,
) -> Vec<(Finding, CarryDecision, CarriedState, Vec<String>)> {
    let Some(prior) = prior else {
        return Vec::new();
    };
    delta::carry_forward(prior, dismissed, &[], changed)
        .into_iter()
        .map(|(finding, decision, fingerprints)| {
            let state = match decision {
                CarryDecision::Carried | CarryDecision::Reproduced => CarriedState::Unresolved,
                CarryDecision::Dismissed | CarryDecision::ResolvedByChanges => {
                    CarriedState::Resolved
                }
            };
            (finding, decision, state, fingerprints)
        })
        .collect()
}

/// The marker for a run that reviewed nothing: prior state with the run's
/// recorded spend added. The reviewed head and the run count stay as the
/// prior marker had them, so the next run's delta base is still the last
/// head anything was actually reviewed at, and no finding resolves,
/// because nothing was reviewed that could resolve it.
fn state_only_marker(
    prior: Option<&Marker>,
    dismissed: &HashSet<String>,
    run_spend: &BTreeMap<String, f64>,
) -> Option<Marker> {
    let prior = prior?;
    let mut marker = delta::build_marker(
        &prior.head_sha,
        Some(prior),
        run_spend,
        &carried_states(Some(prior), dismissed, &HashMap::new()),
        &[],
    );
    marker.run_count = prior.run_count;
    Some(marker)
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
    for concern in &finding.further_concerns {
        body.push_str(&format!(
            "\n---\n\n**{}**\n\n{}\n",
            concern.message, concern.harm
        ));
        if let Some(suggestion) = &concern.suggestion {
            body.push_str("```\n");
            body.push_str(suggestion);
            body.push('\n');
            body.push_str("```\n");
        }
    }
    body.push_str(&format!("<!-- demur:fp {fingerprint} -->"));
    body
}

//! The multi-pass review pipeline: triage, deep dives, cross-examination,
//! verdict synthesis, and the budget gate wiring across them.

pub mod budget;
pub mod findings;
pub mod prompt;
pub mod synthesis;

#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::collections::HashSet;

use crate::config::{Config, Lenses, Profile};
use crate::cost::{ModelPrice, estimate_tokens};
use crate::diff::Hunk;
use crate::ingest::Ingestion;
use crate::provider::{
    CompletionRequest, CompletionResponse, ProviderError, ProviderRegistry, RetryPolicy,
    TokenUsage, complete_with_retries,
};
use budget::{BudgetGate, Degradation, LadderDecision, PassKind};
use prompt::PullRequestMeta;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use synthesis::{SynthesisInput, Verdict, synthesize};

/// Bounded attempts for responses that deserialize but fail schema checks.
const SCHEMA_ATTEMPTS: u32 = 3;

/// How often a truncated response may raise the output ceiling, which
/// grows 4x per escalation.
const MAX_CEILING_ESCALATIONS: u32 = 2;

/// A provider replaying recorded responses. Used by the fixture harness for
/// end-to-end pipeline tests and for offline replay runs.
pub struct RecordedProvider {
    steps: std::sync::Mutex<std::collections::VecDeque<Result<Value, ProviderError>>>,
    usage: TokenUsage,
}

impl RecordedProvider {
    /// Build from recorded steps, consumed in order. Each step is either a
    /// JSON response value or an error to raise.
    pub fn new(steps: Vec<Result<Value, ProviderError>>) -> RecordedProvider {
        RecordedProvider {
            steps: std::sync::Mutex::new(steps.into()),
            usage: TokenUsage {
                input_tokens: 100,
                cached_input_tokens: 0,
                output_tokens: 20,
            },
        }
    }
}

impl crate::provider::Provider for RecordedProvider {
    async fn complete(
        &self,
        _request: &CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        let step = self
            .steps
            .lock()
            .expect("recorded steps lock")
            .pop_front()
            .ok_or_else(|| ProviderError::Malformed {
                message: "no more recorded steps".to_string(),
            })?;
        match step {
            Ok(content) => Ok(CompletionResponse {
                content,
                usage: self.usage,
            }),
            Err(error) => Err(error),
        }
    }
}

/// Failure of a pipeline run. Nothing is published when it occurs.
#[derive(Debug, thiserror::Error)]
pub enum PipelineError {
    /// A provider call failed after its bounded retries.
    #[error(transparent)]
    Provider(#[from] ProviderError),
}

/// The pull request under review plus everything earlier runs established.
pub struct PipelineInput {
    /// Pull request metadata embedded in prompts.
    pub meta: PullRequestMeta,
    /// Ingested clusters and exclusions.
    pub ingestion: Ingestion,
    /// Repository-wide unified diff text for triage and cross-examination.
    pub diff_text: String,
    /// Spend recorded by earlier runs on this pull request.
    pub prior_spend: f64,
    /// Unresolved findings carried forward from earlier runs.
    pub carried_findings: Vec<findings::Finding>,
    /// Fingerprints a human dismissed. Fresh findings matching them stay
    /// silent.
    pub suppress_fingerprints: HashSet<String>,
}

/// Spend for one pass.
#[derive(Debug, Clone)]
pub struct PassSpend {
    /// Pass name.
    pub pass: String,
    /// Token usage reported by the provider.
    pub usage: TokenUsage,
    /// Cost in USD at the role model's prices.
    pub cost: f64,
}

/// Spend for one run.
#[derive(Debug, Clone)]
pub struct RunSpend {
    /// Per-pass spend in execution order.
    pub passes: Vec<PassSpend>,
    /// Spend recorded by earlier runs on this pull request.
    pub prior_spend: f64,
    /// This run's total.
    pub total: f64,
}

/// A completed review ready for publication. The verdict is final;
/// publication never recomputes it.
#[derive(Debug)]
pub struct Review {
    /// The synthesized verdict.
    pub verdict: Verdict,
    /// Findings published within the comment budget, ranked.
    pub published: Vec<findings::Finding>,
    /// Findings omitted beyond the comment budget.
    pub omitted: usize,
    /// Verdict-setting findings cut by the budget, named in the body.
    pub beyond_budget: Vec<findings::Finding>,
    /// The rendered markdown review body.
    pub body: String,
    /// Degradations applied, always disclosed in the body.
    pub degradations: Vec<Degradation>,
    /// Spend for this run and the cumulative pull request spend.
    pub spend: RunSpend,
}

/// The outcome of a pipeline run.
#[derive(Debug)]
pub enum RunOutcome {
    /// A review was synthesized and is ready for publication.
    Review(Box<Review>),
    /// No review could be produced. The string is the explanatory notice.
    Skipped(String),
}

/// Run the pipeline for the configured profile.
pub async fn run(
    registry: &ProviderRegistry,
    config: &Config,
    input: &PipelineInput,
) -> Result<RunOutcome, PipelineError> {
    let profile = config.profile.unwrap_or(Profile::Standard);
    let triage_price = ModelPrice::from_model(&config.models.triage);
    let deep_price = ModelPrice::from_model(&config.models.deep);
    let verdict_price = ModelPrice::from_model(&config.models.verdict);
    let mut gate = BudgetGate::new(config.budget.cap(), input.prior_spend);
    let mut degradations: Vec<Degradation> = Vec::new();
    let mut spend: Vec<PassSpend> = Vec::new();
    let mut all_findings: Vec<findings::Finding> = Vec::new();
    let diff_paths: Vec<String> = input
        .ingestion
        .clusters
        .iter()
        .map(|cluster| cluster.path.clone())
        .collect();

    if input.ingestion.clusters.is_empty() {
        for degradation in &degradations {
            log::warn!("degradation: {}", degradation.describe());
        }
        let review = synthesize_review(
            config,
            input,
            &mut gate,
            &verdict_price,
            registry,
            &mut degradations,
            &mut spend,
            std::mem::take(&mut all_findings),
        )
        .await?;
        return Ok(RunOutcome::Review(Box::new(review)));
    }

    // Triage.
    let context = prompt::repository_context(&input.meta, &input.diff_text);
    let shrunk_context = prompt::repository_context(&input.meta, &shrunk_diff_text(input));
    let triage_task = "Triage the changed hunks. For each file cluster, suggest review \
lenses from: correctness, security, performance, style. Also report any finding \
you can already anchor to an exact file and line range with its concrete harm.";
    log::info!(
        "triage: ~{} input tokens estimated on {}",
        estimate_prompt_tokens(&context),
        config.models.triage.name
    );
    let triage_started = std::time::Instant::now();
    let estimate = budget::PassEstimate {
        kind: PassKind::Triage,
        pass: "triage",
        full_tokens: estimate_prompt_tokens(&context),
        shrunk_tokens: estimate_prompt_tokens(&shrunk_context),
        max_output_tokens: config.limits.max_tokens,
        price: &triage_price,
        downgrade_price: None,
    };
    match gate.authorize(estimate) {
        LadderDecision::Run => {}
        LadderDecision::RunDegraded(degradation) => degradations.push(degradation),
        LadderDecision::StandDown => {
            return Ok(RunOutcome::Skipped(skip_notice(input, &gate)));
        }
    }
    let (triage_output, usage) = call_pass::<findings::TriageOutput>(
        &registry.triage,
        prompt::assemble(&context, triage_task, &prompt::findings_schema()),
        prompt::findings_schema(),
        "triage",
        config.limits.max_tokens,
    )
    .await?;
    spend.push(PassSpend {
        pass: "triage".to_string(),
        usage,
        cost: gate.record(&usage, &triage_price),
    });
    log::info!(
        "triage: {} finding(s) in {:.1?}, ${:.4}",
        triage_output.findings.len(),
        triage_started.elapsed(),
        spend
            .last()
            .map(|pass_spend| pass_spend.cost)
            .unwrap_or(0.0)
    );
    for raw in &triage_output.findings {
        if let Some(finding) = findings::validate(raw, &diff_paths) {
            let hunks = input
                .ingestion
                .clusters
                .iter()
                .find(|cluster| cluster.path == finding.file)
                .map(|cluster| &cluster.hunks);
            let suppressed = hunks.is_some_and(|hunks| {
                input
                    .suppress_fingerprints
                    .contains(&crate::delta::fingerprint(&finding, hunks))
            });
            if suppressed {
                continue;
            }
            all_findings.push(finding);
        }
    }
    let lens_map: HashMap<String, Vec<String>> = triage_output
        .cluster_lens
        .iter()
        .map(|entry| (entry.path.clone(), entry.lenses.clone()))
        .collect();

    // Deep dives.
    let mut unreviewed: Vec<String> = Vec::new();
    if profile != Profile::Quick {
        let ceiling = config.limits.deep_calls;
        let mut calls: u32 = 0;
        log::info!(
            "deep dives: {} cluster(s) to consider, ceiling {} call(s)",
            input.ingestion.clusters.len(),
            ceiling
        );
        'clusters: for cluster in &input.ingestion.clusters {
            if calls >= ceiling {
                unreviewed.push(cluster.path.clone());
                continue;
            }
            let lenses = select_lenses(cluster, &lens_map, &config.lenses);
            if lenses.is_empty() {
                continue;
            }
            for lens in lenses {
                if calls >= ceiling {
                    unreviewed.push(cluster.path.clone());
                    break;
                }
                log::info!("deep dive [{}]: {}", lens, cluster.path);
                let dive_started = std::time::Instant::now();
                let context = prompt::cluster_context(&input.meta, &cluster.path, &cluster.hunks);
                let shrunk_hunks: Vec<Hunk> = cluster.hunks.iter().take(1).cloned().collect();
                let shrunk = prompt::cluster_context(&input.meta, &cluster.path, &shrunk_hunks);
                let decision = gate.authorize(budget::PassEstimate {
                    kind: PassKind::Deep,
                    pass: "deep dive",
                    full_tokens: estimate_prompt_tokens(&context),
                    shrunk_tokens: estimate_prompt_tokens(&shrunk),
                    max_output_tokens: config.limits.max_tokens,
                    price: &deep_price,
                    downgrade_price: Some(&triage_price),
                });
                let mut downgraded = false;
                if let LadderDecision::RunDegraded(Degradation::ModelDowngraded { .. }) = &decision
                {
                    downgraded = true;
                }
                if let LadderDecision::RunDegraded(degradation) = &decision {
                    degradations.push(degradation.clone());
                }
                match decision {
                    LadderDecision::Run | LadderDecision::RunDegraded(_) => {}
                    LadderDecision::StandDown => {
                        degradations.push(Degradation::SummaryOnly {
                            skipped: vec!["remaining deep dives".to_string()],
                        });
                        break 'clusters;
                    }
                }
                let task = deep_dive_task(&lens);
                let dived = call_pass::<findings::ModelFindings>(
                    &registry.deep,
                    prompt::assemble(&context, &task, &prompt::findings_schema()),
                    prompt::findings_schema(),
                    "deep dive",
                    config.limits.max_tokens,
                )
                .await;
                let (dive_output, usage) = match dived {
                    Ok(output) => output,
                    Err(ProviderError::ContextOverflow { .. }) => {
                        // Shrink to the first hunk and retry once.
                        match call_pass::<findings::ModelFindings>(
                            &registry.deep,
                            prompt::assemble(&shrunk, &task, &prompt::findings_schema()),
                            prompt::findings_schema(),
                            "deep dive",
                            config.limits.max_tokens,
                        )
                        .await
                        {
                            Ok(output) => {
                                degradations.push(Degradation::ContextShrunk {
                                    pass: format!("deep dive ({lens})"),
                                });
                                output
                            }
                            Err(err) => return Err(err.into()),
                        }
                    }
                    Err(err) => return Err(err.into()),
                };
                let paid_price = if downgraded {
                    &triage_price
                } else {
                    &deep_price
                };
                let dive_cost = gate.record(&usage, paid_price);
                spend.push(PassSpend {
                    pass: format!("deep dive {lens}"),
                    usage,
                    cost: dive_cost,
                });
                log::info!(
                    "deep dive [{}]: {} finding(s) in {:.1?}, ${:.4}",
                    lens,
                    dive_output.findings.len(),
                    dive_started.elapsed(),
                    dive_cost
                );
                for raw in &dive_output.findings {
                    if let Some(finding) = findings::validate(raw, &diff_paths) {
                        if input
                            .suppress_fingerprints
                            .contains(&crate::delta::fingerprint(&finding, &cluster.hunks))
                        {
                            continue;
                        }
                        all_findings.push(finding);
                    }
                }
                calls += 1;
            }
        }
        if !unreviewed.is_empty() {
            degradations.push(Degradation::DeepCallsCapped { unreviewed });
        }
    }

    // Cross-examination, deep profile only.
    if profile == Profile::Deep {
        let context = prompt::repository_context(&input.meta, &input.diff_text);
        let shrunk_context = prompt::repository_context(&input.meta, &shrunk_diff_text(input));
        let task = "Cross-examine this pull request adversarially: adversarial inputs, \
rollback safety, concurrency hazards, migration safety, and breaking interface \
changes. Report only findings you can anchor to an exact file and line range \
with their concrete harm.";
        let estimate = budget::PassEstimate {
            kind: PassKind::Deep,
            pass: "cross-examination",
            full_tokens: estimate_prompt_tokens(&context),
            shrunk_tokens: estimate_prompt_tokens(&shrunk_context),
            max_output_tokens: config.limits.max_tokens,
            price: &deep_price,
            downgrade_price: Some(&triage_price),
        };
        match gate.authorize(estimate) {
            LadderDecision::Run => {}
            LadderDecision::RunDegraded(degradation) => degradations.push(degradation),
            LadderDecision::StandDown => {
                degradations.push(Degradation::SummaryOnly {
                    skipped: vec!["cross-examination".to_string()],
                });
            }
        }
        if !gate_is_stood_down(&degradations) {
            log::info!("cross-examination: running on {}", config.models.deep.name);
            let cross_started = std::time::Instant::now();
            let (cross_output, usage) = call_pass::<findings::ModelFindings>(
                &registry.deep,
                prompt::assemble(&context, task, &prompt::cross_examination_schema()),
                prompt::cross_examination_schema(),
                "cross-examination",
                config.limits.max_tokens,
            )
            .await?;
            spend.push(PassSpend {
                pass: "cross-examination".to_string(),
                usage,
                cost: gate.record(&usage, &deep_price),
            });
            log::info!(
                "cross-examination: {} finding(s) in {:.1?}",
                cross_output.findings.len(),
                cross_started.elapsed()
            );
            for raw in &cross_output.findings {
                if let Some(finding) = findings::validate(raw, &diff_paths) {
                    let hunks = input
                        .ingestion
                        .clusters
                        .iter()
                        .find(|cluster| cluster.path == finding.file)
                        .map(|cluster| &cluster.hunks);
                    let suppressed = hunks.is_some_and(|hunks| {
                        input
                            .suppress_fingerprints
                            .contains(&crate::delta::fingerprint(&finding, hunks))
                    });
                    if suppressed {
                        continue;
                    }
                    all_findings.push(finding);
                }
            }
        }
    }

    for degradation in &degradations {
        log::warn!("degradation: {}", degradation.describe());
    }
    let review = synthesize_review(
        config,
        input,
        &mut gate,
        &verdict_price,
        registry,
        &mut degradations,
        &mut spend,
        all_findings,
    )
    .await?;
    Ok(RunOutcome::Review(Box::new(review)))
}

#[allow(clippy::too_many_arguments)]
async fn synthesize_review(
    config: &Config,
    input: &PipelineInput,
    gate: &mut BudgetGate,
    verdict_price: &ModelPrice,
    registry: &ProviderRegistry,
    degradations: &mut Vec<Degradation>,
    spend: &mut Vec<PassSpend>,
    findings: Vec<findings::Finding>,
) -> Result<Review, PipelineError> {
    let mut all = findings;
    all.extend(input.carried_findings.iter().cloned());

    // The verdict model drafts the summary prose; the verdict itself is
    // computed mechanically in synthesis and never by the model.
    let summary = if all.is_empty() && input.ingestion.clusters.is_empty() {
        None
    } else {
        let findings_text = all
            .iter()
            .map(|finding| {
                format!(
                    "- [{}] {}: {}",
                    severity_word(finding.severity),
                    finding.location(),
                    finding.message
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let context = prompt::repository_context(&input.meta, &findings_text);
        let task = "Draft a two sentence summary of the strongest case against merging, \
based only on the findings listed above. If no findings are listed, state that \
coverage was complete and no defect was established.";
        let cheap_verdict_price = ModelPrice::from_model(&config.models.triage);
        let decision = gate.authorize(budget::PassEstimate {
            kind: PassKind::Verdict,
            pass: "verdict summary",
            full_tokens: estimate_prompt_tokens(&context),
            shrunk_tokens: estimate_prompt_tokens(&context),
            max_output_tokens: config.limits.max_tokens,
            price: verdict_price,
            downgrade_price: Some(&cheap_verdict_price),
        });
        if let LadderDecision::RunDegraded(degradation) = &decision {
            degradations.push(degradation.clone());
        }
        match decision {
            LadderDecision::Run | LadderDecision::RunDegraded(_) => {
                let (summary_output, usage) = call_pass::<SummaryOutput>(
                    &registry.verdict,
                    prompt::assemble(&context, task, &summary_schema()),
                    summary_schema(),
                    "verdict summary",
                    config.limits.max_tokens,
                )
                .await?;
                spend.push(PassSpend {
                    pass: "verdict summary".to_string(),
                    usage,
                    cost: gate.record(&usage, verdict_price),
                });
                Some(summary_output.summary)
            }
            LadderDecision::StandDown => {
                degradations.push(Degradation::SummaryOnly {
                    skipped: vec!["verdict summary".to_string()],
                });
                None
            }
        }
    };

    let synthesis = synthesize(SynthesisInput {
        findings: all,
        block_on: config.block_on.severities.clone(),
        comment_budget: config.limits.comments,
        degradations: degradations.clone(),
        spend_lines: spend
            .iter()
            .map(|pass_spend| (pass_spend.pass.clone(), pass_spend.cost))
            .collect(),
        prior_spend: input.prior_spend,
        summary,
    });
    Ok(Review {
        verdict: synthesis.verdict,
        published: synthesis.published,
        omitted: synthesis.omitted,
        beyond_budget: synthesis.beyond_budget,
        body: synthesis.body,
        degradations: degradations.clone(),
        spend: RunSpend {
            total: spend.iter().map(|pass_spend| pass_spend.cost).sum(),
            passes: spend.clone(),
            prior_spend: input.prior_spend,
        },
    })
}

fn summary_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "summary": {"type": "string"},
        },
        "required": ["summary"],
        "additionalProperties": false,
    })
}

#[derive(Debug, Deserialize)]
struct SummaryOutput {
    summary: String,
}

/// One provider call with schema-validated output and bounded retries.
/// Returns the parsed output and the usage the call reported.
async fn call_pass<T: DeserializeOwned>(
    provider: &crate::provider::AnyProvider,
    prompt: prompt::Prompt,
    schema: Value,
    schema_name: &str,
    max_output: u32,
) -> Result<(T, TokenUsage), ProviderError> {
    let make_request = {
        let system = prompt.system.clone();
        let user = prompt.user.clone();
        let schema = schema.clone();
        let schema_name = schema_name.to_string();
        move |ceiling: u32| CompletionRequest {
            system: system.clone(),
            user: user.clone(),
            schema: schema.clone(),
            schema_name: schema_name.clone(),
            max_output_tokens: ceiling,
        }
    };
    let mut ceiling = max_output;
    let mut escalations: u32 = 0;
    let mut attempts: u32 = 0;
    let mut carried_usage = TokenUsage::default();
    let mut last_error;
    loop {
        let response =
            match complete_with_retries(provider, &make_request(ceiling), &RetryPolicy::default())
                .await
            {
                Ok(response) => response,
                Err(ProviderError::OutputTruncated { message, usage }) => {
                    carried_usage.input_tokens = carried_usage
                        .input_tokens
                        .saturating_add(usage.input_tokens);
                    carried_usage.cached_input_tokens = carried_usage
                        .cached_input_tokens
                        .saturating_add(usage.cached_input_tokens);
                    carried_usage.output_tokens = carried_usage
                        .output_tokens
                        .saturating_add(usage.output_tokens);
                    if escalations >= MAX_CEILING_ESCALATIONS {
                        return Err(ProviderError::OutputTruncated {
                            message,
                            usage: carried_usage,
                        });
                    }
                    escalations += 1;
                    let raised = ceiling.saturating_mul(4);
                    log::warn!(
                        "{schema_name}: output ceiling {ceiling} truncated the response, \
retrying with {raised} output tokens"
                    );
                    ceiling = raised;
                    continue;
                }
                Err(err) => return Err(err),
            };
        attempts += 1;
        match serde_json::from_value::<T>(response.content.clone()) {
            Ok(parsed) => {
                let mut usage = response.usage;
                usage.input_tokens = usage
                    .input_tokens
                    .saturating_add(carried_usage.input_tokens);
                usage.cached_input_tokens = usage
                    .cached_input_tokens
                    .saturating_add(carried_usage.cached_input_tokens);
                usage.output_tokens = usage
                    .output_tokens
                    .saturating_add(carried_usage.output_tokens);
                return Ok((parsed, usage));
            }
            Err(err) => {
                last_error = Some(err);
                if attempts >= SCHEMA_ATTEMPTS {
                    break;
                }
            }
        }
    }
    Err(ProviderError::Malformed {
        message: format!(
            "{schema_name} response failed schema validation {} times: {}",
            SCHEMA_ATTEMPTS,
            last_error
                .map(|err| err.to_string())
                .unwrap_or_else(|| "unknown".to_string())
        ),
    })
}

fn deep_dive_task(lens: &str) -> String {
    format!(
        "Deep dive this file cluster through the {lens} lens. Report only defects you \
can anchor to exact lines in the diff, each with the concrete harm merging would \
cause and a suggested fix when one can be expressed."
    )
}

/// Lenses for a cluster: triage suggestions intersected with enabled
/// lenses. A cluster triage left unmentioned defaults to correctness when
/// enabled; a cluster triage marked with no usable lens receives none.
fn select_lenses(
    cluster: &crate::ingest::Cluster,
    lens_map: &HashMap<String, Vec<String>>,
    lenses: &Lenses,
) -> Vec<String> {
    let enabled = |name: &str| match name {
        "correctness" => lenses.correctness,
        "security" => lenses.security,
        "performance" => lenses.performance,
        "style" => lenses.style,
        _ => false,
    };
    match lens_map.get(&cluster.path) {
        Some(names) => names.iter().filter(|name| enabled(name)).cloned().collect(),
        None if lenses.correctness => vec!["correctness".to_string()],
        None => Vec::new(),
    }
}

fn shrunk_diff_text(input: &PipelineInput) -> String {
    let mut text = String::new();
    for cluster in input.ingestion.clusters.iter().take(5) {
        text.push_str(&format!("File: {}\n", cluster.path));
        for hunk in cluster.hunks.iter().take(2) {
            text.push_str(&hunk.render());
        }
    }
    text
}

fn estimate_prompt_tokens(prompt: &str) -> u64 {
    estimate_tokens(prompt)
}

fn gate_is_stood_down(degradations: &[Degradation]) -> bool {
    degradations
        .iter()
        .any(|degradation| matches!(degradation, Degradation::SummaryOnly { .. }))
}

fn severity_word(severity: crate::config::Severity) -> &'static str {
    match severity {
        crate::config::Severity::Blocker => "blocker",
        crate::config::Severity::Warning => "warning",
        crate::config::Severity::Note => "note",
    }
}

fn skip_notice(input: &PipelineInput, gate: &BudgetGate) -> String {
    let carried: Vec<String> = input
        .carried_findings
        .iter()
        .map(|finding| finding.location())
        .collect();
    format!(
        "Review skipped: the remaining budget ({:.4} USD) cannot fund any pass. \
No findings were claimed and no review was performed.\n\
Cumulative spend for this pull request: {:.4} USD (earlier runs: {:.4}).\n\
{}",
        gate.remaining(),
        gate.spent_this_run() + input.prior_spend,
        input.prior_spend,
        if carried.is_empty() {
            "No unresolved findings are carried forward from earlier runs.".to_string()
        } else {
            format!(
                "Unresolved findings from earlier runs still stand at: {}.",
                carried.join(", ")
            )
        }
    )
}

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
use budget::{BudgetGate, Degradation, LadderDecision, PassEstimate};
use futures::StreamExt;
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

/// Absolute output ceiling. Escalation never asks for more than any
/// current model will grant, because a ceiling past the model's own limit
/// turns a truncation into a rejection.
const MAX_OUTPUT_CEILING: u32 = 64_000;

/// How many deep dives may fail against the provider before the run is
/// treated as broken rather than degraded.
const MAX_FAILED_DIVES: u32 = 2;

/// A provider replaying recorded responses. Used by the fixture harness for
/// end-to-end pipeline tests and for offline replay runs.
pub struct RecordedProvider {
    steps: std::sync::Mutex<std::collections::VecDeque<Result<Value, ProviderError>>>,
    /// Responses matched to a marker in the request rather than to call
    /// order. Concurrency makes call order arbitrary, so a fixture that
    /// models per-cluster answers has to key on the request.
    keyed: Vec<(String, Result<Value, ProviderError>, std::time::Duration)>,
    seen: std::sync::Mutex<Vec<CompletionRequest>>,
    /// Calls currently inside `complete`, and the high water mark. A
    /// fixture can assert that a concurrency limit was actually honored.
    in_flight: std::sync::atomic::AtomicUsize,
    peak_in_flight: std::sync::atomic::AtomicUsize,
    usage: TokenUsage,
}

impl RecordedProvider {
    /// Build from recorded steps, consumed in order. Each step is either a
    /// JSON response value or an error to raise.
    pub fn new(steps: Vec<Result<Value, ProviderError>>) -> RecordedProvider {
        RecordedProvider {
            steps: std::sync::Mutex::new(steps.into()),
            keyed: Vec::new(),
            seen: std::sync::Mutex::new(Vec::new()),
            in_flight: std::sync::atomic::AtomicUsize::new(0),
            peak_in_flight: std::sync::atomic::AtomicUsize::new(0),
            usage: TokenUsage {
                input_tokens: 100,
                cached_input_tokens: 0,
                output_tokens: 20,
            },
        }
    }

    /// Build from responses keyed by a marker that must appear in the
    /// request, each with a delay before it answers. The delay lets a
    /// fixture force completions to arrive in a different order than the
    /// calls were made.
    pub fn keyed(
        responses: Vec<(String, Result<Value, ProviderError>, std::time::Duration)>,
    ) -> RecordedProvider {
        RecordedProvider {
            steps: std::sync::Mutex::new(std::collections::VecDeque::new()),
            keyed: responses,
            seen: std::sync::Mutex::new(Vec::new()),
            in_flight: std::sync::atomic::AtomicUsize::new(0),
            peak_in_flight: std::sync::atomic::AtomicUsize::new(0),
            usage: TokenUsage {
                input_tokens: 100,
                cached_input_tokens: 0,
                output_tokens: 20,
            },
        }
    }

    /// Requests this provider was asked to complete, in order.
    pub fn requests(&self) -> Vec<CompletionRequest> {
        self.seen.lock().expect("recorded requests lock").clone()
    }

    /// The most calls this provider ever had in flight at once.
    pub fn peak_in_flight(&self) -> usize {
        self.peak_in_flight
            .load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// Decrements the in-flight count however the call leaves.
struct InFlight<'a>(&'a std::sync::atomic::AtomicUsize);

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

impl crate::provider::Provider for RecordedProvider {
    async fn complete(
        &self,
        request: &CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        use std::sync::atomic::Ordering;
        self.seen
            .lock()
            .expect("recorded requests lock")
            .push(request.clone());
        let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak_in_flight.fetch_max(now, Ordering::SeqCst);
        let _guard = InFlight(&self.in_flight);
        if !self.keyed.is_empty() {
            let (_, response, delay) = self
                .keyed
                .iter()
                .find(|(marker, _, _)| request.user.contains(marker.as_str()))
                .ok_or_else(|| ProviderError::Malformed {
                    message: "no recorded response matches this request".to_string(),
                    usage: TokenUsage::default(),
                })?;
            if !delay.is_zero() {
                tokio::time::sleep(*delay).await;
            }
            return match response {
                Ok(content) => Ok(CompletionResponse {
                    content: content.clone(),
                    usage: self.usage,
                    reported_cost: None,
                    reported_model: None,
                }),
                Err(error) => Err(ProviderError::Rejected {
                    message: error.to_string(),
                }),
            };
        }
        let step = self
            .steps
            .lock()
            .expect("recorded steps lock")
            .pop_front()
            .ok_or_else(|| ProviderError::Malformed {
                message: "no more recorded steps".to_string(),
                usage: TokenUsage::default(),
            })?;
        match step {
            Ok(content) => Ok(CompletionResponse {
                content,
                usage: self.usage,
                reported_cost: None,
                reported_model: None,
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
    /// A declared rule could not be applied. Configuration validation
    /// normally catches this first.
    #[error("review rules cannot be applied: {0}")]
    Rules(String),
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
    /// The checkout retrieval may read from. None means there is no
    /// repository on disk for this run, so nothing can be retrieved.
    pub repo_root: Option<std::path::PathBuf>,
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
    /// True when this pass was served from the resume cache, so its cost
    /// was paid by an earlier attempt rather than by this run.
    pub resumed: bool,
    /// The model this pass actually called. A pass the budget downgraded
    /// names the cheaper model it used, not the role's configured one.
    pub model: String,
    /// How the cost figure was computed, disclosed in the spend section.
    pub cost_source: CostSource,
}

/// How a pass's recorded cost figure was computed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CostSource {
    /// Token counts priced at the configured rates: the ordinary HTTP pass.
    TokenPrice,
    /// Token counts at the configured rates because the agent transport
    /// reported no cost figure of its own.
    AgentTokenPrice,
    /// The figure the agent's own accounting reported.
    AgentReported,
}

impl CostSource {
    /// Derive the disclosure from what the response carried.
    pub fn from_response(reported_cost: Option<f64>, reported_model: Option<&str>) -> CostSource {
        if reported_cost.is_some() {
            CostSource::AgentReported
        } else if reported_model.is_some() {
            CostSource::AgentTokenPrice
        } else {
            CostSource::TokenPrice
        }
    }
}

/// Spend for one run.
#[derive(Debug, Clone)]
pub struct RunSpend {
    /// Per-pass spend in execution order.
    pub passes: Vec<PassSpend>,
    /// Spend recorded by earlier runs on this pull request.
    pub prior_spend: f64,
    /// What this run paid to providers.
    pub paid: f64,
    /// What an earlier attempt paid for passes this run resumed. Those
    /// dollars were spent, so they keep counting.
    pub inherited: f64,
    /// Paid plus inherited.
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
    /// No review could be produced.
    Skipped {
        /// The explanatory notice.
        notice: String,
        /// Metadata rule violations, which are evaluated without a
        /// provider call and therefore survive a run that funds no pass.
        violations: Vec<findings::Finding>,
    },
    /// The run failed after some passes had already run and been billed.
    /// The recorded spend travels out so it can be persisted before the
    /// error is reported.
    Failed {
        /// Why the run failed.
        error: PipelineError,
        /// Spend recorded up to the failure.
        spend: Vec<PassSpend>,
    },
}

/// Run the pipeline for the configured profile.
pub async fn run(
    registry: &ProviderRegistry,
    config: &Config,
    input: &PipelineInput,
) -> Result<RunOutcome, PipelineError> {
    // Rules are mechanical and free, so they are evaluated before the
    // budget is consulted. A run that can afford no pass still knows
    // whether the pull request itself breaks a rule.
    let rules = crate::rules::Rules::compile(&config.review)
        .map_err(|err| PipelineError::Rules(err.to_string()))?;
    // Rules judge what the author claimed. A run with no pull request has
    // only the label demur wrote for its own output, which is not a claim
    // and must never be measured against a rule.
    let rules_apply = input.meta.origin.carries_an_authored_claim();
    let rules_skipped = !rules.is_empty() && !rules_apply;
    let violations = if rules.is_empty() || !rules_apply {
        if rules_skipped {
            log::info!("metadata rules: not evaluated, this run has no pull request");
        }
        Vec::new()
    } else {
        let found = rules.evaluate(&input.meta.title, &input.meta.description);
        log::info!("metadata rules: {} violation(s)", found.len());
        found
    };

    let store = open_cache(config);
    let store_ref: Option<&dyn crate::cache::Store> = store
        .as_ref()
        .map(|store| store as &dyn crate::cache::Store);
    let triage_cache = PassCache::for_role(store_ref, config, &config.models.triage);
    let deep_cache = PassCache::for_role(store_ref, config, &config.models.deep);

    let profile = config.profile.unwrap_or(Profile::Standard);
    let triage_price = ModelPrice::from_model(&config.models.triage);
    let deep_price = ModelPrice::from_model(&config.models.deep);
    let verdict_price = ModelPrice::from_model(&config.models.verdict);
    let mut gate = BudgetGate::new(config.budget.cap(), input.prior_spend);
    let mut degradations: Vec<Degradation> = Vec::new();
    let mut spend: Vec<PassSpend> = Vec::new();
    let mut all_findings: Vec<findings::Finding> = violations.clone();
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
            rules_skipped,
        )
        .await?;
        return Ok(RunOutcome::Review(Box::new(review)));
    }

    // Triage.
    let context = prompt::repository_context(&input.meta, &rendered_clusters(input));
    let shrunk_context = prompt::repository_context(&input.meta, &shrunk_diff_text(input));
    let triage_task = "Triage the changed hunks. For each file cluster, suggest review \
lenses from: correctness, security, performance, style. Also report any finding \
you can already anchor to an exact file and line range with its concrete harm.";
    let triage_schema = prompt::triage_schema();
    let triage_full = prompt::assemble(&context, triage_task);
    let triage_shrunk = prompt::assemble(&shrunk_context, triage_task);
    log::info!(
        "triage: ~{} input tokens estimated on {}",
        estimate_prompt(&triage_full, &triage_schema),
        config.models.triage.name
    );
    let triage_started = std::time::Instant::now();
    let estimate = PassEstimate {
        pass: "triage",
        full_tokens: estimate_prompt(&triage_full, &triage_schema),
        shrunk_tokens: estimate_prompt(&triage_shrunk, &triage_schema),
        max_output_tokens: config.limits.max_tokens,
        price: &triage_price,
        downgrade_price: None,
    };
    let (triage_prompt, ran_shrunk, triage_hold) = match gate.authorize(estimate) {
        LadderDecision::Run {
            shrink,
            degradations: disclosed,
            hold,
            ..
        } => {
            degradations.extend(disclosed);
            (
                if shrink { triage_shrunk } else { triage_full },
                shrink,
                hold,
            )
        }
        LadderDecision::StandDown => {
            return Ok(RunOutcome::Skipped {
                notice: skip_notice(input, &gate, &violations, rules_skipped),
                violations,
            });
        }
    };
    let triage_result = match call_pass::<findings::TriageOutput>(
        &registry.triage,
        triage_prompt,
        triage_schema.clone(),
        "triage",
        config.limits.max_tokens,
        triage_cache,
    )
    .await
    {
        Err(ProviderError::ContextOverflow { .. }) if !ran_shrunk => {
            // The full rendering cannot be reviewed, so retry once at the
            // shrink rung's width before giving up on the run.
            call_pass::<findings::TriageOutput>(
                &registry.triage,
                prompt::assemble(&shrunk_context, triage_task),
                triage_schema,
                "triage",
                config.limits.max_tokens,
                triage_cache,
            )
            .await
            .inspect(|_| {
                degradations.push(Degradation::ContextShrunk {
                    pass: "triage".to_string(),
                });
            })
        }
        other => other,
    };
    let triage_result = match triage_result {
        Ok(result) => result,
        Err(err) => {
            gate.release(triage_hold);
            let mut spend = Vec::new();
            if let Some(usage) = billed_usage(&err) {
                spend.push(PassSpend {
                    pass: "triage (failed)".to_string(),
                    usage,
                    cost: gate.record(&usage, &triage_price),
                    resumed: false,
                    model: config.models.triage.name.clone(),
                    cost_source: CostSource::TokenPrice,
                });
            }
            return Ok(RunOutcome::Failed {
                error: err.into(),
                spend,
            });
        }
    };
    let (triage_output, usage, resumed) = (
        triage_result.output,
        triage_result.usage,
        triage_result.resumed,
    );
    let triage_reported = (triage_result.reported_cost, triage_result.reported_model);
    spend.push(PassSpend {
        pass: "triage".to_string(),
        usage,
        cost: settle_pass(
            &mut gate,
            triage_hold,
            &usage,
            &triage_price,
            resumed,
            triage_reported.0,
        ),
        resumed,
        model: triage_reported
            .1
            .clone()
            .unwrap_or_else(|| config.models.triage.name.clone()),
        cost_source: CostSource::from_response(triage_reported.0, triage_reported.1.as_deref()),
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

    // Deep dives. Planned in full before any of them runs, so the ceiling
    // bounds what is launched rather than what has finished, then executed
    // concurrently because no dive reads another's output.
    let mut unreviewed: Vec<String> = Vec::new();
    if profile != Profile::Quick {
        let ceiling = config.limits.deep_calls as usize;
        let mut planned: Vec<PlannedDive<'_>> = Vec::new();
        for cluster in &input.ingestion.clusters {
            let lenses = select_lenses(cluster, &lens_map, &config.lenses);
            if lenses.is_empty() {
                continue;
            }
            for lens in lenses {
                if planned.len() >= ceiling {
                    if !unreviewed.contains(&cluster.path) {
                        unreviewed.push(cluster.path.clone());
                    }
                    continue;
                }
                planned.push(PlannedDive {
                    index: planned.len(),
                    cluster,
                    lens,
                });
            }
        }
        log::info!(
            "deep dives: {} planned from {} cluster(s), ceiling {}, concurrency {}",
            planned.len(),
            input.ingestion.clusters.len(),
            ceiling,
            config.limits.concurrency
        );

        let gate_cell = std::sync::Mutex::new(std::mem::replace(
            &mut gate,
            BudgetGate::new(config.budget.cap(), input.prior_spend),
        ));
        let stop = std::sync::atomic::AtomicBool::new(false);
        let failures = std::sync::atomic::AtomicU32::new(0);
        // Retrieval is off unless asked for, and a repository root is only
        // available where the run has a checkout to read.
        let retriever = if config.retrieval.enabled {
            input
                .repo_root
                .as_deref()
                .and_then(|root| crate::retrieval::Retriever::new(root, config))
        } else {
            None
        };
        let retrieval_budget = std::sync::Mutex::new(config.retrieval.max_bytes());

        let dives = planned.iter().map(|dive| {
            run_deep_dive(
                dive,
                config,
                registry,
                input,
                &diff_paths,
                &gate_cell,
                &deep_price,
                &triage_price,
                triage_cache,
                deep_cache,
                &stop,
                &failures,
                retriever.as_ref(),
                &retrieval_budget,
            )
        });
        let mut outcomes: Vec<Option<DiveOutcome>> = (0..planned.len()).map(|_| None).collect();
        let mut stream = futures::stream::iter(dives)
            .buffer_unordered(config.limits.concurrency.max(1) as usize);
        while let Some(outcome) = stream.next().await {
            let index = outcome.index;
            outcomes[index] = Some(outcome);
        }
        drop(stream);
        gate = gate_cell.into_inner().expect("budget gate lock");

        // Results are placed by position, so the sequence entering
        // synthesis is the one a serial run would have produced.
        let mut stood_down = false;
        for outcome in outcomes.into_iter().flatten() {
            spend.extend(outcome.spend);
            degradations.extend(outcome.degradations);
            if outcome.stood_down {
                stood_down = true;
            }
            all_findings.extend(outcome.findings);
            if let Some(err) = outcome.fatal {
                return Ok(RunOutcome::Failed {
                    error: err.into(),
                    spend: std::mem::take(&mut spend),
                });
            }
        }
        if stood_down {
            degradations.push(Degradation::SummaryOnly {
                skipped: vec!["remaining deep dives".to_string()],
            });
        }
        if failures.load(std::sync::atomic::Ordering::SeqCst) > MAX_FAILED_DIVES {
            return Ok(RunOutcome::Failed {
                error: PipelineError::Provider(ProviderError::Rejected {
                    message: format!(
                        "{} deep dives failed against the provider; the run stopped rather than \
publishing coverage it could not establish",
                        failures.load(std::sync::atomic::Ordering::SeqCst)
                    ),
                }),
                spend: std::mem::take(&mut spend),
            });
        }
        if !unreviewed.is_empty() {
            degradations.push(Degradation::DeepCallsCapped { unreviewed });
        }
    }

    // Cross-examination, deep profile only.
    if profile == Profile::Deep {
        let context = prompt::repository_context(&input.meta, &rendered_clusters(input));
        let shrunk_context = prompt::repository_context(&input.meta, &shrunk_diff_text(input));
        let task = "Cross-examine this pull request adversarially: adversarial inputs, \
rollback safety, concurrency hazards, migration safety, and breaking interface \
changes. Report only findings you can anchor to an exact file and line range \
with their concrete harm.";
        let cross_schema = prompt::cross_examination_schema();
        let full_prompt = prompt::assemble(&context, task);
        let shrunk_prompt = prompt::assemble(&shrunk_context, task);
        let estimate = PassEstimate {
            pass: "cross-examination",
            full_tokens: estimate_prompt(&full_prompt, &cross_schema),
            shrunk_tokens: estimate_prompt(&shrunk_prompt, &cross_schema),
            max_output_tokens: config.limits.max_tokens,
            price: &deep_price,
            downgrade_price: Some(&triage_price),
        };
        // The decision for this pass, not the accumulated degradation list,
        // decides whether it runs. An earlier stand-down among the deep
        // dives must not be reported as if cross-examination had been
        // priced and refused on its own.
        let plan = match gate.authorize(estimate) {
            LadderDecision::Run {
                shrink,
                downgrade,
                degradations: disclosed,
                hold,
            } => {
                degradations.extend(disclosed);
                Some((shrink, downgrade, hold))
            }
            LadderDecision::StandDown => {
                degradations.push(Degradation::SummaryOnly {
                    skipped: vec!["cross-examination".to_string()],
                });
                None
            }
        };
        if let Some((shrink, downgrade, cross_hold)) = plan {
            let provider = if downgrade {
                &registry.triage
            } else {
                &registry.deep
            };
            let paid_price = if downgrade {
                &triage_price
            } else {
                &deep_price
            };
            let model = if downgrade {
                &config.models.triage.name
            } else {
                &config.models.deep.name
            };
            log::info!("cross-examination: running on {model}");
            let cross_started = std::time::Instant::now();
            let chosen = if shrink { shrunk_prompt } else { full_prompt };
            let cross = call_pass::<findings::ModelFindings>(
                provider,
                chosen,
                cross_schema,
                "cross-examination",
                config.limits.max_tokens,
                if downgrade { triage_cache } else { deep_cache },
            )
            .await;
            // A failed pass releases its hold and contributes no spend
            // line, because it spent nothing.
            let cross_output = match cross {
                Ok(result) => {
                    spend.push(PassSpend {
                        pass: "cross-examination".to_string(),
                        usage: result.usage,
                        cost: settle_pass(
                            &mut gate,
                            cross_hold,
                            &result.usage,
                            paid_price,
                            result.resumed,
                            result.reported_cost,
                        ),
                        resumed: result.resumed,
                        model: result
                            .reported_model
                            .clone()
                            .unwrap_or_else(|| model.clone()),
                        cost_source: CostSource::from_response(
                            result.reported_cost,
                            result.reported_model.as_deref(),
                        ),
                    });
                    result.output
                }
                Err(err @ ProviderError::Auth { .. }) => {
                    gate.release(cross_hold);
                    return Ok(RunOutcome::Failed {
                        error: err.into(),
                        spend: std::mem::take(&mut spend),
                    });
                }
                Err(err) => {
                    gate.release(cross_hold);
                    log::warn!("cross-examination failed: {err}");
                    if let Some(usage) = billed_usage(&err) {
                        spend.push(PassSpend {
                            pass: "cross-examination (failed)".to_string(),
                            usage,
                            cost: gate.record(&usage, paid_price),
                            resumed: false,
                            model: model.clone(),
                            cost_source: CostSource::TokenPrice,
                        });
                    }
                    degradations.push(Degradation::PassFailed {
                        pass: "cross-examination".to_string(),
                        reason: err.to_string(),
                    });
                    findings::ModelFindings::default()
                }
            };
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
        rules_skipped,
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
    rules_skipped: bool,
) -> Result<Review, PipelineError> {
    let mut all = findings;
    all.extend(input.carried_findings.iter().cloned());
    // The verdict pass sees the prepared set, so duplicated defects never
    // overweigh the summary prose; synthesis re-prepares idempotently.
    let all = findings::prepare(all);

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
        let summary_prompt = prompt::assemble(&context, task);
        let summary_schema = summary_schema();
        let decision = gate.authorize(PassEstimate {
            pass: "verdict summary",
            full_tokens: estimate_prompt(&summary_prompt, &summary_schema),
            shrunk_tokens: estimate_prompt(&summary_prompt, &summary_schema),
            max_output_tokens: config.limits.max_tokens,
            price: verdict_price,
            downgrade_price: Some(&cheap_verdict_price),
        });
        match decision {
            LadderDecision::Run {
                downgrade,
                degradations: disclosed,
                hold,
                ..
            } => {
                degradations.extend(disclosed);
                let provider = if downgrade {
                    &registry.triage
                } else {
                    &registry.verdict
                };
                let paid_price = if downgrade {
                    &cheap_verdict_price
                } else {
                    verdict_price
                };
                // This pass drafts prose only. The verdict and every
                // finding are already settled, so a failure here costs a
                // paragraph, never the run that paid for the deep dives.
                let store = open_cache(config);
                let store_ref: Option<&dyn crate::cache::Store> = store
                    .as_ref()
                    .map(|store| store as &dyn crate::cache::Store);
                let summary_cache = PassCache::for_role(
                    store_ref,
                    config,
                    if downgrade {
                        &config.models.triage
                    } else {
                        &config.models.verdict
                    },
                );
                match call_pass::<SummaryOutput>(
                    provider,
                    summary_prompt,
                    summary_schema,
                    "verdict summary",
                    config.limits.max_tokens,
                    summary_cache,
                )
                .await
                {
                    Ok(result) => {
                        let configured = if downgrade {
                            config.models.triage.name.clone()
                        } else {
                            config.models.verdict.name.clone()
                        };
                        spend.push(PassSpend {
                            pass: "verdict summary".to_string(),
                            usage: result.usage,
                            cost: settle_pass(
                                &mut *gate,
                                hold,
                                &result.usage,
                                paid_price,
                                result.resumed,
                                result.reported_cost,
                            ),
                            resumed: result.resumed,
                            model: result.reported_model.clone().unwrap_or(configured),
                            cost_source: CostSource::from_response(
                                result.reported_cost,
                                result.reported_model.as_deref(),
                            ),
                        });
                        Some(result.output.summary)
                    }
                    Err(err) => {
                        log::warn!("verdict summary failed, publishing without it: {err}");
                        gate.release(hold);
                        if let Some(usage) = billed_usage(&err) {
                            spend.push(PassSpend {
                                pass: "verdict summary (failed)".to_string(),
                                usage,
                                cost: gate.record(&usage, paid_price),
                                resumed: false,
                                model: if downgrade {
                                    config.models.triage.name.clone()
                                } else {
                                    config.models.verdict.name.clone()
                                },
                                cost_source: CostSource::TokenPrice,
                            });
                        }
                        degradations.push(Degradation::SummaryUnavailable {
                            reason: err.to_string(),
                        });
                        None
                    }
                }
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
            .map(|pass_spend| {
                (
                    pass_spend.pass.clone(),
                    pass_spend.cost,
                    pass_spend.resumed,
                    pass_spend.cost_source,
                )
            })
            .collect(),
        prior_spend: input.prior_spend,
        summary,
        rules_skipped,
        template: config.review.template.clone(),
        models: spend
            .iter()
            .map(|pass| (pass.pass.clone(), pass.model.clone()))
            .collect(),
    });
    Ok(Review {
        verdict: synthesis.verdict,
        published: synthesis.published,
        omitted: synthesis.omitted,
        beyond_budget: synthesis.beyond_budget,
        body: synthesis.body,
        degradations: degradations.clone(),
        spend: RunSpend {
            paid: spend
                .iter()
                .filter(|pass| !pass.resumed)
                .map(|pass| pass.cost)
                .sum(),
            inherited: spend
                .iter()
                .filter(|pass| pass.resumed)
                .map(|pass| pass.cost)
                .sum(),
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

/// What a pass needs to consult the resume cache: somewhere to look, and
/// the model identity that, with the request, decides whether an entry
/// answers this pass's question.
#[derive(Clone, Copy)]
pub struct PassCache<'a> {
    /// Where entries live. None disables the cache for this pass.
    pub store: Option<&'a dyn crate::cache::Store>,
    /// The model this pass will actually call.
    pub model: &'a crate::config::ModelDef,
}

impl<'a> PassCache<'a> {
    /// A pass cache for one role. The agent family never participates: its
    /// configured name cannot vouch for the model the login actually ran,
    /// so a cache hit could answer with another model's work.
    pub fn for_role(
        store: Option<&'a dyn crate::cache::Store>,
        config: &Config,
        model: &'a crate::config::ModelDef,
    ) -> PassCache<'a> {
        let keyless = config
            .providers
            .get(&model.provider)
            .is_some_and(|provider| provider.family == crate::config::Family::ClaudeCode);
        PassCache {
            store: if keyless { None } else { store },
            model,
        }
    }
}

/// Outcome of one pass: its output, its usage, and whether the cache
/// supplied it rather than the provider.
struct PassResult<T> {
    output: T,
    usage: TokenUsage,
    resumed: bool,
    reported_cost: Option<f64>,
    reported_model: Option<String>,
}

/// One provider call with schema-validated output and bounded retries.
/// Returns the parsed output and the usage the call reported.
async fn call_pass<T: DeserializeOwned>(
    provider: &crate::provider::AnyProvider,
    prompt: prompt::Prompt,
    schema: Value,
    schema_name: &str,
    max_output: u32,
    cache: PassCache<'_>,
) -> Result<PassResult<T>, ProviderError> {
    let make_request = {
        let system = prompt.system.clone();
        let user = prompt.user.clone();
        let schema = schema.clone();
        let schema_name = schema_name.to_string();
        move |ceiling: u32, correction: Option<&str>| CompletionRequest {
            system: system.clone(),
            user: match correction {
                Some(problem) => format!(
                    "{user}\n\nYour previous response did not match the schema: {problem}\n\
Respond again with only a JSON object that matches it exactly."
                ),
                None => user.clone(),
            },
            schema: schema.clone(),
            schema_name: schema_name.clone(),
            max_output_tokens: ceiling,
        }
    };
    // The key is the request this pass starts from. Ceiling escalation is
    // an internal retry, so a run that escalated still stores its result
    // under the question it originally asked.
    let initial = make_request(max_output, None);
    let key = cache
        .store
        .map(|_| crate::cache::CacheKey::new(&initial, cache.model));
    if let (Some(store), Some(key)) = (cache.store, key.as_ref())
        && let Some(entry) = store.get(key)
    {
        // A retrieved entry is validated exactly as a live response is, so
        // the strongest thing a bad entry can do is what a bad model
        // response can already do.
        match serde_json::from_value::<T>(entry.content.clone()) {
            Ok(parsed) => {
                log::info!("{schema_name}: resumed from cache");
                return Ok(PassResult {
                    output: parsed,
                    usage: entry.usage(),
                    resumed: true,
                    reported_cost: None,
                    reported_model: None,
                });
            }
            Err(err) => {
                log::warn!(
                    "{schema_name}: cached entry failed validation, running the pass: {err}"
                );
            }
        }
    }

    let mut ceiling = max_output;
    let mut escalations: u32 = 0;
    let mut attempts: u32 = 0;
    let mut carried_usage = TokenUsage::default();
    // Attempts discarded for failing schema validation were billed all the
    // same, so their usage travels with the pass however it ends.
    let mut schema_usage = TokenUsage::default();
    let mut last_error: Option<serde_json::Error> = None;
    loop {
        let correction = last_error.as_ref().map(|err| err.to_string());
        let request = make_request(ceiling, correction.as_deref());
        let response =
            match complete_with_retries(provider, &request, &RetryPolicy::default()).await {
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
                    if escalations >= MAX_CEILING_ESCALATIONS || ceiling >= MAX_OUTPUT_CEILING {
                        return Err(ProviderError::OutputTruncated {
                            message,
                            usage: carried_usage,
                        });
                    }
                    escalations += 1;
                    let raised = ceiling.saturating_mul(4).min(MAX_OUTPUT_CEILING);
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
                    .saturating_add(carried_usage.input_tokens)
                    .saturating_add(schema_usage.input_tokens);
                usage.cached_input_tokens = usage
                    .cached_input_tokens
                    .saturating_add(carried_usage.cached_input_tokens)
                    .saturating_add(schema_usage.cached_input_tokens);
                usage.output_tokens = usage
                    .output_tokens
                    .saturating_add(carried_usage.output_tokens)
                    .saturating_add(schema_usage.output_tokens);
                // Only a completed, schema-valid pass is stored. A failure
                // anywhere above leaves nothing behind to resume from.
                if let (Some(store), Some(key)) = (cache.store, key.as_ref()) {
                    store.put(key, &crate::cache::Entry::new(response.content, &usage));
                }
                return Ok(PassResult {
                    output: parsed,
                    usage,
                    resumed: false,
                    reported_cost: response.reported_cost,
                    reported_model: response.reported_model,
                });
            }
            Err(err) => {
                schema_usage.input_tokens = schema_usage
                    .input_tokens
                    .saturating_add(response.usage.input_tokens);
                schema_usage.cached_input_tokens = schema_usage
                    .cached_input_tokens
                    .saturating_add(response.usage.cached_input_tokens);
                schema_usage.output_tokens = schema_usage
                    .output_tokens
                    .saturating_add(response.usage.output_tokens);
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
        usage: schema_usage,
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
        Some(names) => {
            // A triage response may name a lens twice; two identical dives
            // would only double the spend under one name.
            let mut chosen: Vec<String> =
                names.iter().filter(|name| enabled(name)).cloned().collect();
            chosen.sort();
            chosen.dedup();
            chosen
        }
        None if lenses.correctness => vec!["correctness".to_string()],
        None => Vec::new(),
    }
}

/// The full ingested pull request, rendered the way every pass sees it:
/// one heading per cluster, then its hunks. The raw diff text never
/// enters a prompt.
fn rendered_clusters(input: &PipelineInput) -> String {
    prompt::clusters_text(
        input
            .ingestion
            .clusters
            .iter()
            .map(|cluster| (cluster.path.as_str(), cluster.hunks.as_slice())),
    )
}

fn shrunk_diff_text(input: &PipelineInput) -> String {
    prompt::clusters_text(input.ingestion.clusters.iter().take(5).map(|cluster| {
        let width = cluster.hunks.len().min(2);
        (cluster.path.as_str(), &cluster.hunks[..width])
    }))
}

/// Open the resume cache when configuration asks for one. A location that
/// cannot be opened yields no cache rather than an error, because a cache
/// is never worth failing a run over.
fn open_cache(config: &Config) -> Option<crate::cache::FsStore> {
    if !config.cache.enabled {
        return None;
    }
    let dir = config.cache.dir.as_ref()?;
    let store = crate::cache::FsStore::open(dir, config.cache.max_age(), config.cache.max_bytes());
    if store.is_none() {
        log::warn!(
            "cache directory {} is unusable; running cold",
            dir.display()
        );
    }
    store
}

/// One deep dive decided before any of them runs.
struct PlannedDive<'a> {
    index: usize,
    cluster: &'a crate::ingest::Cluster,
    lens: String,
}

/// What one deep dive produced. Collected by position so the order
/// entering synthesis never depends on which call returned first.
struct DiveOutcome {
    index: usize,
    findings: Vec<findings::Finding>,
    spend: Vec<PassSpend>,
    degradations: Vec<Degradation>,
    /// The budget stood down on this dive, so no further dive should run.
    stood_down: bool,
    /// A failure no amount of degrading survives, such as a bad key.
    fatal: Option<ProviderError>,
}

/// Run one planned deep dive. Everything it touches is either its own or
/// shared behind a lock held only for bookkeeping, never across a call.
#[allow(clippy::too_many_arguments)]
async fn run_deep_dive(
    dive: &PlannedDive<'_>,
    config: &Config,
    registry: &ProviderRegistry,
    input: &PipelineInput,
    diff_paths: &[String],
    gate: &std::sync::Mutex<BudgetGate>,
    deep_price: &ModelPrice,
    triage_price: &ModelPrice,
    triage_cache: PassCache<'_>,
    deep_cache: PassCache<'_>,
    stop: &std::sync::atomic::AtomicBool,
    failures: &std::sync::atomic::AtomicU32,
    retriever: Option<&crate::retrieval::Retriever>,
    retrieval_budget: &std::sync::Mutex<usize>,
) -> DiveOutcome {
    use std::sync::atomic::Ordering;

    let empty = |stood_down: bool| DiveOutcome {
        index: dive.index,
        findings: Vec::new(),
        spend: Vec::new(),
        degradations: Vec::new(),
        stood_down,
        fatal: None,
    };
    if stop.load(Ordering::SeqCst) {
        return empty(false);
    }

    let cluster = dive.cluster;
    let lens = &dive.lens;
    let context = prompt::cluster_context(&input.meta, &cluster.path, &cluster.hunks);
    let shrunk_hunks: Vec<Hunk> = cluster.hunks.iter().take(1).cloned().collect();
    let shrunk = prompt::cluster_context(&input.meta, &cluster.path, &shrunk_hunks);
    let task = deep_dive_task(lens);
    let schema = prompt::findings_schema();
    let full_prompt = prompt::assemble(&context, &task);
    let shrunk_prompt = prompt::assemble(&shrunk, &task);

    // The lock is held for the decision only, never across the call.
    let decision = {
        let mut gate = gate.lock().expect("budget gate lock");
        gate.authorize(PassEstimate {
            pass: "deep dive",
            full_tokens: estimate_prompt(&full_prompt, &schema),
            shrunk_tokens: estimate_prompt(&shrunk_prompt, &schema),
            max_output_tokens: config.limits.max_tokens,
            price: deep_price,
            downgrade_price: Some(triage_price),
        })
    };
    let (shrink, downgraded, hold, mut degradations) = match decision {
        LadderDecision::Run {
            shrink,
            downgrade,
            degradations,
            hold,
        } => (shrink, downgrade, hold, degradations),
        LadderDecision::StandDown => {
            stop.store(true, Ordering::SeqCst);
            return empty(true);
        }
    };

    log::info!("deep dive [{}]: {}", lens, cluster.path);
    let started = std::time::Instant::now();
    let provider = if downgraded {
        &registry.triage
    } else {
        &registry.deep
    };
    let pass_cache = if downgraded { triage_cache } else { deep_cache };
    let base_prompt = if shrink { shrunk_prompt } else { full_prompt };
    let dived = call_pass::<findings::ModelFindings>(
        provider,
        base_prompt.clone(),
        schema,
        "deep dive",
        config.limits.max_tokens,
        pass_cache,
    )
    .await;
    let dived = match dived {
        Err(ProviderError::ContextOverflow { .. }) if !shrink => {
            // Shrink to the first hunk and retry once.
            call_pass::<findings::ModelFindings>(
                provider,
                prompt::assemble(&shrunk, &task),
                prompt::findings_schema(),
                "deep dive",
                config.limits.max_tokens,
                pass_cache,
            )
            .await
            .inspect(|_| {
                degradations.push(Degradation::ContextShrunk {
                    pass: format!("deep dive ({lens})"),
                });
            })
        }
        other => other,
    };

    let result = match dived {
        Ok(result) => result,
        // A key problem repeats on every remaining cluster, so failing fast
        // beats burning the ceiling to learn it.
        Err(err @ ProviderError::Auth { .. }) => {
            gate.lock().expect("budget gate lock").release(hold);
            stop.store(true, Ordering::SeqCst);
            return DiveOutcome {
                fatal: Some(err),
                ..empty(false)
            };
        }
        Err(err) => {
            log::warn!("deep dive [{lens}] on {} failed: {err}", cluster.path);
            degradations.push(Degradation::PassFailed {
                pass: format!("deep dive ({lens}) on {}", cluster.path),
                reason: err.to_string(),
            });
            let mut spend_lines = Vec::new();
            // The lock covers the release and the booking only.
            {
                let gate = &mut *gate.lock().expect("budget gate lock");
                gate.release(hold);
                if let Some(usage) = billed_usage(&err) {
                    let paid_price = if downgraded { triage_price } else { deep_price };
                    spend_lines.push(PassSpend {
                        pass: format!("deep dive {lens} on {} (failed)", cluster.path),
                        usage,
                        cost: gate.record(&usage, paid_price),
                        resumed: false,
                        model: if downgraded {
                            config.models.triage.name.clone()
                        } else {
                            config.models.deep.name.clone()
                        },
                        cost_source: CostSource::TokenPrice,
                    });
                }
            }
            if failures.fetch_add(1, Ordering::SeqCst) + 1 > MAX_FAILED_DIVES {
                stop.store(true, Ordering::SeqCst);
            }
            return DiveOutcome {
                degradations,
                spend: spend_lines,
                ..empty(false)
            };
        }
    };

    let paid_price = if downgraded { triage_price } else { deep_price };
    let cost = {
        let mut gate = gate.lock().expect("budget gate lock");
        settle_pass(
            &mut gate,
            hold,
            &result.usage,
            paid_price,
            result.resumed,
            result.reported_cost,
        )
    };
    let model = if downgraded {
        config.models.triage.name.clone()
    } else {
        config.models.deep.name.clone()
    };
    let mut spend_lines = vec![PassSpend {
        pass: format!("deep dive {lens} on {}", cluster.path),
        usage: result.usage,
        cost,
        resumed: result.resumed,
        model: result
            .reported_model
            .clone()
            .unwrap_or_else(|| model.clone()),
        cost_source: CostSource::from_response(
            result.reported_cost,
            result.reported_model.as_deref(),
        ),
    }];
    log::info!(
        "deep dive [{}]: {} finding(s) in {:.1?}, ${:.4}",
        lens,
        result.output.findings.len(),
        started.elapsed(),
        cost
    );

    // Retrieval rounds. The pass named what it wanted; the bot decides
    // what each name means and whether it is willing to read it.
    let mut result = result;
    let mut carried_prompt = base_prompt;
    if let Some(retriever) = retriever {
        let rounds = config.retrieval.max_rounds;
        for round in 1..=rounds {
            let requested = std::mem::take(&mut result.output.context_requests);
            if requested.is_empty() {
                break;
            }
            let resolved = resolve_requests(retriever, &requested, retrieval_budget);
            let attached = resolved
                .iter()
                .filter(|r| matches!(r, crate::retrieval::Resolution::Found { .. }))
                .count();
            log::info!(
                "deep dive [{}]: round {round} asked for {} item(s), {attached} attached",
                lens,
                requested.len()
            );
            // Attachments accumulate, so a later round still sees what an
            // earlier one attached. Only on the last permitted round must
            // the pass stop asking.
            let round_prompt = prompt::with_retrieved(&carried_prompt, &resolved, round == rounds);
            let decision = {
                let mut gate = gate.lock().expect("budget gate lock");
                gate.authorize(PassEstimate {
                    pass: "retrieval round",
                    full_tokens: estimate_prompt(&round_prompt, &prompt::findings_schema()),
                    shrunk_tokens: estimate_prompt(&round_prompt, &prompt::findings_schema()),
                    max_output_tokens: config.limits.max_tokens,
                    price: if downgraded { triage_price } else { deep_price },
                    downgrade_price: None,
                })
            };
            let LadderDecision::Run {
                hold: round_hold, ..
            } = decision
            else {
                // Retrieval must not be a way to spend outside the cap.
                degradations.push(Degradation::PassFailed {
                    pass: format!("retrieval round for deep dive ({lens}) on {}", cluster.path),
                    reason: "the remaining budget could not fund it".to_string(),
                });
                break;
            };
            match call_pass::<findings::ModelFindings>(
                provider,
                round_prompt.clone(),
                prompt::findings_schema(),
                "deep dive",
                config.limits.max_tokens,
                pass_cache,
            )
            .await
            {
                Ok(next) => {
                    let round_cost = {
                        let mut gate = gate.lock().expect("budget gate lock");
                        settle_pass(
                            &mut gate,
                            round_hold,
                            &next.usage,
                            paid_price,
                            next.resumed,
                            next.reported_cost,
                        )
                    };
                    spend_lines.push(PassSpend {
                        pass: format!("deep dive {lens} on {} (round {round})", cluster.path),
                        usage: next.usage,
                        cost: round_cost,
                        resumed: next.resumed,
                        model: next.reported_model.clone().unwrap_or_else(|| model.clone()),
                        cost_source: CostSource::from_response(
                            next.reported_cost,
                            next.reported_model.as_deref(),
                        ),
                    });
                    degradations.push(Degradation::ContextRetrieved {
                        pass: format!("deep dive ({lens}) on {}", cluster.path),
                        items: resolved.iter().map(|r| r.label().to_string()).collect(),
                        attached,
                    });
                    result = next;
                    carried_prompt = round_prompt;
                }
                Err(err) => {
                    gate.lock().expect("budget gate lock").release(round_hold);
                    log::warn!("deep dive [{lens}] retrieval round failed: {err}");
                    degradations.push(Degradation::PassFailed {
                        pass: format!("retrieval round for deep dive ({lens}) on {}", cluster.path),
                        reason: err.to_string(),
                    });
                    break;
                }
            }
        }
    }
    let mut found = Vec::new();
    for raw in &result.output.findings {
        if let Some(finding) = findings::validate(raw, diff_paths)
            && !input
                .suppress_fingerprints
                .contains(&crate::delta::fingerprint(&finding, &cluster.hunks))
        {
            found.push(finding);
        }
    }
    DiveOutcome {
        index: dive.index,
        findings: found,
        spend: spend_lines,
        degradations,
        stood_down: false,
        fatal: None,
    }
}

/// Resolve what a pass asked for, stopping at the run's size bound. A
/// refusal or a miss is answered rather than dropped.
fn resolve_requests(
    retriever: &crate::retrieval::Retriever,
    requested: &[String],
    budget: &std::sync::Mutex<usize>,
) -> Vec<crate::retrieval::Resolution> {
    use crate::retrieval::{Request, Resolution};
    let mut out = Vec::new();
    for raw in requested {
        let request = Request::parse(raw);
        let resolution = retriever.resolve(&request);
        if let Resolution::Found { content, .. } = &resolution {
            let mut remaining = budget.lock().expect("retrieval budget lock");
            if content.len() > *remaining {
                // Past the bound the request is answered as unavailable
                // rather than silently truncated into nonsense.
                out.push(Resolution::Refused {
                    label: request.label(),
                });
                continue;
            }
            *remaining -= content.len();
        }
        out.push(resolution);
    }
    out
}

/// Settle a completed pass against its hold. A pass served from cache made
/// no provider call this run, so its hold is released and it consumes no
/// budget, but its original cost is still reported because those dollars
/// were spent.
fn settle_pass(
    gate: &mut BudgetGate,
    hold: budget::Hold,
    usage: &TokenUsage,
    price: &ModelPrice,
    resumed: bool,
    reported_cost: Option<f64>,
) -> f64 {
    if resumed {
        gate.release(hold);
        price.cost_of_usage(usage)
    } else {
        gate.settle(hold, usage, price, reported_cost)
    }
}

/// The usage a failed pass burned and its error carries, so a run that
/// fails after billing still reports what it spent. Malformed covers
/// schema-invalid responses; OutputTruncated covers attempts that grew
/// past every ceiling and can never be recovered by a retry.
fn billed_usage(err: &ProviderError) -> Option<TokenUsage> {
    match err {
        ProviderError::Malformed { usage, .. } | ProviderError::OutputTruncated { usage, .. } => {
            Some(*usage)
        }
        _ => None,
    }
}

/// Estimate the input tokens a pass will actually send. The system rules
/// and the schema travel with every request (embedded by the transport),
/// so an estimate over the context alone underprices every pass.
fn estimate_prompt(prompt: &prompt::Prompt, schema: &serde_json::Value) -> u64 {
    estimate_tokens(&prompt.system)
        + estimate_tokens(&prompt.user)
        + estimate_tokens(&schema.to_string())
}

fn severity_word(severity: crate::config::Severity) -> &'static str {
    match severity {
        crate::config::Severity::Blocker => "blocker",
        crate::config::Severity::Warning => "warning",
        crate::config::Severity::Note => "note",
    }
}

fn skip_notice(
    input: &PipelineInput,
    gate: &BudgetGate,
    violations: &[findings::Finding],
    rules_skipped: bool,
) -> String {
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
    ) + &render_violations(violations)
        + if rules_skipped {
            RULES_NOT_APPLICABLE
        } else {
            ""
        }
}

/// Said once when rules are configured and the run has no pull request to
/// apply them to. A user who configured a rule and saw nothing could not
/// otherwise tell a satisfied rule from one that never ran.
pub(crate) const RULES_NOT_APPLICABLE: &str = "\n\nMetadata rules were not evaluated: \
this run reviews a local range and has no pull request title or description to judge.";

/// Rule violations rendered for a notice. They cost nothing to find, so a
/// skipped run still reports them rather than staying silent about the one
/// thing it did establish.
fn render_violations(violations: &[findings::Finding]) -> String {
    if violations.is_empty() {
        return String::new();
    }
    let mut out = String::from("\n\nRule violations found without a provider call:\n");
    for violation in violations {
        out.push_str(&format!(
            "- **[{}]** `{}`: {}\n",
            severity_word(violation.severity),
            violation.file,
            violation.message
        ));
    }
    out
}

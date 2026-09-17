//! Budget gate: pre-pass cost estimation, the per-pull-request cap, and
//! the fixed degradation ladder.

use crate::config::BudgetCap;
use crate::cost::ModelPrice;

/// A coverage reduction applied by the ladder or by the deep call ceiling.
/// Every variant renders into the published review.
#[derive(Debug, Clone, PartialEq)]
pub enum Degradation {
    /// The pass ran with only the highest-risk content.
    ContextShrunk {
        /// Pass that was shrunk.
        pass: String,
    },
    /// The pass ran on the triage model instead of its own role model.
    ModelDowngraded {
        /// Pass that was downgraded.
        pass: String,
    },
    /// The deep call ceiling left clusters without a deep dive.
    DeepCallsCapped {
        /// Paths of clusters that received no deep dive.
        unreviewed: Vec<String>,
    },
    /// Remaining passes were skipped and the review summarizes what ran.
    SummaryOnly {
        /// Passes that did not run.
        skipped: Vec<String>,
    },
    /// No pass could run. Nothing is published.
    Skipped {
        /// Why the run was skipped.
        reason: String,
    },
}

impl Degradation {
    /// The line rendered into the review body.
    pub fn describe(&self) -> String {
        match self {
            Degradation::ContextShrunk { pass } => {
                format!("{pass}: context was shrunk to the highest-risk content")
            }
            Degradation::ModelDowngraded { pass } => {
                format!("{pass}: ran on the triage model instead of the deep model")
            }
            Degradation::DeepCallsCapped { unreviewed } => {
                let names = unreviewed.join(", ");
                format!(
                    "deep call ceiling left {} cluster(s) without a deep dive: {names}",
                    unreviewed.len()
                )
            }
            Degradation::SummaryOnly { skipped } => {
                let names = skipped.join(", ");
                format!(
                    "budget exhausted before completion; the review summarizes only what ran. Passes skipped: {names}"
                )
            }
            Degradation::Skipped { reason } => {
                format!("review skipped: {reason}")
            }
        }
    }
}

/// Which kind of pass is asking to run. Controls which ladder rungs apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PassKind {
    /// The triage pass. No deep model to downgrade to.
    Triage,
    /// A deep dive or the cross-examination pass.
    Deep,
    /// The verdict synthesis pass.
    Verdict,
}

/// One authorization request: which pass, its full and shrunk context
/// estimates, and the prices it would pay.
pub struct PassEstimate<'a> {
    /// Which kind of pass is asking.
    pub kind: PassKind,
    /// Display name of the pass, used in degradation disclosures.
    pub pass: &'a str,
    /// Estimated input tokens for the full context.
    pub full_tokens: u64,
    /// Estimated input tokens for the shrunk context.
    pub shrunk_tokens: u64,
    /// Maximum output tokens the pass may produce.
    pub max_output_tokens: u32,
    /// Price of the role model.
    pub price: &'a ModelPrice,
    /// Price of the cheaper fallback model for deep passes.
    pub downgrade_price: Option<&'a ModelPrice>,
}

/// The ladder's answer to an authorization request.
#[derive(Debug, Clone, PartialEq)]
pub enum LadderDecision {
    /// Run the pass with its full selected context and role model.
    Run,
    /// Run the pass with a degradation applied.
    RunDegraded(Degradation),
    /// Stop running passes and synthesize from what completed.
    StandDown,
}

/// Tracks spend for one run against the pull request's cumulative cap and
/// applies the degradation ladder in the fixed order: shrink context,
/// downgrade model, summary-only, skip.
pub struct BudgetGate {
    cap: BudgetCap,
    prior_spend: f64,
    spent: f64,
    context_shrunk: bool,
    model_downgraded: bool,
    summary_only: bool,
}

impl BudgetGate {
    /// Build a gate for a run against the recorded prior spend.
    pub fn new(cap: BudgetCap, prior_spend: f64) -> BudgetGate {
        BudgetGate {
            cap,
            prior_spend,
            spent: 0.0,
            context_shrunk: false,
            model_downgraded: false,
            summary_only: false,
        }
    }

    /// Remaining budget in USD, infinite when explicitly unlimited.
    pub fn remaining(&self) -> f64 {
        match self.cap {
            BudgetCap::Unlimited => f64::INFINITY,
            BudgetCap::Limited(amount) => amount - self.prior_spend - self.spent,
        }
    }

    /// Spend recorded by this run so far.
    pub fn spent_this_run(&self) -> f64 {
        self.spent
    }

    /// Ask permission to run a pass.
    pub fn authorize(&mut self, estimate: PassEstimate<'_>) -> LadderDecision {
        if self.summary_only {
            return LadderDecision::StandDown;
        }
        let PassEstimate {
            kind: _kind,
            pass,
            full_tokens,
            shrunk_tokens,
            max_output_tokens,
            price,
            downgrade_price,
        } = estimate;
        let output_cost = max_output_tokens as f64 / 1_000_000.0 * price.output;
        let full_cost = price.cost_of_estimated_input(full_tokens) + output_cost;
        let shrunk_cost = price.cost_of_estimated_input(shrunk_tokens) + output_cost;
        let remaining = self.remaining();

        if remaining >= full_cost {
            return LadderDecision::Run;
        }
        // Rung 1: shrink the context to the highest-risk content. Every
        // pass that runs shrunk is disclosed, so repeats are recorded.
        if remaining >= shrunk_cost {
            self.context_shrunk = true;
            return LadderDecision::RunDegraded(Degradation::ContextShrunk {
                pass: pass.to_string(),
            });
        }
        // Rung 2: deep passes drop to the triage model.
        if let Some(cheap) = downgrade_price
            && !self.model_downgraded
        {
            let cheap_output = max_output_tokens as f64 / 1_000_000.0 * cheap.output;
            let cheap_full = cheap.cost_of_estimated_input(full_tokens) + cheap_output;
            let cheap_shrunk = cheap.cost_of_estimated_input(shrunk_tokens) + cheap_output;
            if remaining >= cheap_full {
                self.model_downgraded = true;
                return LadderDecision::RunDegraded(Degradation::ModelDowngraded {
                    pass: pass.to_string(),
                });
            }
            if remaining >= cheap_shrunk {
                self.model_downgraded = true;
                self.context_shrunk = true;
                return LadderDecision::RunDegraded(Degradation::ModelDowngraded {
                    pass: pass.to_string(),
                });
            }
        }
        // Rung 3: stand down. Rung 4 (skip) is what StandDown means for
        // a run that has nothing to synthesize.
        self.summary_only = true;
        LadderDecision::StandDown
    }

    /// Record actual usage after a pass ran.
    pub fn record(&mut self, usage: &crate::provider::TokenUsage, price: &ModelPrice) -> f64 {
        let cost = price.cost_of_usage(usage);
        self.spent += cost;
        cost
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_estimate<'a>(
        full: u64,
        shrunk: u64,
        price: &'a ModelPrice,
        downgrade: Option<&'a ModelPrice>,
    ) -> PassEstimate<'a> {
        PassEstimate {
            kind: PassKind::Deep,
            pass: "deep dive",
            full_tokens: full,
            shrunk_tokens: shrunk,
            max_output_tokens: 1000,
            price,
            downgrade_price: downgrade,
        }
    }

    fn price(input: f64, output: f64) -> ModelPrice {
        ModelPrice {
            input,
            cached_input: input / 2.0,
            output,
        }
    }

    #[test]
    fn unlimited_cap_never_stands_down() {
        let mut gate = BudgetGate::new(BudgetCap::Unlimited, 0.0);
        let decision = gate.authorize(test_estimate(
            u64::MAX / 2,
            u64::MAX / 2,
            &price(1000.0, 1000.0),
            None,
        ));
        assert_eq!(decision, LadderDecision::Run);
    }

    #[test]
    fn ladder_shrinks_before_downgrading() {
        // The full estimate does not fit the cap, the shrunk one does.
        let mut gate = BudgetGate::new(BudgetCap::Limited(0.05), 0.0);
        let deep = price(0.2, 10.0);
        let decision = gate.authorize(test_estimate(1_000_000, 100_000, &deep, None));
        assert_eq!(
            decision,
            LadderDecision::RunDegraded(Degradation::ContextShrunk {
                pass: "deep dive".to_string()
            })
        );
        // Each subsequent pass that runs shrunk is disclosed as well.
        let decision = gate.authorize(test_estimate(1_000_000, 100_000, &deep, None));
        assert_eq!(
            decision,
            LadderDecision::RunDegraded(Degradation::ContextShrunk {
                pass: "deep dive".to_string()
            })
        );
        // When even the shrunk estimate no longer fits, the run stands down.
        gate.record(
            &crate::provider::TokenUsage {
                input_tokens: 150_000,
                cached_input_tokens: 0,
                output_tokens: 1000,
            },
            &deep,
        );
        let decision = gate.authorize(test_estimate(1_000_000, 100_000, &deep, None));
        assert_eq!(decision, LadderDecision::StandDown);
    }

    #[test]
    fn ladder_downgrades_model_after_shrink_rung_is_taken() {
        // Neither price fits at full size; the shrunk estimate fits only at
        // the triage price, so the downgrade rung fires.
        let mut gate = BudgetGate::new(BudgetCap::Limited(0.05), 0.0);
        let deep = price(30.0, 15.0);
        let cheap = price(0.15, 0.6);
        gate.context_shrunk = true;
        let decision = gate.authorize(test_estimate(5_000_000, 100_000, &deep, Some(&cheap)));
        assert_eq!(
            decision,
            LadderDecision::RunDegraded(Degradation::ModelDowngraded {
                pass: "deep dive".to_string()
            })
        );
    }

    #[test]
    fn consumed_cap_skips_instead_of_spending_fresh() {
        let mut gate = BudgetGate::new(BudgetCap::Limited(5.0), 5.0);
        let deep = price(30.0, 15.0);
        let decision = gate.authorize(test_estimate(10_000, 5_000, &deep, None));
        assert_eq!(decision, LadderDecision::StandDown);
    }

    #[test]
    fn triage_standdown_means_nothing_can_run() {
        let mut gate = BudgetGate::new(BudgetCap::Limited(0.001), 0.0);
        let triage = price(0.15, 0.6);
        let mut estimate = test_estimate(4_000_000, 3_000_000, &triage, None);
        estimate.kind = PassKind::Triage;
        estimate.pass = "triage";
        estimate.max_output_tokens = 100;
        let decision = gate.authorize(estimate);
        assert_eq!(decision, LadderDecision::StandDown);
        assert!(gate.summary_only);
    }

    #[test]
    fn recorded_usage_counts_against_the_cap() {
        let mut gate = BudgetGate::new(BudgetCap::Limited(1.0), 0.0);
        let usage = crate::provider::TokenUsage {
            input_tokens: 100_000,
            cached_input_tokens: 0,
            output_tokens: 50_000,
        };
        let cost = gate.record(&usage, &price(2.0, 10.0));
        assert!((cost - 0.7).abs() < 1e-9);
        assert!((gate.remaining() - 0.3).abs() < 1e-9);
    }
}

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
    /// A pass failed against the provider and the run continued without it.
    PassFailed {
        /// Pass that failed.
        pass: String,
        /// Redacted reason the provider gave.
        reason: String,
    },
    /// The verdict summary prose could not be drafted. Findings and the
    /// verdict are unaffected because neither comes from that pass.
    SummaryUnavailable {
        /// Redacted reason the provider gave.
        reason: String,
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
            Degradation::PassFailed { pass, reason } => {
                format!("{pass} failed and was skipped: {reason}")
            }
            Degradation::SummaryUnavailable { reason } => {
                format!(
                    "the verdict summary could not be drafted ({reason}); findings and the verdict are unaffected"
                )
            }
            Degradation::Skipped { reason } => {
                format!("review skipped: {reason}")
            }
        }
    }
}

/// One authorization request: which pass, its full and shrunk context
/// estimates, and the prices it would pay.
pub struct PassEstimate<'a> {
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

/// The ladder's answer to an authorization request. A caller that runs a
/// pass must honor `shrink` and `downgrade`, because the gate priced the
/// pass on the assumption that it would.
#[derive(Debug, Clone, PartialEq)]
pub enum LadderDecision {
    /// Run the pass under the stated rungs.
    Run {
        /// Send the shrunk context instead of the full one.
        shrink: bool,
        /// Call the triage model instead of the role model.
        downgrade: bool,
        /// Disclosures for the rungs that applied.
        degradations: Vec<Degradation>,
        /// The estimate held for this pass. Settle it with the reported
        /// usage, or release it if the pass never reports.
        hold: Hold,
    },
    /// Stop running passes and synthesize from what completed.
    StandDown,
}

impl LadderDecision {
    /// Run with the full context on the role model, holding `estimate`.
    fn full(estimate: f64) -> LadderDecision {
        LadderDecision::Run {
            shrink: false,
            downgrade: false,
            degradations: Vec::new(),
            hold: Hold { estimate },
        }
    }

    /// The hold this decision took, if it authorized a pass.
    pub fn hold(&self) -> Option<Hold> {
        match self {
            LadderDecision::Run { hold, .. } => Some(*hold),
            LadderDecision::StandDown => None,
        }
    }
}

/// Tracks spend for one run against the pull request's cumulative cap and
/// applies the degradation ladder in the fixed order: shrink context,
/// downgrade model, summary-only, skip.
pub struct BudgetGate {
    cap: BudgetCap,
    prior_spend: f64,
    spent: f64,
    /// Estimates authorized for passes that have not reported usage yet.
    /// Without this the gate believes nothing is committed between
    /// authorizing a pass and recording it, which is true with one pass in
    /// flight and false with several: each would be told the whole
    /// remaining budget is free.
    held: f64,
    summary_only: bool,
}

/// An authorized estimate held against the budget until the pass it
/// belongs to reports what it actually used.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Hold {
    estimate: f64,
}

impl BudgetGate {
    /// Build a gate for a run against the recorded prior spend.
    pub fn new(cap: BudgetCap, prior_spend: f64) -> BudgetGate {
        BudgetGate {
            cap,
            prior_spend,
            spent: 0.0,
            held: 0.0,
            summary_only: false,
        }
    }

    /// Remaining budget in USD, infinite when explicitly unlimited.
    pub fn remaining(&self) -> f64 {
        match self.cap {
            BudgetCap::Unlimited => f64::INFINITY,
            BudgetCap::Limited(amount) => amount - self.prior_spend - self.spent - self.held,
        }
    }

    /// Spend recorded by this run so far.
    pub fn spent_this_run(&self) -> f64 {
        self.spent
    }

    /// Ask permission to run a pass. A decision to run holds its estimate
    /// against the remaining budget until the pass settles or releases, so
    /// passes authorized while others are in flight cannot together exceed
    /// the cap.
    pub fn authorize(&mut self, estimate: PassEstimate<'_>) -> LadderDecision {
        if self.summary_only {
            return LadderDecision::StandDown;
        }
        let PassEstimate {
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
            self.held += full_cost;
            return LadderDecision::full(full_cost);
        }
        // Rung 1: shrink the context to the highest-risk content. Every
        // pass that runs shrunk is disclosed, so repeats are recorded.
        if remaining >= shrunk_cost {
            self.held += shrunk_cost;
            return LadderDecision::Run {
                shrink: true,
                downgrade: false,
                degradations: vec![Degradation::ContextShrunk {
                    pass: pass.to_string(),
                }],
                hold: Hold {
                    estimate: shrunk_cost,
                },
            };
        }
        // Rung 2: deep passes drop to the triage model. The rung stays
        // available to every later pass; collapsing straight to stand-down
        // once one pass has downgraded would cut coverage the budget can
        // still afford.
        if let Some(cheap) = downgrade_price {
            let cheap_output = max_output_tokens as f64 / 1_000_000.0 * cheap.output;
            let cheap_full = cheap.cost_of_estimated_input(full_tokens) + cheap_output;
            let cheap_shrunk = cheap.cost_of_estimated_input(shrunk_tokens) + cheap_output;
            if remaining >= cheap_full {
                self.held += cheap_full;
                return LadderDecision::Run {
                    shrink: false,
                    downgrade: true,
                    degradations: vec![Degradation::ModelDowngraded {
                        pass: pass.to_string(),
                    }],
                    hold: Hold {
                        estimate: cheap_full,
                    },
                };
            }
            if remaining >= cheap_shrunk {
                self.held += cheap_shrunk;
                return LadderDecision::Run {
                    shrink: true,
                    downgrade: true,
                    degradations: vec![
                        Degradation::ContextShrunk {
                            pass: pass.to_string(),
                        },
                        Degradation::ModelDowngraded {
                            pass: pass.to_string(),
                        },
                    ],
                    hold: Hold {
                        estimate: cheap_shrunk,
                    },
                };
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

    /// Release a hold and book what the provider actually reported. A hold
    /// is never recorded as spend: it is released and replaced by the
    /// measured cost, so the recorded figure stays exact.
    pub fn settle(
        &mut self,
        hold: Hold,
        usage: &crate::provider::TokenUsage,
        price: &ModelPrice,
    ) -> f64 {
        self.release(hold);
        self.record(usage, price)
    }

    /// Release a hold for a pass that never reported usage. A failure must
    /// not permanently consume budget that was never spent.
    pub fn release(&mut self, hold: Hold) {
        self.held = (self.held - hold.estimate).max(0.0);
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
        assert!(
            matches!(
                decision,
                LadderDecision::Run {
                    shrink: false,
                    downgrade: false,
                    ..
                }
            ),
            "{decision:?}"
        );
    }

    #[test]
    fn ladder_shrinks_before_downgrading() {
        // The full estimate does not fit the cap, the shrunk one does.
        let mut gate = BudgetGate::new(BudgetCap::Limited(0.05), 0.0);
        let deep = price(0.2, 10.0);
        let decision = gate.authorize(test_estimate(1_000_000, 100_000, &deep, None));
        assert_shrunk(&decision);
        // The hold would keep a second pass from being authorized against
        // the same budget, so release it to test the rung in isolation.
        gate.release(decision.hold().unwrap());
        let decision = gate.authorize(test_estimate(1_000_000, 100_000, &deep, None));
        assert_shrunk(&decision);
        gate.release(decision.hold().unwrap());
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
        let decision = gate.authorize(test_estimate(5_000_000, 100_000, &deep, Some(&cheap)));
        let LadderDecision::Run {
            shrink,
            downgrade,
            degradations,
            ..
        } = &decision
        else {
            panic!("expected a run, got {decision:?}");
        };
        assert!(*shrink && *downgrade, "{decision:?}");
        assert_eq!(degradations.len(), 2);
    }

    fn assert_shrunk(decision: &LadderDecision) {
        let LadderDecision::Run {
            shrink,
            downgrade,
            degradations,
            ..
        } = decision
        else {
            panic!("expected a run, got {decision:?}");
        };
        assert!(*shrink && !*downgrade, "{decision:?}");
        assert_eq!(
            degradations,
            &vec![Degradation::ContextShrunk {
                pass: "deep dive".to_string()
            }]
        );
    }

    #[test]
    fn downgrade_rung_stays_available_to_later_passes() {
        // One pass downgrading must not collapse every later pass straight
        // to stand-down while the cheap model is still affordable.
        let mut gate = BudgetGate::new(BudgetCap::Limited(1.0), 0.0);
        let deep = price(300.0, 150.0);
        let cheap = price(0.15, 0.6);
        for _ in 0..3 {
            let decision = gate.authorize(test_estimate(1_000_000, 100_000, &deep, Some(&cheap)));
            assert!(
                matches!(
                    decision,
                    LadderDecision::Run {
                        downgrade: true,
                        ..
                    }
                ),
                "{decision:?}"
            );
        }
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
        estimate.pass = "triage";
        estimate.max_output_tokens = 100;
        let decision = gate.authorize(estimate);
        assert_eq!(decision, LadderDecision::StandDown);
        assert!(gate.summary_only);
    }

    #[test]
    fn authorizations_in_flight_cannot_together_exceed_the_cap() {
        // Without a hold, each pass would be told the whole remaining
        // budget is free and they would together spend several times it.
        let mut gate = BudgetGate::new(BudgetCap::Limited(0.10), 0.0);
        let deep = price(1.0, 10.0);
        let mut authorized = 0;
        for _ in 0..10 {
            match gate.authorize(test_estimate(10_000, 10_000, &deep, None)) {
                LadderDecision::Run { .. } => authorized += 1,
                LadderDecision::StandDown => break,
            }
        }
        // Each pass holds 1000 output tokens at $10/M plus input, so the
        // cap allows only a handful, not all ten.
        assert!(authorized > 0, "at least one pass must be affordable");
        assert!(
            authorized < 10,
            "the cap must stop authorizing, got {authorized}"
        );
        assert!(gate.remaining() >= 0.0, "never authorized past the cap");
    }

    #[test]
    fn settling_books_actual_usage_not_the_estimate() {
        let mut gate = BudgetGate::new(BudgetCap::Limited(1.0), 0.0);
        let deep = price(2.0, 10.0);
        let LadderDecision::Run { hold, .. } =
            gate.authorize(test_estimate(10_000, 10_000, &deep, None))
        else {
            panic!("expected a run");
        };
        // The estimate assumed a full 1000 output tokens; the pass used 10.
        let usage = crate::provider::TokenUsage {
            input_tokens: 1_000,
            cached_input_tokens: 0,
            output_tokens: 10,
        };
        let cost = gate.settle(hold, &usage, &deep);
        let expected = deep.cost_of_usage(&usage);
        assert!((cost - expected).abs() < 1e-12, "{cost} != {expected}");
        assert!(
            (gate.remaining() - (1.0 - expected)).abs() < 1e-12,
            "the hold must be released, leaving only actual spend: {}",
            gate.remaining()
        );
    }

    #[test]
    fn a_released_hold_restores_the_budget() {
        // A pass that fails without reporting must not permanently consume
        // budget it never spent.
        let mut gate = BudgetGate::new(BudgetCap::Limited(1.0), 0.0);
        let deep = price(2.0, 10.0);
        let before = gate.remaining();
        let LadderDecision::Run { hold, .. } =
            gate.authorize(test_estimate(10_000, 10_000, &deep, None))
        else {
            panic!("expected a run");
        };
        assert!(gate.remaining() < before, "authorizing holds the estimate");
        gate.release(hold);
        assert!(
            (gate.remaining() - before).abs() < 1e-12,
            "releasing restores it: {} != {before}",
            gate.remaining()
        );
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

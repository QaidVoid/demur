//! Output rendering: machine-readable JSON mirroring the review, and the
//! terminal markdown pass-through.

use demur_core::config::Severity;
use demur_core::pipeline::synthesis::Verdict;
use serde::Serialize;

/// One finding in JSON output.
#[derive(Debug, Serialize)]
pub struct JsonFinding {
    /// Severity of the finding.
    pub severity: Severity,
    /// File the finding cites.
    pub file: String,
    /// First cited line.
    pub start_line: u32,
    /// Last cited line.
    pub end_line: u32,
    /// Short statement of the defect.
    pub message: String,
    /// The concrete harm merging would cause.
    pub harm: String,
    /// A concrete fix, when available.
    pub suggestion: Option<String>,
}

/// Spend breakdown in JSON output.
#[derive(Debug, Serialize)]
pub struct JsonSpend {
    /// Per-pass spend in USD.
    pub passes: Vec<JsonPassSpend>,
    /// This run's total in USD.
    pub total: f64,
    /// Spend recorded by earlier runs on this pull request.
    pub prior_spend: f64,
}

/// One pass's spend in JSON output.
#[derive(Debug, Serialize)]
pub struct JsonPassSpend {
    /// Pass name.
    pub pass: String,
    /// Cost in USD.
    pub cost: f64,
}

/// The machine-readable review document.
#[derive(Debug, Serialize)]
pub struct JsonReview {
    /// The verdict: approve or request_changes.
    pub verdict: JsonVerdict,
    /// Ranked findings, exactly as published.
    pub findings: Vec<JsonFinding>,
    /// Findings omitted beyond the comment budget.
    pub omitted: usize,
    /// Degradations that reduced coverage.
    pub degradations: Vec<String>,
    /// Spend for the run and the cumulative pull request spend.
    pub spend: JsonSpend,
}

/// The verdict in JSON output.
#[derive(Debug, Serialize)]
pub enum JsonVerdict {
    /// No blocking finding survived.
    #[serde(rename = "approve")]
    Approve,
    /// A blocking finding stands.
    #[serde(rename = "request_changes")]
    RequestChanges,
}

impl From<Verdict> for JsonVerdict {
    fn from(verdict: Verdict) -> Self {
        match verdict {
            Verdict::Approve => JsonVerdict::Approve,
            Verdict::RequestChanges => JsonVerdict::RequestChanges,
        }
    }
}

/// Render a review into the JSON document.
pub fn json_review(review: &demur_core::pipeline::Review) -> JsonReview {
    JsonReview {
        verdict: review.verdict.into(),
        findings: review
            .published
            .iter()
            .map(|finding| JsonFinding {
                severity: finding.severity,
                file: finding.file.clone(),
                start_line: finding.start_line,
                end_line: finding.end_line,
                message: finding.message.clone(),
                harm: finding.harm.clone(),
                suggestion: finding.suggestion.clone(),
            })
            .collect(),
        omitted: review.omitted,
        degradations: review
            .degradations
            .iter()
            .map(|degradation| degradation.describe())
            .collect(),
        spend: JsonSpend {
            passes: review
                .spend
                .passes
                .iter()
                .map(|pass| JsonPassSpend {
                    pass: pass.pass.clone(),
                    cost: pass.cost,
                })
                .collect(),
            total: review.spend.total,
            prior_spend: review.spend.prior_spend,
        },
    }
}

//! Verdict synthesis: the single place a verdict is computed, with forced
//! ranking, the comment budget, and the review body rendering.

use crate::config::Severity;
use crate::pipeline::budget::Degradation;
use crate::pipeline::findings::{Finding, dedupe, rank};

/// The review verdict. Publication reports it and never recomputes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// No finding at a blocking severity survived.
    Approve,
    /// At least one finding sits at or above the lowest blocking severity.
    RequestChanges,
}

impl Verdict {
    /// The GitHub review event name for the verdict.
    pub fn event(&self) -> &'static str {
        match self {
            Verdict::Approve => "APPROVE",
            Verdict::RequestChanges => "REQUEST_CHANGES",
        }
    }

    /// The check run conclusion for the verdict.
    pub fn check_conclusion(&self) -> &'static str {
        match self {
            Verdict::Approve => "success",
            Verdict::RequestChanges => "failure",
        }
    }
}

/// Everything synthesis consumes.
pub struct SynthesisInput {
    /// All validated findings entering synthesis, including findings
    /// carried forward from earlier runs.
    pub findings: Vec<Finding>,
    /// Severities that block a merge.
    pub block_on: Vec<Severity>,
    /// Maximum published findings.
    pub comment_budget: u32,
    /// Degradations applied during the run, always disclosed.
    pub degradations: Vec<Degradation>,
    /// Spend per pass for this run, as (pass, cost, resumed) triples.
    pub spend_lines: Vec<(String, f64, bool)>,
    /// Spend recorded by earlier runs on this pull request.
    pub prior_spend: f64,
    /// Summary paragraph drafted by the verdict model, if available.
    pub summary: Option<String>,
    /// True when rules were configured but the run had no pull request to
    /// apply them to. Stated once, and never a finding.
    pub rules_skipped: bool,
}

/// The synthesized review.
pub struct Synthesis {
    /// The verdict computed from every finding entering synthesis.
    pub verdict: Verdict,
    /// Findings published within the comment budget, ranked.
    pub published: Vec<Finding>,
    /// How many findings were omitted beyond the budget.
    pub omitted: usize,
    /// Verdict-setting findings cut by the budget, named in the body.
    pub beyond_budget: Vec<Finding>,
    /// The rendered markdown body.
    pub body: String,
}

/// Compute the verdict, enforce the comment budget, and render the body.
pub fn synthesize(input: SynthesisInput) -> Synthesis {
    let SynthesisInput {
        findings,
        block_on,
        comment_budget,
        degradations,
        spend_lines,
        prior_spend,
        summary,
        rules_skipped,
    } = input;
    let mut findings = dedupe(findings);
    rank(&mut findings);

    let blocking_rank = block_on
        .iter()
        .map(|severity| severity.rank())
        .min()
        .unwrap_or(Severity::Blocker.rank());
    let verdict = if findings
        .iter()
        .any(|finding| finding.severity.rank() >= blocking_rank)
    {
        Verdict::RequestChanges
    } else {
        Verdict::Approve
    };

    let budget = comment_budget as usize;
    let published: Vec<Finding> = findings.iter().take(budget).cloned().collect();
    let omitted = findings.len().saturating_sub(published.len());
    let beyond_budget: Vec<Finding> = findings
        .iter()
        .skip(published.len())
        .filter(|finding| finding.severity.rank() >= blocking_rank)
        .cloned()
        .collect();

    let body = render_body(
        verdict,
        &published,
        omitted,
        &beyond_budget,
        &comment_budget,
        &degradations,
        &spend_lines,
        prior_spend,
        &summary,
        rules_skipped,
    );
    Synthesis {
        verdict,
        published,
        omitted,
        beyond_budget,
        body,
    }
}

fn money(amount: f64) -> String {
    format!("${:.4}", amount)
}

#[allow(clippy::too_many_arguments)]
fn render_body(
    verdict: Verdict,
    published: &[Finding],
    omitted: usize,
    beyond_budget: &[Finding],
    comment_budget: &u32,
    degradations: &[Degradation],
    spend_lines: &[(String, f64, bool)],
    prior_spend: f64,
    summary: &Option<String>,
    rules_skipped: bool,
) -> String {
    let mut body = String::new();
    match verdict {
        Verdict::RequestChanges => {
            body.push_str("## demur: changes requested\n\n");
            body.push_str(
                "Granting every fact in this pull request, there is still no case for merging it.\n\n",
            );
        }
        Verdict::Approve => {
            body.push_str("## demur: no case against merging\n\n");
            body.push_str(
                "Granting every fact in this pull request, no blocking defect was established.\n\n",
            );
        }
    }
    if let Some(summary) = summary {
        body.push_str(summary.trim());
        body.push_str("\n\n");
    }
    if !published.is_empty() {
        body.push_str("### Findings (ranked)\n\n");
        for (index, finding) in published.iter().enumerate() {
            body.push_str(&render_finding(index + 1, finding));
        }
    }
    if omitted > 0 {
        body.push_str(&format!(
            "### Omitted findings\n\n{omitted} finding(s) were omitted beyond the comment budget of {comment_budget}.\n\n"
        ));
    }
    if !beyond_budget.is_empty() {
        body.push_str("### Blocking findings outside the comment budget\n\n");
        for finding in beyond_budget {
            body.push_str(&format!(
                "- **[{}]** `{}`: {}\n",
                severity_name(finding.severity),
                finding.location(),
                finding.message
            ));
        }
        body.push('\n');
    }
    body.push_str("### Coverage and spend\n\n");
    if degradations.is_empty() {
        body.push_str("- Full coverage: every pass ran without degradation.\n");
    } else {
        for degradation in degradations {
            body.push_str(&format!("- {}\n", degradation.describe()));
        }
    }
    let paid: f64 = spend_lines
        .iter()
        .filter(|(_, _, resumed)| !resumed)
        .map(|(_, cost, _)| cost)
        .sum();
    let inherited: f64 = spend_lines
        .iter()
        .filter(|(_, _, resumed)| *resumed)
        .map(|(_, cost, _)| cost)
        .sum();
    let run_total = paid + inherited;
    for (pass, cost, resumed) in spend_lines {
        let note = if *resumed {
            " (resumed from cache)"
        } else {
            ""
        };
        body.push_str(&format!("- {pass} spend: {}{note}\n", money(*cost)));
    }
    if inherited > 0.0 {
        // A resumed run is not a cheap review. It is a review that had to
        // be paid for across more than one attempt.
        let resumed: Vec<&str> = spend_lines
            .iter()
            .filter(|(_, _, resumed)| *resumed)
            .map(|(pass, _, _)| pass.as_str())
            .collect();
        body.push_str(&format!(
            "- Paid by this run: {}\n- Inherited from an earlier attempt: {} ({})\n",
            money(paid),
            money(inherited),
            resumed.join(", ")
        ));
    }
    body.push_str(&format!("- Total spend this run: {}\n", money(run_total)));
    body.push_str(&format!(
        "- Cumulative spend for this pull request: {} (earlier runs: {})\n",
        money(run_total + prior_spend),
        money(prior_spend)
    ));
    if rules_skipped {
        body.push_str(
            "- Metadata rules were not evaluated: this run reviews a local range and \
has no pull request title or description to judge.\n",
        );
    }
    body
}

fn render_finding(index: usize, finding: &Finding) -> String {
    let mut out = format!(
        "{}. **[{}]** `{}`: {}\n",
        index,
        severity_name(finding.severity),
        finding.location(),
        finding.message
    );
    out.push_str(&format!("   {}\n", finding.harm));
    if let Some(suggestion) = &finding.suggestion {
        out.push_str("   ```suggestion\n");
        for line in suggestion.lines() {
            out.push_str("   ");
            out.push_str(line);
            out.push('\n');
        }
        out.push_str("   ```\n");
    }
    out
}

fn severity_name(severity: Severity) -> &'static str {
    match severity {
        Severity::Blocker => "blocker",
        Severity::Warning => "warning",
        Severity::Note => "note",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finding(severity: Severity, file: &str, message: &str) -> Finding {
        Finding {
            file: file.to_string(),
            start_line: 3,
            end_line: 5,
            severity,
            message: message.to_string(),
            harm: format!("Merging {} causes concrete harm to users.", message),
            suggestion: None,
        }
    }

    fn input(findings: Vec<Finding>, block_on: Vec<Severity>, budget: u32) -> SynthesisInput {
        SynthesisInput {
            findings,
            block_on,
            comment_budget: budget,
            degradations: Vec::new(),
            spend_lines: vec![("triage".to_string(), 0.0021, false)],
            prior_spend: 0.01,
            summary: None,
            rules_skipped: false,
        }
    }

    #[test]
    fn blocker_under_default_configuration_requests_changes() {
        let result = synthesize(input(
            vec![
                finding(Severity::Note, "a.rs", "note one"),
                finding(Severity::Blocker, "b.rs", "blocked"),
            ],
            vec![Severity::Blocker],
            10,
        ));
        assert_eq!(result.verdict, Verdict::RequestChanges);
        assert_eq!(result.published.len(), 2);
        assert_eq!(result.published[0].severity, Severity::Blocker);
    }

    #[test]
    fn notes_and_warnings_only_approve_under_default() {
        let result = synthesize(input(
            vec![
                finding(Severity::Warning, "a.rs", "warned"),
                finding(Severity::Note, "b.rs", "noted"),
            ],
            vec![Severity::Blocker],
            10,
        ));
        assert_eq!(result.verdict, Verdict::Approve);
    }

    #[test]
    fn warning_block_on_yields_request_changes() {
        let result = synthesize(input(
            vec![finding(Severity::Warning, "a.rs", "warned")],
            vec![Severity::Warning, Severity::Blocker],
            10,
        ));
        assert_eq!(result.verdict, Verdict::RequestChanges);
    }

    #[test]
    fn carried_blocker_with_clean_delta_requests_changes() {
        let carried = finding(Severity::Blocker, "old.rs", "carried blocker");
        let result = synthesize(input(vec![carried], vec![Severity::Blocker], 10));
        assert_eq!(result.verdict, Verdict::RequestChanges);
    }

    #[test]
    fn forty_findings_publish_only_the_budget_with_omission_count() {
        let findings: Vec<Finding> = (0..40)
            .map(|i| {
                finding(
                    Severity::Note,
                    "a.rs",
                    &format!("distinct finding number {i}"),
                )
            })
            .collect();
        let result = synthesize(input(findings, vec![Severity::Blocker], 10));
        assert_eq!(result.published.len(), 10);
        assert_eq!(result.omitted, 30);
        assert!(result.body.contains("30 finding(s) were omitted"));
    }

    #[test]
    fn cut_blocking_finding_still_sets_verdict_and_is_named() {
        let findings: Vec<Finding> = (0..15)
            .map(|i| {
                finding(
                    Severity::Blocker,
                    "z.rs",
                    &format!("blocking defect number {i} with a long harm story"),
                )
            })
            .chain((0..30).map(|i| {
                finding(
                    Severity::Note,
                    "a.rs",
                    &format!("distinct finding number {i}"),
                )
            }))
            .collect();
        let result = synthesize(input(findings, vec![Severity::Blocker], 10));
        assert_eq!(result.verdict, Verdict::RequestChanges);
        assert_eq!(result.published.len(), 10);
        assert_eq!(result.omitted, 35);
        assert!(
            result
                .body
                .contains("Blocking findings outside the comment budget")
        );
        assert!(result.body.contains("blocking defect number 14"));
    }

    #[test]
    fn carried_findings_are_not_duplicated_in_publication() {
        let same = finding(Severity::Blocker, "old.rs", "carried blocker");
        let result = synthesize(input(vec![same.clone(), same], vec![Severity::Blocker], 10));
        assert_eq!(result.published.len(), 1);
    }

    #[test]
    fn body_reports_spend_and_degradations() {
        let result = synthesize(SynthesisInput {
            findings: vec![],
            block_on: vec![Severity::Blocker],
            comment_budget: 10,
            degradations: vec![Degradation::DeepCallsCapped {
                unreviewed: vec!["big.rs".to_string()],
            }],
            spend_lines: vec![
                ("triage".to_string(), 0.0021, false),
                ("deep".to_string(), 0.1130, false),
            ],
            prior_spend: 0.25,
            summary: None,
            rules_skipped: false,
        });
        assert!(result.body.contains("deep call ceiling left 1 cluster(s)"));
        assert!(result.body.contains("triage spend: $0.0021"));
        assert!(result.body.contains("Total spend this run: $0.1151"));
        assert!(
            result
                .body
                .contains("Cumulative spend for this pull request: $0.3651")
        );
    }
}

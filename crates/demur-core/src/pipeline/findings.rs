//! The finding contract: model output types, validation, normalization,
//! deduplication, and ranking.

use crate::config::Severity;
use serde::Deserialize;

/// A finding as reported by a model pass, before validation.
#[derive(Debug, Clone, Deserialize)]
pub struct ModelFinding {
    /// File path the finding cites.
    pub file: String,
    /// First cited line, one-based.
    pub start_line: u32,
    /// Last cited line, inclusive.
    pub end_line: u32,
    /// Severity of the finding.
    pub severity: Severity,
    /// Short statement of the defect.
    pub message: String,
    /// The concrete harm merging would cause.
    pub harm: String,
    /// A concrete fix, when one can be expressed.
    pub suggestion: Option<String>,
}

/// The triage pass output: findings plus per-cluster lens suggestions.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct TriageOutput {
    /// Findings the triage model raised directly.
    pub findings: Vec<ModelFinding>,
    /// Suggested review lenses per cluster path.
    pub cluster_lens: Vec<ClusterLens>,
}

/// Lens suggestions for one cluster path.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ClusterLens {
    /// Cluster path.
    pub path: String,
    /// Suggested lens names.
    pub lenses: Vec<String>,
}

/// Findings-only output, used by deep dives and cross-examination.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ModelFindings {
    /// Findings raised by the pass.
    pub findings: Vec<ModelFinding>,
    /// Repository context the pass says it needs before it can argue.
    /// Names only: the bot resolves them, nothing is executed.
    #[serde(default)]
    pub context_requests: Vec<String>,
}

/// One concern about a location: a statement and the harm merging causes.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Concern {
    /// Short statement of the defect.
    pub message: String,
    /// The concrete harm merging would cause.
    pub harm: String,
    /// A concrete fix, when one can be expressed. Rendered as a plain code
    /// fence, since a comment carries at most one suggestion block.
    pub suggestion: Option<String>,
}

/// A validated finding anchored to the pull request diff.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Finding {
    /// File path the finding cites, matching a path in the diff.
    pub file: String,
    /// First cited line, one-based.
    pub start_line: u32,
    /// Last cited line, inclusive.
    pub end_line: u32,
    /// Severity of the finding.
    pub severity: Severity,
    /// Short statement of the defect.
    pub message: String,
    /// The concrete harm merging would cause.
    pub harm: String,
    /// A concrete fix, when one can be expressed.
    pub suggestion: Option<String>,
    /// Further concerns raised about this location, filled by
    /// reconciliation. The leading concern is `message` and `harm`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub further_concerns: Vec<Concern>,
}

impl Finding {
    /// Location rendered as file:line-line for reports.
    pub fn location(&self) -> String {
        // A metadata finding cites a pull request field, which has no
        // line. Rendering `pull request title:0` would invent one.
        if self.start_line == 0 && self.end_line == 0 {
            return self.file.clone();
        }
        if self.start_line == self.end_line {
            format!("{}:{}", self.file, self.start_line)
        } else {
            format!("{}:{}-{}", self.file, self.start_line, self.end_line)
        }
    }
}

const PRAISE_MARKERS: &[&str] = &[
    "great",
    "nice",
    "excellent",
    "well done",
    "good job",
    "love",
    "perfect",
    "elegant",
    "impressive",
    "beautiful",
    "clean code",
    "thank",
];

const HARM_MARKERS: &[&str] = &[
    "break",
    "leak",
    "expose",
    "fail",
    "crash",
    "corrupt",
    "bypass",
    "loss",
    "risk",
    "vulnerab",
    "inject",
    "deadlock",
    "race",
    "overflow",
    "regress",
    "delete",
    "drop",
    "overcharge",
    "undercharge",
    "deny",
    "lock out",
    "corruption",
    "hang",
    "timeout",
    "misuse",
    "escalat",
    "spoof",
    "tamper",
];

/// Validate a model finding against the diff paths. Returns None for
/// findings with no usable location or no concrete harm, and for praise or
/// pure style remarks.
pub fn validate(raw: &ModelFinding, diff_paths: &[String]) -> Option<Finding> {
    if !diff_paths.iter().any(|path| path == &raw.file) {
        return None;
    }
    if raw.start_line == 0 || raw.end_line < raw.start_line {
        return None;
    }
    if raw.end_line - raw.start_line > 500 {
        return None;
    }
    let harm = raw.harm.trim();
    if harm.len() < 10 {
        return None;
    }
    if raw.message.trim().is_empty() {
        return None;
    }
    let combined = format!("{} {}", raw.message.to_lowercase(), harm.to_lowercase());
    let praises = PRAISE_MARKERS
        .iter()
        .any(|marker| combined.contains(marker));
    let harms = HARM_MARKERS.iter().any(|marker| combined.contains(marker));
    if praises && !harms {
        return None;
    }
    Some(Finding {
        file: raw.file.clone(),
        start_line: raw.start_line,
        end_line: raw.end_line,
        severity: raw.severity,
        message: raw.message.trim().to_string(),
        harm: harm.to_string(),
        suggestion: raw
            .suggestion
            .as_ref()
            .map(|text| text.trim().to_string())
            .filter(|text| !text.is_empty()),
        further_concerns: Vec::new(),
    })
}

/// Normalize a message for deduplication: lowercase, strip punctuation,
/// collapse whitespace. Same area with a different message stays distinct.
pub fn normalize_message(text: &str) -> String {
    let mut out = String::new();
    let mut last_space = true;
    for ch in text.chars() {
        if ch.is_alphanumeric() {
            out.extend(ch.to_lowercase());
            last_space = false;
        } else if !last_space {
            out.push(' ');
            last_space = true;
        }
    }
    out.trim().to_string()
}

/// Dedupe findings by path and normalized message, keeping the first
/// occurrence. The caller ranks first, so the survivor is the
/// highest-ranked of the duplicates.
pub fn dedupe(findings: Vec<Finding>) -> Vec<Finding> {
    let mut seen = std::collections::HashSet::new();
    findings
        .into_iter()
        .filter(|finding| seen.insert((finding.file.clone(), normalize_message(&finding.message))))
        .collect()
}

/// Rank findings: most severe first, then findings with a concrete fix,
/// then longer harm arguments. Deterministic.
pub fn rank(findings: &mut [Finding]) {
    findings.sort_by(|a, b| {
        b.severity
            .rank()
            .cmp(&a.severity.rank())
            .then_with(|| b.suggestion.is_some().cmp(&a.suggestion.is_some()))
            .then_with(|| b.harm.len().cmp(&a.harm.len()))
            .then_with(|| a.location().cmp(&b.location()))
    });
}

/// Prepare a finding set for any consumer that argues about it: rank,
/// deduplicate, and reconcile. Idempotent, so a prompt built from a
/// prepared set and a synthesis run over the same set agree.
pub fn prepare(mut findings: Vec<Finding>) -> Vec<Finding> {
    rank(&mut findings);
    reconcile(dedupe(findings))
}

/// Reconcile findings that cite the same location into one finding
/// carrying every concern raised about it, and fold near-duplicates:
/// findings on one file whose ranges overlap and whose opening statements
/// argue the same defect in different words are one defect reported by two
/// passes. Merges and never discards: the leading concern is the first,
/// which is the highest-ranked, and the severity is the most severe
/// present. Deterministic.
pub fn reconcile(findings: Vec<Finding>) -> Vec<Finding> {
    let mut groups: Vec<Finding> = Vec::new();
    let mut index: std::collections::HashMap<(String, u32), usize> =
        std::collections::HashMap::new();
    for finding in findings {
        let key = (finding.file.clone(), finding.start_line);
        match index.get(&key) {
            Some(&position) => {
                let lead = &mut groups[position];
                if finding.severity.rank() > lead.severity.rank() {
                    lead.severity = finding.severity;
                }
                lead.further_concerns.push(Concern {
                    message: finding.message,
                    harm: finding.harm,
                    suggestion: finding.suggestion,
                });
            }
            None => {
                index.insert(key, groups.len());
                groups.push(finding);
            }
        }
    }
    let mut folded: Vec<Finding> = Vec::new();
    for finding in groups {
        match folded.iter_mut().find(|lead| {
            lead.file == finding.file
                && ranges_overlap(lead, &finding)
                && messages_align(&lead.message, &finding.message)
        }) {
            Some(lead) => {
                if finding.severity.rank() > lead.severity.rank() {
                    lead.severity = finding.severity;
                }
                lead.further_concerns.push(Concern {
                    message: finding.message,
                    harm: finding.harm,
                    suggestion: finding.suggestion,
                });
                lead.further_concerns.extend(finding.further_concerns);
            }
            None => folded.push(finding),
        }
    }
    folded
}

/// True when two cited ranges share at least one line.
fn ranges_overlap(a: &Finding, b: &Finding) -> bool {
    a.start_line <= b.end_line && b.start_line <= a.end_line
}

/// The opening statement of a message, where a finding states its defect.
/// Two passes reporting one defect diverge later into different worked
/// examples; their openings say the same thing.
fn opening_statement(text: &str) -> &str {
    match text.find(['.', '!', '?']) {
        Some(end) => &text[..=end],
        None => text,
    }
}

/// Content words of a text: lowercased, punctuation split, stopwords and
/// single letters dropped.
fn content_tokens(text: &str) -> std::collections::HashSet<String> {
    const STOPWORDS: &[&str] = &[
        "the", "a", "an", "is", "are", "was", "be", "been", "to", "of", "and", "or", "in", "on",
        "for", "with", "as", "by", "at", "from", "it", "its", "that", "this", "those", "these",
        "not", "no", "so", "when", "then", "than", "which", "would", "could", "should", "into",
        "onto", "all", "every", "each", "any", "also", "but",
    ];
    text.split(|ch: char| !ch.is_alphanumeric())
        .map(|word| word.to_lowercase())
        .filter(|word| word.len() > 1 && !STOPWORDS.contains(&word.as_str()))
        .collect()
}

/// True when the shorter opening's content words mostly appear in the
/// longer one: the same defect claim stated twice. Measured on a real
/// duplicate pair at 0.8 and on distinct findings at 0.0, so the
/// threshold sits far from both.
fn messages_align(a: &str, b: &str) -> bool {
    const THRESHOLD: f64 = 0.6;
    let (left, right) = (
        content_tokens(opening_statement(a)),
        content_tokens(opening_statement(b)),
    );
    let (small, large) = if left.len() <= right.len() {
        (&left, &right)
    } else {
        (&right, &left)
    };
    if small.is_empty() {
        return false;
    }
    let shared = small.iter().filter(|word| large.contains(*word)).count();
    shared as f64 / small.len() as f64 >= THRESHOLD
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths() -> Vec<String> {
        vec!["src/main.rs".to_string()]
    }

    fn raw() -> ModelFinding {
        ModelFinding {
            file: "src/main.rs".to_string(),
            start_line: 3,
            end_line: 5,
            severity: Severity::Blocker,
            message: "hardcoded credential".to_string(),
            harm: "Merging publishes a live credential in the repository history.".to_string(),
            suggestion: None,
        }
    }

    #[test]
    fn valid_finding_passes_validation() {
        let finding = validate(&raw(), &paths()).unwrap();
        assert_eq!(finding.location(), "src/main.rs:3-5");
    }

    #[test]
    fn unanchored_findings_are_dropped() {
        let mut unknown_file = raw();
        unknown_file.file = "src/unknown.rs".to_string();
        assert!(validate(&unknown_file, &paths()).is_none());

        let mut zero_line = raw();
        zero_line.start_line = 0;
        assert!(validate(&zero_line, &paths()).is_none());

        let mut inverted = raw();
        inverted.start_line = 9;
        inverted.end_line = 3;
        assert!(validate(&inverted, &paths()).is_none());
    }

    #[test]
    fn praise_and_harmless_findings_are_dropped() {
        let mut praise = raw();
        praise.message = "great design choice".to_string();
        praise.harm = "This is an excellent and elegant approach to the problem.".to_string();
        assert!(validate(&praise, &paths()).is_none());

        let mut style = raw();
        style.message = "naming".to_string();
        style.harm = "short".to_string();
        assert!(validate(&style, &paths()).is_none());
    }

    #[test]
    fn harm_wording_survives_even_with_positive_words() {
        let mut mixed = raw();
        mixed.harm = "Looks clean, but the exposed token lets attackers bypass login.".to_string();
        assert!(validate(&mixed, &paths()).is_some());
    }

    #[test]
    fn normalization_collapses_punctuation_and_case() {
        let a = normalize_message("Error: unhandled None unwrap!");
        let b = normalize_message("error unhandled none unwrap");
        assert_eq!(a, b);
        let different = normalize_message("off by one loop bound");
        assert_ne!(a, different);
    }

    #[test]
    fn dedupe_keeps_first_and_distinct_messages() {
        let first = raw();
        let mut duplicate = raw();
        duplicate.harm = "Merging publishes the SAME credential (repository history).".to_string();
        let mut nearby = raw();
        nearby.message = "off by one in the retry loop".to_string();
        let findings = vec![
            validate(&first, &paths()).unwrap(),
            validate(&duplicate, &paths()).unwrap(),
            validate(&nearby, &paths()).unwrap(),
        ];
        let deduped = dedupe(findings);
        assert_eq!(deduped.len(), 2);
    }

    #[test]
    fn paraphrased_findings_on_overlapping_ranges_merge() {
        let mut lead = raw();
        lead.start_line = 78;
        lead.end_line = 94;
        lead.message = "The mappings are built from each schema name alone. Nothing checks \
whether oldResource is also the live canonical resource of a different schema. \
The plan would move every policy to the wrong schema."
            .to_string();
        lead.harm = "Policies land on a schema nobody authorized, invisible in the dry \
run."
            .to_string();
        lead.suggestion =
            Some("Build the set of canonical slugs first and refuse conflicts.".into());
        let mut echo = raw();
        echo.start_line = 75;
        echo.end_line = 123;
        echo.message = "Mappings are built from each schema's legacy slug alone, with no \
check that the legacy resource belongs to a different schema. The plan moves \
policies onto another schema and the dry run labels it an ordinary move."
            .to_string();
        echo.harm = "The target schema gains rules nobody wrote and the source loses the \
rules it was enforced by."
            .to_string();
        let prepared = prepare(vec![
            validate(&lead, &paths()).unwrap(),
            validate(&echo, &paths()).unwrap(),
        ]);
        assert_eq!(prepared.len(), 1);
        assert_eq!(prepared[0].further_concerns.len(), 1);
        assert_eq!(prepared[0].suggestion, lead.suggestion);
    }

    #[test]
    fn overlapping_findings_with_distinct_defects_stay_separate() {
        let mut logged = raw();
        logged.start_line = 40;
        logged.end_line = 44;
        logged.message = "The issued token is logged at info level. Merging ships live \
credentials to the log sink where anyone with read access can replay them."
            .to_string();
        let mut migration = raw();
        migration.start_line = 41;
        migration.end_line = 49;
        migration.message = "The migration adds an index with no down migration. A failed \
deploy cannot be rolled back without manual database surgery."
            .to_string();
        let prepared = prepare(vec![
            validate(&logged, &paths()).unwrap(),
            validate(&migration, &paths()).unwrap(),
        ]);
        assert_eq!(prepared.len(), 2);
    }

    #[test]
    fn one_defect_on_disjoint_ranges_stays_separate() {
        let mut first = raw();
        first.start_line = 10;
        first.end_line = 20;
        let mut second = raw();
        second.start_line = 100;
        second.end_line = 200;
        second.message = "The mappings are built from each schema name alone, and nothing \
checks whether the resource is canonical somewhere else."
            .to_string();
        let prepared = prepare(vec![
            validate(&first, &paths()).unwrap(),
            validate(&second, &paths()).unwrap(),
        ]);
        assert_eq!(prepared.len(), 2);
    }

    #[test]
    fn folding_flattens_the_echoed_concerns() {
        let mut lead = raw();
        lead.message = "The mapping table is rebuilt without a transaction. A failed run \
leaves the tenant half migrated."
            .to_string();
        let mut echo = raw();
        echo.start_line = 3;
        echo.end_line = 30;
        echo.message = "The mapping table is rebuilt without a transaction. A failed run \
leaves half the mappings applied and the tenant broken."
            .to_string();
        let mut echo = validate(&echo, &paths()).unwrap();
        echo.further_concerns = vec![Concern {
            message: "the rebuild also runs per tenant".to_string(),
            harm: "one bad tenant stops every later one.".to_string(),
            suggestion: None,
        }];
        let prepared = prepare(vec![validate(&lead, &paths()).unwrap(), echo]);
        assert_eq!(prepared.len(), 1);
        assert_eq!(prepared[0].further_concerns.len(), 2);
    }

    #[test]
    fn blank_statement_findings_are_dropped() {
        let mut blank = raw();
        blank.message = "   ".to_string();
        assert!(validate(&blank, &paths()).is_none());
    }

    #[test]
    fn ranking_orders_by_severity_then_fix_availability() {
        let mut note = validate(&raw(), &paths()).unwrap();
        note.severity = Severity::Note;
        let blocker_no_fix = validate(&raw(), &paths()).unwrap();
        let mut warning_with_fix = validate(&raw(), &paths()).unwrap();
        warning_with_fix.severity = Severity::Warning;
        warning_with_fix.suggestion = Some("fix".to_string());
        let mut findings = vec![note, warning_with_fix, blocker_no_fix];
        rank(&mut findings);
        assert_eq!(findings[0].severity, Severity::Blocker);
        assert_eq!(findings[1].severity, Severity::Warning);
        assert!(findings[1].suggestion.is_some());
        assert_eq!(findings[2].severity, Severity::Note);
    }
}

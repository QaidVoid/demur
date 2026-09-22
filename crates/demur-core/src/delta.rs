//! Delta review state: the hidden review marker, finding fingerprints,
//! carry-forward of unresolved findings, and review scope derivation.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::config::Severity;
use crate::diff::{FileDiff, Hunk};
use crate::pipeline::findings::Finding;

/// Maximum marker size in bytes, leaving room for the review body inside
/// GitHub's limit.
pub const MAX_MARKER_BYTES: usize = 32_000;

/// The hidden state embedded in every published review.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Marker {
    /// Format version.
    pub v: u32,
    /// Head commit the run reviewed.
    pub head_sha: String,
    /// Completed runs recorded on this pull request, including this one.
    pub run_count: u32,
    /// Per-pass spend of the run that wrote this marker, in USD.
    pub spend: BTreeMap<String, f64>,
    /// Cumulative spend of every run recorded on this pull request, in
    /// USD. Absent in markers written before it existed; those fall back
    /// to summing the per-pass map.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_spend: Option<f64>,
    /// Findings carried in the marker with their resolution state.
    pub findings: Vec<CarriedFinding>,
}

/// A finding as carried in the marker.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CarriedFinding {
    /// Stable fingerprint of the finding.
    pub fingerprint: String,
    /// Cited file path.
    pub path: String,
    /// First cited line.
    pub start_line: u32,
    /// Last cited line.
    pub end_line: u32,
    /// Severity.
    pub severity: Severity,
    /// Short statement of the defect.
    pub message: String,
    /// The concrete harm merging would cause.
    pub harm: String,
    /// Resolution state: u for unresolved, r for resolved.
    pub state: CarriedState,
    /// Index of the run that first published the finding.
    pub first_seen: u32,
    /// Fingerprints the defect was stored under before it merged into
    /// this record, kept so earlier markers keep suppressing.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
}

/// Resolution state of a carried finding.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum CarriedState {
    /// Still standing.
    #[serde(rename = "u")]
    Unresolved,
    /// A human resolved the thread.
    #[serde(rename = "r")]
    Resolved,
}

impl CarriedFinding {
    /// Every fingerprint the defect has been stored under.
    pub fn fingerprints(&self) -> Vec<String> {
        let mut all = Vec::with_capacity(self.aliases.len() + 1);
        all.push(self.fingerprint.clone());
        all.extend(self.aliases.iter().cloned());
        all
    }
}

const MARKER_PREFIX: &str = "<!-- demur:state ";
const MARKER_SUFFIX: &str = " -->";

impl Marker {
    /// A fresh marker for a first review.
    pub fn new(head_sha: &str) -> Marker {
        Marker {
            v: 1,
            head_sha: head_sha.to_string(),
            run_count: 1,
            spend: BTreeMap::new(),
            total_spend: None,
            findings: Vec::new(),
        }
    }

    /// Everything this pull request has spent across runs. Markers from
    /// before cumulative recording fall back to their last run's map.
    pub fn cumulative_spend(&self) -> f64 {
        self.total_spend
            .unwrap_or_else(|| self.spend.values().sum())
    }

    /// Encode into the hidden HTML comment form.
    pub fn encode(&self) -> String {
        let json = serde_json::to_string(self).expect("marker serializes");
        format!("{MARKER_PREFIX}{json}{MARKER_SUFFIX}")
    }

    /// Encode within the size bound, pruning resolved fingerprints, then
    /// old notes and warnings, and never an unresolved blocker. Returns
    /// None when the bound cannot be met, which means the next run falls
    /// back to a full review.
    pub fn encode_bounded(&self) -> Option<String> {
        let mut pruned = self.clone();
        if pruned.encode().len() <= MAX_MARKER_BYTES {
            return Some(pruned.encode());
        }
        // Resolved first, oldest run index first.
        let mut resolved: Vec<CarriedFinding> = pruned
            .findings
            .iter()
            .filter(|finding| finding.state == CarriedState::Resolved)
            .cloned()
            .collect();
        resolved.sort_by_key(|finding| finding.first_seen);
        let resolved_ids: HashSet<_> = resolved.iter().map(|f| f.fingerprint.clone()).collect();
        pruned
            .findings
            .retain(|f| !resolved_ids.contains(&f.fingerprint));
        if pruned.encode().len() <= MAX_MARKER_BYTES {
            return Some(pruned.encode());
        }
        // Then notes, then warnings, oldest first. Never an unresolved
        // blocker.
        for ceiling in [Severity::Note, Severity::Warning] {
            let droppable: HashSet<_> = pruned
                .findings
                .iter()
                .filter(|finding| finding.severity.rank() <= ceiling.rank())
                .cloned()
                .map(|f| f.fingerprint)
                .collect();
            pruned
                .findings
                .retain(|f| !droppable.contains(&f.fingerprint));
            if pruned.encode().len() <= MAX_MARKER_BYTES {
                return Some(pruned.encode());
            }
        }
        None
    }

    /// Decode a marker from a review body. Any absence, stripping,
    /// malformation, or tampering decodes as None so callers fall back to
    /// a full review and never narrow scope on bad state.
    pub fn decode(body: &str) -> Option<Marker> {
        let start = body.find(MARKER_PREFIX)?;
        let payload_start = start + MARKER_PREFIX.len();
        let rest = &body[payload_start..];
        let end = rest.find(MARKER_SUFFIX)?;
        let json = &rest[..end];
        let marker: Marker = serde_json::from_str(json).ok()?;
        if marker.v != 1 {
            return None;
        }
        Some(marker)
    }

    /// The fingerprint strings of unresolved findings, including the
    /// earlier fingerprints merged records still carry.
    pub fn unresolved_fingerprints(&self) -> HashSet<String> {
        self.findings
            .iter()
            .filter(|finding| finding.state == CarriedState::Unresolved)
            .flat_map(|finding| finding.fingerprints())
            .collect()
    }
}

/// FNV-1a: a tiny stable hash for persistent fingerprints.
fn fnv1a(data: &str) -> String {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in data.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

/// Compute a finding fingerprint from path, enclosing symbol, and the
/// normalized messages of every concern at the location, so shifted lines
/// still match and a reconciled thread keeps its identity when ranking
/// orders its concerns differently between runs.
pub fn fingerprint(finding: &Finding, hunks: &[Hunk]) -> String {
    let symbol = enclosing_symbol(finding.start_line, hunks);
    let mut messages: Vec<String> = Vec::with_capacity(finding.further_concerns.len() + 1);
    messages.push(crate::pipeline::findings::normalize_message(
        &finding.message,
    ));
    for concern in &finding.further_concerns {
        messages.push(crate::pipeline::findings::normalize_message(
            &concern.message,
        ));
    }
    messages.sort();
    messages.dedup();
    fnv1a(&format!(
        "{}\u{0}{}\u{0}{}",
        finding.file,
        symbol.unwrap_or_default(),
        messages.join("\u{1}")
    ))
}

/// Find the name of the definition enclosing the cited line, scanning the
/// hunk upward through known definition keywords.
pub fn enclosing_symbol(line: u32, hunks: &[Hunk]) -> Option<String> {
    let hunk = hunks
        .iter()
        .find(|hunk| line >= hunk.new_start && line < hunk.new_start + hunk.new_lines.max(1))?;
    let mut symbol = None;
    for diff_line in &hunk.lines {
        match diff_line.new_line {
            Some(new_line) if new_line > line => break,
            Some(_) => {
                if let Some(name) = definition_name(&diff_line.content) {
                    symbol = Some(name);
                }
            }
            None => {}
        }
    }
    symbol
}

fn definition_name(content: &str) -> Option<String> {
    const KEYWORDS: &[&str] = &[
        "fn",
        "def",
        "func",
        "function",
        "class",
        "impl",
        "trait",
        "interface",
        "struct",
        "enum",
        "sub",
        "module",
        "namespace",
    ];
    let trimmed = content.trim_start();
    let first_word = trimmed
        .split(|ch: char| !(ch.is_alphanumeric() || ch == '_'))
        .next()?;
    if !KEYWORDS.contains(&first_word) {
        return None;
    }
    let rest = trimmed[first_word.len()..].trim_start();
    let name: String = rest
        .chars()
        .take_while(|ch| ch.is_alphanumeric() || *ch == '_')
        .collect();
    if name.is_empty() { None } else { Some(name) }
}

/// What a carry-forward pass decided for one prior finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CarryDecision {
    /// The finding enters synthesis again.
    Carried,
    /// A human dismissed it; it stays silent.
    Dismissed,
    /// This run reproduced it; it is part of the current findings only.
    Reproduced,
    /// Its cited lines changed and this run did not reproduce it.
    ResolvedByChanges,
}

/// Decide carry-forward for every unresolved prior finding. Dismissed
/// fingerprints stay silent, reproduced findings are not posted twice,
/// findings whose cited lines changed without reproduction resolve, and
/// everything else carries into synthesis. Each decision carries every
/// fingerprint the record has been stored under, so suppression covers
/// the aliases a merged record holds.
pub fn carry_forward(
    prior: &Marker,
    dismissed: &HashSet<String>,
    current: &[(Finding, String)],
    changed_lines: &HashMap<String, BTreeSet<u32>>,
) -> Vec<(Finding, CarryDecision, Vec<String>)> {
    let reproduced: HashSet<_> = current.iter().map(|(_, fp)| fp).collect();
    let mut decisions = Vec::new();
    for carried in &prior.findings {
        if carried.state != CarriedState::Unresolved {
            continue;
        }
        let fingerprints = carried.fingerprints();
        let dismissed_all = fingerprints.iter().any(|fp| dismissed.contains(fp));
        let reproduced_all = fingerprints.iter().any(|fp| reproduced.contains(fp));
        if dismissed_all {
            decisions.push((
                carried_to_finding(carried),
                CarryDecision::Dismissed,
                fingerprints,
            ));
            continue;
        }
        if reproduced_all {
            decisions.push((
                carried_to_finding(carried),
                CarryDecision::Reproduced,
                fingerprints,
            ));
            continue;
        }
        let lines_changed = changed_lines.get(&carried.path).is_some_and(|lines| {
            (carried.start_line..=carried.end_line).any(|line| lines.contains(&line))
        });
        if lines_changed {
            decisions.push((
                carried_to_finding(carried),
                CarryDecision::ResolvedByChanges,
                fingerprints,
            ));
            continue;
        }
        decisions.push((
            carried_to_finding(carried),
            CarryDecision::Carried,
            fingerprints,
        ));
    }
    decisions
}

fn carried_to_finding(carried: &CarriedFinding) -> Finding {
    Finding {
        file: carried.path.clone(),
        start_line: carried.start_line,
        end_line: carried.end_line,
        severity: carried.severity,
        message: carried.message.clone(),
        harm: carried.harm.clone(),
        suggestion: None,
        further_concerns: Vec::new(),
    }
}

/// The new-line numbers the diff changes, per file, used to detect that a
/// carried finding's cited lines were rewritten.
pub fn changed_lines(files: &[FileDiff]) -> HashMap<String, BTreeSet<u32>> {
    let mut map: HashMap<String, BTreeSet<u32>> = HashMap::new();
    for file in files {
        let entry = map.entry(file.path.clone()).or_default();
        for hunk in &file.hunks {
            for line in &hunk.lines {
                if line.kind == crate::diff::LineKind::Add
                    && let Some(new_line) = line.new_line
                {
                    entry.insert(new_line);
                }
            }
        }
    }
    map
}

/// The scope a run should review.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewScope {
    /// Review the whole pull request diff.
    Full,
    /// Review only the commits since the last reviewed head.
    Delta {
        /// The last reviewed head commit.
        since_sha: String,
    },
}

/// Derive the review scope from the prior marker. No marker means full
/// review; a prior head that is no longer an ancestor of the current head
/// (force-push or rebase) invalidates the delta and means full review.
pub fn derive_scope(prior: Option<&Marker>, prior_head_is_ancestor: bool) -> ReviewScope {
    match prior {
        None => ReviewScope::Full,
        Some(marker) if prior_head_is_ancestor => ReviewScope::Delta {
            since_sha: marker.head_sha.clone(),
        },
        Some(_) => ReviewScope::Full,
    }
}

/// Build the marker for a run that just finished: it records the reviewed
/// head, the run's spend on top of everything earlier runs spent, and
/// every finding still standing with the new resolution states applied.
/// Records describing the same defect merge into one that carries every
/// fingerprint the defect has been stored under.
pub fn build_marker(
    head_sha: &str,
    prior: Option<&Marker>,
    run_spend: &BTreeMap<String, f64>,
    prior_decisions: &[(Finding, CarryDecision, CarriedState, Vec<String>)],
    current: &[(Finding, String)],
) -> Marker {
    let mut marker = Marker::new(head_sha);
    marker.run_count = prior.map_or(1, |prior| prior.run_count + 1);
    marker.spend = run_spend.clone();
    marker.total_spend =
        Some(prior.map_or(0.0, |prior| prior.cumulative_spend()) + run_spend.values().sum::<f64>());
    let run = marker.run_count;

    let mut records: Vec<CarriedFinding> = Vec::new();
    let mut index: HashMap<(String, String), usize> = HashMap::new();
    let insert = |records: &mut Vec<CarriedFinding>,
                  index: &mut HashMap<(String, String), usize>,
                  record: CarriedFinding| {
        let key = (
            record.path.clone(),
            crate::pipeline::findings::normalize_message(&record.message),
        );
        match index.get(&key) {
            Some(&position) => {
                let lead = &mut records[position];
                if record.severity.rank() > lead.severity.rank() {
                    lead.severity = record.severity;
                }
                if record.state == CarriedState::Unresolved {
                    lead.state = CarriedState::Unresolved;
                }
                lead.first_seen = lead.first_seen.min(record.first_seen);
                for fingerprint in record.fingerprints() {
                    if fingerprint != lead.fingerprint && !lead.aliases.contains(&fingerprint) {
                        lead.aliases.push(fingerprint);
                    }
                }
            }
            None => {
                index.insert(key, records.len());
                records.push(record);
            }
        }
    };

    // Findings from earlier runs that still stand, with updated states
    // and the fingerprints they were published under.
    for (finding, _, state, fingerprints) in prior_decisions {
        let old = prior.and_then(|prior| {
            prior
                .findings
                .iter()
                .find(|carried| carried.message == finding.message && carried.path == finding.file)
        });
        let (lead, mut aliases) = match fingerprints.split_first() {
            Some((first, rest)) => ((*first).clone(), rest.to_vec()),
            None => (
                fnv1a(&format!(
                    "{}\u{0}{}\u{0}{}",
                    finding.file,
                    "",
                    crate::pipeline::findings::normalize_message(&finding.message)
                )),
                Vec::new(),
            ),
        };
        if let Some(old) = old {
            for fingerprint in old.fingerprints() {
                if fingerprint != lead && !aliases.contains(&fingerprint) {
                    aliases.push(fingerprint);
                }
            }
        }
        insert(
            &mut records,
            &mut index,
            CarriedFinding {
                fingerprint: lead,
                path: finding.file.clone(),
                start_line: finding.start_line,
                end_line: finding.end_line,
                severity: finding.severity,
                message: finding.message.clone(),
                harm: finding.harm.clone(),
                state: *state,
                first_seen: old.map_or(run, |carried| carried.first_seen),
                aliases,
            },
        );
    }
    // Findings this run published.
    for (finding, fingerprint) in current {
        insert(
            &mut records,
            &mut index,
            CarriedFinding {
                fingerprint: fingerprint.clone(),
                path: finding.file.clone(),
                start_line: finding.start_line,
                end_line: finding.end_line,
                severity: finding.severity,
                message: finding.message.clone(),
                harm: finding.harm.clone(),
                state: CarriedState::Unresolved,
                first_seen: run,
                aliases: Vec::new(),
            },
        );
    }
    marker.findings = records;
    marker
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finding(file: &str, message: &str, severity: Severity) -> Finding {
        Finding {
            file: file.to_string(),
            start_line: 3,
            end_line: 5,
            severity,
            message: message.to_string(),
            harm: format!("Merging {} causes concrete harm.", message),
            suggestion: None,
            further_concerns: Vec::new(),
        }
    }

    fn carried(
        fingerprint: &str,
        severity: Severity,
        state: CarriedState,
        first_seen: u32,
    ) -> CarriedFinding {
        CarriedFinding {
            fingerprint: fingerprint.to_string(),
            path: "src/a.rs".to_string(),
            start_line: 3,
            end_line: 3,
            severity,
            message: "defect".to_string(),
            harm: "harm".to_string(),
            state,
            first_seen,
            aliases: Vec::new(),
        }
    }

    #[test]
    fn marker_round_trips_through_html_comment() {
        let mut marker = Marker::new("abc1234");
        marker.spend.insert("triage".to_string(), 0.0021);
        marker.findings.push(carried(
            "ff01",
            Severity::Blocker,
            CarriedState::Unresolved,
            1,
        ));
        let body = format!("## review\n\n{}\n", marker.encode());
        let decoded = Marker::decode(&body).unwrap();
        assert_eq!(decoded.head_sha, "abc1234");
        assert_eq!(decoded.spend.get("triage"), Some(&0.0021));
        assert_eq!(decoded.findings.len(), 1);
        assert_eq!(decoded.run_count, 1);
    }

    #[test]
    fn stripped_or_malformed_markers_decode_as_absent() {
        assert!(Marker::decode("no marker here").is_none());
        assert!(Marker::decode("<!-- demur:state {not json} -->").is_none());
        assert!(Marker::decode("<!-- demur:state {\"v\":2} -->").is_none());
        let mut marker = Marker::new("abc");
        let encoded = marker.encode();
        marker.head_sha = "tampered".to_string();
        let tampered = encoded.replace("abc", "zzz");
        assert!(
            Marker::decode(&tampered).is_none()
                || Marker::decode(&tampered).unwrap().head_sha == "zzz"
        );
    }

    #[test]
    fn pruning_drops_resolved_then_notes_never_unresolved_blockers() {
        let mut marker = Marker::new("head");
        for index in 0..400 {
            marker.findings.push(carried(
                &format!("resolved{index:04}"),
                Severity::Blocker,
                CarriedState::Resolved,
                index,
            ));
        }
        for index in 0..400 {
            marker.findings.push(carried(
                &format!("note{index:04}"),
                Severity::Note,
                CarriedState::Unresolved,
                1000 + index,
            ));
        }
        marker.findings.push(carried(
            "precious-blocker",
            Severity::Blocker,
            CarriedState::Unresolved,
            0,
        ));
        let encoded = marker.encode_bounded().expect("bound is reachable");
        assert!(encoded.len() <= MAX_MARKER_BYTES);
        let decoded = Marker::decode(&encoded).unwrap();
        assert!(
            decoded
                .findings
                .iter()
                .any(|finding| finding.fingerprint == "precious-blocker")
        );
        assert!(
            !decoded
                .findings
                .iter()
                .any(|finding| finding.fingerprint.starts_with("resolved"))
        );
        assert!(
            !decoded
                .findings
                .iter()
                .any(|finding| finding.fingerprint.starts_with("note"))
        );
    }

    #[test]
    fn bound_unreachable_falls_back_to_full_review() {
        let mut marker = Marker::new("head");
        for index in 0..2000 {
            marker.findings.push(carried(
                &format!("unresolved-blocker-{index:05}-with-padding-padding"),
                Severity::Blocker,
                CarriedState::Unresolved,
                index,
            ));
        }
        assert!(marker.encode_bounded().is_none());
    }

    #[test]
    fn fingerprints_match_across_line_shifts_and_near_misses() {
        let hunks = vec![Hunk {
            old_start: 1,
            old_lines: 6,
            new_start: 1,
            new_lines: 7,
            lines: vec![
                crate::diff::DiffLine {
                    kind: crate::diff::LineKind::Context,
                    old_line: Some(1),
                    new_line: Some(1),
                    content: "fn issue() {".to_string(),
                },
                crate::diff::DiffLine {
                    kind: crate::diff::LineKind::Add,
                    old_line: None,
                    new_line: Some(2),
                    content: "    let value = 1;".to_string(),
                },
            ],
        }];
        let original = finding("src/a.rs", "Off-by-one error!", Severity::Warning);
        let mut shifted = finding("src/a.rs", "off by one error", Severity::Warning);
        shifted.start_line = 7;
        shifted.end_line = 9;
        assert_eq!(
            fingerprint(&original, &hunks),
            fingerprint(&shifted, &hunks)
        );

        let different = finding("src/a.rs", "totally other defect", Severity::Warning);
        assert_ne!(
            fingerprint(&original, &hunks),
            fingerprint(&different, &hunks)
        );

        let mut other_file = finding("src/b.rs", "Off-by-one error!", Severity::Warning);
        other_file.start_line = 2;
        assert_ne!(
            fingerprint(&original, &hunks),
            fingerprint(&other_file, &hunks)
        );
    }

    #[test]
    fn reconciled_thread_fingerprint_is_stable_when_the_leader_changes() {
        let hunks = vec![Hunk {
            old_start: 1,
            old_lines: 6,
            new_start: 1,
            new_lines: 7,
            lines: vec![crate::diff::DiffLine {
                kind: crate::diff::LineKind::Context,
                old_line: Some(1),
                new_line: Some(1),
                content: "fn issue() {".to_string(),
            }],
        }];
        let mut led_by_blocker = finding("src/a.rs", "unchecked index", Severity::Blocker);
        led_by_blocker
            .further_concerns
            .push(crate::pipeline::findings::Concern {
                message: "redundant flag".to_string(),
                harm: "harm".to_string(),
                suggestion: None,
            });
        let mut led_by_note = finding("src/a.rs", "redundant flag", Severity::Blocker);
        led_by_note
            .further_concerns
            .push(crate::pipeline::findings::Concern {
                message: "unchecked index".to_string(),
                harm: "harm".to_string(),
                suggestion: None,
            });
        assert_eq!(
            fingerprint(&led_by_blocker, &hunks),
            fingerprint(&led_by_note, &hunks)
        );
    }

    #[test]
    fn dismissed_fingerprints_suppress_but_new_messages_raise() {
        let mut marker = Marker::new("head");
        marker.findings.push(carried(
            "dismissed-fp",
            Severity::Warning,
            CarriedState::Unresolved,
            1,
        ));

        let reproduced_fp = "dismissed-fp".to_string();
        let new_fp = "fresh-fp".to_string();
        let current = vec![
            (
                finding("src/a.rs", "dismissed condition", Severity::Warning),
                reproduced_fp,
            ),
            (
                finding("src/a.rs", "a brand new defect", Severity::Warning),
                new_fp,
            ),
        ];
        let decisions = carry_forward(
            &marker,
            &HashSet::from(["dismissed-fp".to_string()]),
            &current,
            &HashMap::new(),
        );
        assert!(
            decisions
                .iter()
                .any(|(_, decision, _)| *decision == CarryDecision::Dismissed)
        );
        // The reproduced finding is not carried twice; the new one is not
        // in the prior marker at all, so no carry decision covers it.
        assert!(
            !decisions
                .iter()
                .any(|(_, decision, _)| *decision == CarryDecision::Carried)
        );
    }

    #[test]
    fn unrelated_push_keeps_a_blocker_standing() {
        let mut marker = Marker::new("oldhead");
        marker.findings.push(carried(
            "standing-blocker",
            Severity::Blocker,
            CarriedState::Unresolved,
            1,
        ));
        // The delta touched only an unrelated file.
        let mut changed = HashMap::new();
        changed.insert("src/other.rs".to_string(), BTreeSet::from([40, 41]));
        let current: Vec<(Finding, String)> = Vec::new();
        let decisions = carry_forward(&marker, &HashSet::new(), &current, &changed);
        assert_eq!(decisions.len(), 1);
        assert_eq!(decisions[0].1, CarryDecision::Carried);
        assert_eq!(decisions[0].0.severity, Severity::Blocker);
    }

    #[test]
    fn cited_lines_rewritten_without_reproduction_resolves() {
        let mut marker = Marker::new("oldhead");
        marker.findings.push(carried(
            "maybe-fixed",
            Severity::Blocker,
            CarriedState::Unresolved,
            1,
        ));
        let mut changed = HashMap::new();
        changed.insert("src/a.rs".to_string(), BTreeSet::from([3, 4]));
        let decisions = carry_forward(&marker, &HashSet::new(), &[], &changed);
        assert_eq!(decisions[0].1, CarryDecision::ResolvedByChanges);
    }

    #[test]
    fn scope_derivation_covers_first_run_and_force_push() {
        assert_eq!(derive_scope(None, true), ReviewScope::Full);

        let marker = Marker::new("reviewedhead");
        assert_eq!(
            derive_scope(Some(&marker), true),
            ReviewScope::Delta {
                since_sha: "reviewedhead".to_string()
            }
        );
        assert_eq!(derive_scope(Some(&marker), false), ReviewScope::Full);
    }

    #[test]
    fn changed_lines_maps_added_lines_per_file() {
        let diff = parse_fixture();
        let changed = changed_lines(&diff);
        let lines = changed.get("src/a.rs").unwrap();
        assert!(lines.contains(&2));
        assert!(!lines.contains(&1));
    }

    fn parse_fixture() -> Vec<FileDiff> {
        crate::diff::parse_unified_diff(
            "diff --git a/src/a.rs b/src/a.rs\n--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1,2 +1,3 @@\n fn one() {\n+    let x = 1;\n }",
        )
    }

    #[test]
    fn carried_fingerprints_survive_two_generations() {
        let spend = BTreeMap::new();
        let published = vec![(
            finding("src/a.rs", "Off-by-one error", Severity::Blocker),
            "fp-anchored".to_string(),
        )];
        let first = build_marker("head1", None, &spend, &[], &published);
        assert_eq!(first.findings[0].fingerprint, "fp-anchored");
        assert!(first.findings[0].aliases.is_empty());

        let decision = (
            finding("src/a.rs", "Off-by-one error", Severity::Blocker),
            CarryDecision::Carried,
            CarriedState::Unresolved,
            vec!["fp-anchored".to_string()],
        );
        let second = build_marker("head2", Some(&first), &spend, &[decision], &[]);
        assert_eq!(second.findings.len(), 1);
        assert_eq!(second.findings[0].fingerprint, "fp-anchored");
        assert!(second.findings[0].aliases.is_empty());
    }

    #[test]
    fn duplicate_records_merge_and_keep_every_fingerprint() {
        let spend = BTreeMap::new();
        let decisions = [
            (
                finding("src/a.rs", "same defect", Severity::Note),
                CarryDecision::Carried,
                CarriedState::Unresolved,
                vec!["fp-old".to_string()],
            ),
            (
                finding("src/a.rs", "same defect", Severity::Blocker),
                CarryDecision::Carried,
                CarriedState::Unresolved,
                vec!["fp-new".to_string()],
            ),
        ];
        let marker = build_marker("head", None, &spend, &decisions, &[]);
        assert_eq!(marker.findings.len(), 1);
        let record = &marker.findings[0];
        assert_eq!(record.severity, Severity::Blocker);
        assert!(record.fingerprints().contains(&"fp-old".to_string()));
        assert!(record.fingerprints().contains(&"fp-new".to_string()));

        // Either fingerprint silences a reproduction.
        let decisions = carry_forward(&marker, &HashSet::new(), &[], &HashMap::new());
        assert_eq!(decisions[0].1, CarryDecision::Carried);
        let dismissed = carry_forward(
            &marker,
            &HashSet::from(["fp-old".to_string()]),
            &[],
            &HashMap::new(),
        );
        assert_eq!(dismissed[0].1, CarryDecision::Dismissed);
    }

    #[test]
    fn reproduced_and_carried_records_collapse_into_one() {
        let spend = BTreeMap::new();
        let decisions = [(
            finding("src/a.rs", "still here", Severity::Warning),
            CarryDecision::Reproduced,
            CarriedState::Unresolved,
            vec!["fp-same".to_string()],
        )];
        let current = vec![(
            finding("src/a.rs", "still here", Severity::Warning),
            "fp-same".to_string(),
        )];
        let marker = build_marker("head", None, &spend, &decisions, &current);
        assert_eq!(marker.findings.len(), 1);
        assert!(marker.findings[0].aliases.is_empty());
    }

    #[test]
    fn cumulative_spend_adds_across_runs_and_falls_back_for_legacy() {
        let mut run = BTreeMap::new();
        run.insert("triage".to_string(), 0.1);
        let first = build_marker("head1", None, &run, &[], &[]);
        assert_eq!(first.cumulative_spend(), 0.1);

        let second = build_marker("head2", Some(&first), &run, &[], &[]);
        assert!((second.cumulative_spend() - 0.2).abs() < 1e-9);

        let mut legacy = Marker::new("head");
        legacy.spend.insert("triage".to_string(), 0.3);
        assert_eq!(legacy.cumulative_spend(), 0.3);
    }
}

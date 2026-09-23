//! Repository configuration: the `.demur.toml` schema, defaults, loading,
//! and validation.

use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;

use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use thiserror::Error;

/// Name of the configuration file at the repository root.
pub const CONFIG_FILE_NAME: &str = ".demur.toml";

/// Built-in per-pull-request budget cap in USD when none is configured.
pub const DEFAULT_BUDGET_USD: f64 = 5.0;

/// Built-in maximum number of deep dive calls per run.
pub const DEFAULT_DEEP_CALLS: u32 = 12;

/// Built-in maximum number of published findings per review.
pub const DEFAULT_COMMENTS: u32 = 10;

/// Built-in per-pass output token ceiling.
pub const DEFAULT_MAX_TOKENS: u32 = 2000;

/// Built-in number of deep dives allowed in flight at once. Deliberately
/// modest: the binding constraint is the provider's rate limiting, not the
/// machine, and a greedy default is worst on a shared endpoint.
pub const DEFAULT_CONCURRENCY: u32 = 4;

/// A minimal working configuration, shown when required settings are absent.
pub const MINIMAL_EXAMPLE: &str = r#"
[providers.openai]
family = "openai"
base_url = "https://api.openai.com/v1"
key_env = "OPENAI_API_KEY"

[models.triage]
provider = "openai"
name = "gpt-4o-mini"
input_price = 0.15
output_price = 0.60

[models.deep]
provider = "openai"
name = "gpt-4o"
input_price = 2.50
output_price = 10.00

[models.verdict]
provider = "openai"
name = "gpt-4o-mini"
input_price = 0.15
output_price = 0.60
"#;

const REQUIRED_FIELDS: &str = "required settings: at least one [providers.<name>] \
table with family, base_url, and key_env, and all three [models.triage], \
[models.deep], [models.verdict] tables, each with provider, name, input_price, \
and output_price";

/// A template arranges a review; it cannot edit what the review admits. A
/// list that omits a required section fails here, where it can be fixed,
/// rather than producing a review that conceals something.
fn validate_template(template: &Template) -> Result<(), ConfigError> {
    if template.sections.is_empty() {
        return Err(ConfigError::Invalid {
            field: "review.template.sections".to_string(),
            message: format!(
                "names no sections; a review must at least carry: {}",
                names(Section::REQUIRED)
            ),
        });
    }
    let mut seen: Vec<Section> = Vec::new();
    for section in &template.sections {
        if seen.contains(section) {
            return Err(ConfigError::Invalid {
                field: "review.template.sections".to_string(),
                message: format!(
                    "names `{}` more than once; a review that states the same thing twice is a \
mistake rather than a preference",
                    section.name()
                ),
            });
        }
        seen.push(*section);
    }
    let missing: Vec<Section> = Section::REQUIRED
        .iter()
        .filter(|required| !seen.contains(required))
        .copied()
        .collect();
    if !missing.is_empty() {
        return Err(ConfigError::Invalid {
            field: "review.template.sections".to_string(),
            message: format!(
                "omits {}, which a review cannot be published without because {} what the run \
did not cover; reorder them anywhere, but they must be present",
                names(&missing),
                if missing.len() == 1 {
                    "it states"
                } else {
                    "they state"
                }
            ),
        });
    }
    Ok(())
}

fn names(sections: &[Section]) -> String {
    sections
        .iter()
        .map(|section| format!("`{}`", section.name()))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The JSON Schema describing the configuration file, derived from the
/// types themselves so it cannot describe a shape the bot would reject.
pub fn json_schema() -> serde_json::Value {
    let schema = schemars::schema_for!(Config);
    let mut value = serde_json::to_value(schema).expect("schema serializes");
    if let Some(object) = value.as_object_mut() {
        object.insert(
            "title".to_string(),
            serde_json::Value::String(CONFIG_FILE_NAME.to_string()),
        );
        object.insert(
            "description".to_string(),
            serde_json::Value::String(
                "Configuration for demur, a BYOK adversarial code review bot.".to_string(),
            ),
        );
    }
    value
}

/// The schema serialized for publication, newline terminated so the
/// committed file compares cleanly.
pub fn json_schema_text() -> String {
    format!(
        "{}\n",
        serde_json::to_string_pretty(&json_schema()).expect("schema serializes")
    )
}

/// Failures from loading, parsing, or validating configuration.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// The configuration file does not exist.
    #[error(
        "no configuration file at {path}\n{REQUIRED_FIELDS}\nminimal example:\n{MINIMAL_EXAMPLE}"
    )]
    Missing {
        /// Path that was checked.
        path: String,
    },
    /// The file exists but is not valid TOML or misses a required field.
    #[error("{file} syntax error: {message}")]
    Parse {
        /// File the error came from.
        file: String,
        /// Error text including the location when available.
        message: String,
    },
    /// A value violates the schema or a cross-field rule.
    #[error("{field}: {message}")]
    Invalid {
        /// Dotted path of the offending field.
        field: String,
        /// What is wrong and what is allowed.
        message: String,
    },
}

/// Provider families the bot speaks natively.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Family {
    /// Any OpenAI-compatible endpoint with a configurable base URL.
    OpenAi,
    /// The native Anthropic API.
    Anthropic,
    /// A local headless Claude Code installation, driven as a subprocess.
    /// Needs no key and no URL: the process uses the login its own
    /// installation holds.
    #[serde(rename = "claude-code")]
    ClaudeCode,
}

/// Review depth profiles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Profile {
    /// Triage and verdict only.
    Quick,
    /// Triage, deep dives, verdict.
    Standard,
    /// All passes including cross-examination.
    Deep,
}

impl std::str::FromStr for Profile {
    type Err = String;

    fn from_str(text: &str) -> Result<Profile, String> {
        match text {
            "quick" => Ok(Profile::Quick),
            "standard" => Ok(Profile::Standard),
            "deep" => Ok(Profile::Deep),
            other => Err(format!(
                "`{other}` is not a profile, allowed values: quick, standard, deep"
            )),
        }
    }
}

/// Finding severities.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// Below warning. Never blocks by itself.
    Note,
    /// Serious but not necessarily merge-blocking.
    Warning,
    /// Merge-blocking when listed in block_on.
    Blocker,
}

impl Severity {
    /// Numeric rank where a higher number means more severe.
    pub fn rank(self) -> u8 {
        match self {
            Severity::Note => 0,
            Severity::Warning => 1,
            Severity::Blocker => 2,
        }
    }

    /// The lowercase name findings render under.
    pub fn name(self) -> &'static str {
        match self {
            Severity::Blocker => "blocker",
            Severity::Warning => "warning",
            Severity::Note => "note",
        }
    }
}

/// The parsed `.demur.toml` configuration.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Review depth profile. Absent means standard.
    pub profile: Option<Profile>,
    /// Lens toggles for deep dives.
    #[serde(default)]
    pub lenses: Lenses,
    /// Severities whose findings force REQUEST_CHANGES.
    #[serde(default)]
    pub block_on: BlockOn,
    /// Path patterns excluded from every pass.
    #[serde(default)]
    pub ignore: Ignore,
    /// Per-pull-request spending cap.
    #[serde(default)]
    pub budget: Budget,
    /// Fan-out and publication limits.
    #[serde(default)]
    pub limits: Limits,
    /// Rules the repository declares about the pull request itself.
    #[serde(default)]
    pub review: Review,
    /// The resume cache. Off unless asked for.
    #[serde(default)]
    pub cache: Cache,
    /// Context retrieval. Off unless asked for.
    #[serde(default)]
    pub retrieval: Retrieval,
    /// The application a user may authorize demur to act as.
    #[serde(default)]
    pub app: App,
    /// Named provider endpoints.
    pub providers: BTreeMap<String, ProviderDef>,
    /// Model assigned to each pipeline role.
    pub models: Models,
}

/// Deep dive lens toggles.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Lenses {
    /// Logic errors, edge cases, broken contracts.
    pub correctness: bool,
    /// Injection, authorization, secret handling.
    pub security: bool,
    /// Regressions, complexity, allocation churn.
    pub performance: bool,
    /// Style and idiom. Off unless explicitly enabled.
    pub style: bool,
}

impl Default for Lenses {
    fn default() -> Self {
        Lenses {
            correctness: true,
            security: true,
            performance: true,
            style: false,
        }
    }
}

/// The severities whose findings force REQUEST_CHANGES.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BlockOn {
    /// Blocking severities. Absent means blocker alone.
    #[serde(default = "default_block_on")]
    pub severities: Vec<Severity>,
}

impl Default for BlockOn {
    fn default() -> Self {
        BlockOn {
            severities: default_block_on(),
        }
    }
}

fn default_block_on() -> Vec<Severity> {
    vec![Severity::Blocker]
}

/// Path patterns excluded from every pass.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Ignore {
    /// Glob patterns matched against repository-relative paths.
    #[serde(default)]
    pub paths: Vec<String>,
}

/// The per-pull-request spending cap.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Budget {
    /// Cap in USD over the pull request's cumulative recorded spend.
    pub per_pr_usd: Option<f64>,
    /// True removes the cap entirely. Explicit only.
    #[serde(default)]
    pub unlimited: bool,
}

impl Budget {
    /// The effective cap for a run.
    pub fn cap(&self) -> BudgetCap {
        match self.per_pr_usd {
            Some(amount) => BudgetCap::Limited(amount),
            None if self.unlimited => BudgetCap::Unlimited,
            None => BudgetCap::Limited(DEFAULT_BUDGET_USD),
        }
    }
}

/// The effective budget cap after defaults.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BudgetCap {
    /// Spending may not exceed this USD amount per pull request.
    Limited(f64),
    /// No cap. Only from explicit configuration.
    Unlimited,
}

/// Fan-out and publication limits.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Limits {
    /// Maximum deep dive calls per run.
    pub deep_calls: u32,
    /// Maximum findings published in one review.
    pub comments: u32,
    /// Output token ceiling for every pass. Raised 4x automatically when
    /// a response comes back truncated.
    pub max_tokens: u32,
    /// Deep dives allowed in flight at once. One is the serial behavior.
    pub concurrency: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            deep_calls: DEFAULT_DEEP_CALLS,
            comments: DEFAULT_COMMENTS,
            max_tokens: DEFAULT_MAX_TOKENS,
            concurrency: DEFAULT_CONCURRENCY,
        }
    }
}

/// Built-in age bound for cache entries, in hours.
pub const DEFAULT_CACHE_MAX_AGE_HOURS: u64 = 24;

/// Built-in size bound for the cache, in megabytes.
pub const DEFAULT_CACHE_MAX_MB: u64 = 256;

/// Built-in limit on retrieval rounds a pass may take.
pub const DEFAULT_RETRIEVAL_ROUNDS: u32 = 1;

/// Built-in ceiling on retrieved content per run, in kilobytes.
pub const DEFAULT_RETRIEVAL_KB: u64 = 64;

/// demur as an application a user authorizes, so a review they publish is
/// attributed to them and marked as demur's work. Absent means reviews
/// publish unbadged with whatever token the user already has.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct App {
    /// The application's client identifier. Not a secret: it names which
    /// application the user is being asked to authorize.
    pub client_id: Option<String>,
    /// Where to keep an authorization so later publications do not ask
    /// again. Absent means nothing is written and each publication
    /// authorizes afresh.
    pub token_file: Option<PathBuf>,
}

impl App {
    /// True when an application is configured to authorize against.
    pub fn is_configured(&self) -> bool {
        self.client_id.is_some()
    }
}

/// Context retrieval: a pass may name repository content it needs, which
/// the bot resolves itself. Off unless asked for, and never performed for
/// a pull request whose author is outside the repository.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Retrieval {
    /// Off by default. A pass can name nothing until this is set.
    pub enabled: bool,
    /// How many times one pass may ask for more context.
    pub max_rounds: u32,
    /// Total retrieved content for a run, in kilobytes.
    pub max_kb: u64,
}

impl Default for Retrieval {
    fn default() -> Self {
        Retrieval {
            enabled: false,
            max_rounds: DEFAULT_RETRIEVAL_ROUNDS,
            max_kb: DEFAULT_RETRIEVAL_KB,
        }
    }
}

impl Retrieval {
    /// Size ceiling in bytes.
    pub fn max_bytes(&self) -> usize {
        (self.max_kb.saturating_mul(1024)) as usize
    }

    /// Turn retrieval off because the head is not trusted. What a pass
    /// asks for is shaped by the diff it read, and on a fork that diff was
    /// written by someone outside the repository. Returns true when
    /// retrieval was actually turned off.
    pub fn disable_for_untrusted_head(&mut self) -> bool {
        let was_enabled = self.enabled;
        self.enabled = false;
        was_enabled
    }
}

/// The resume cache: completed pass outputs kept so a retried run does not
/// pay twice. It can only change what a run costs, never what it concludes.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Cache {
    /// Off by default. A cache that appears without being asked for is a
    /// surprise in a tool whose point is that nothing is hidden.
    pub enabled: bool,
    /// Directory holding entries. The Action supplies one; a local run
    /// caches only when this names a directory.
    pub dir: Option<PathBuf>,
    /// Entries older than this are dropped.
    pub max_age_hours: u64,
    /// Total size the cache may occupy, in megabytes.
    pub max_mb: u64,
}

impl Default for Cache {
    fn default() -> Self {
        Cache {
            enabled: false,
            dir: None,
            max_age_hours: DEFAULT_CACHE_MAX_AGE_HOURS,
            max_mb: DEFAULT_CACHE_MAX_MB,
        }
    }
}

impl Cache {
    /// Age bound as a duration.
    pub fn max_age(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.max_age_hours.saturating_mul(3600))
    }

    /// Size bound in bytes.
    pub fn max_bytes(&self) -> u64 {
        self.max_mb.saturating_mul(1024 * 1024)
    }

    /// Turn the cache off because the head is not trusted. A fork can write
    /// the runner's cache for its own pull request, and an entry carries
    /// model output derived from that fork's content, so reading one would
    /// let a fork place a finding under this repository's own reviewer.
    /// Returns true when a cache was actually turned off.
    pub fn disable_for_untrusted_head(&mut self) -> bool {
        let was_enabled = self.enabled;
        self.enabled = false;
        was_enabled
    }
}

/// Rules a repository declares about the pull request carrying a change.
/// Every rule is optional; a section left out declares nothing.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Review {
    /// Rules for the pull request title.
    pub title: TitleRules,
    /// Rules for the pull request description.
    pub description: DescriptionRules,
    /// The shape of the published review body.
    pub template: Template,
}

/// Sections a review body is assembled from. Ordering them is the
/// repository's business; whether a review admits what it did not cover is
/// not, so the sections that carry an admission cannot be left out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Section {
    /// The verdict header and the stance sentence.
    Verdict,
    /// The paragraph the verdict model drafted.
    Summary,
    /// The ranked findings.
    Findings,
    /// How many findings the comment budget cut.
    Omitted,
    /// Verdict-setting findings the comment budget cut.
    BeyondBudget,
    /// What the run covered, and every degradation it applied.
    Coverage,
    /// What the run spent, per pass and in total.
    Spend,
    /// Which model each pass actually used.
    Models,
}

impl Section {
    /// Every section, for error messages that name the alternatives.
    pub const ALL: &'static [Section] = &[
        Section::Verdict,
        Section::Summary,
        Section::Findings,
        Section::Omitted,
        Section::BeyondBudget,
        Section::Coverage,
        Section::Spend,
        Section::Models,
    ];

    /// Sections a review cannot be published without. Drawn from what the
    /// specifications require a review to admit, not from taste: findings,
    /// what the comment budget cut, what coverage was achieved, and what
    /// the run spent.
    pub const REQUIRED: &'static [Section] = &[
        Section::Findings,
        Section::Omitted,
        Section::BeyondBudget,
        Section::Coverage,
        Section::Spend,
    ];

    /// The name this section is configured by.
    pub fn name(self) -> &'static str {
        match self {
            Section::Verdict => "verdict",
            Section::Summary => "summary",
            Section::Findings => "findings",
            Section::Omitted => "omitted",
            Section::BeyondBudget => "beyond_budget",
            Section::Coverage => "coverage",
            Section::Spend => "spend",
            Section::Models => "models",
        }
    }
}

/// The default body: what demur publishes when a repository says nothing.
fn default_sections() -> Vec<Section> {
    vec![
        Section::Verdict,
        Section::Summary,
        Section::Findings,
        Section::Omitted,
        Section::BeyondBudget,
        Section::Coverage,
        Section::Spend,
    ]
}

/// The shape of the published review body.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Template {
    /// Text rendered before the sections, exactly as given.
    pub header: Option<String>,
    /// The sections, in the order they are published.
    pub sections: Vec<Section>,
    /// Text rendered after the sections, exactly as given.
    pub footer: Option<String>,
}

impl Default for Template {
    fn default() -> Self {
        Template {
            header: None,
            sections: default_sections(),
            footer: None,
        }
    }
}

impl Review {
    /// True when any rule is declared. Used to skip evaluation entirely.
    pub fn is_empty(&self) -> bool {
        !self.title.declared() && !self.description.declared()
    }
}

/// Rules for the pull request title.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct TitleRules {
    /// Require a non-empty title.
    pub required: bool,
    /// Minimum length in characters.
    pub min_length: Option<usize>,
    /// Maximum length in characters.
    pub max_length: Option<usize>,
    /// Regular expression the title must match. Anchored only where the
    /// pattern anchors itself.
    pub pattern: Option<String>,
    /// Severity carried by a violation of these rules.
    pub severity: Option<Severity>,
}

impl TitleRules {
    /// True when at least one rule is declared.
    pub fn declared(&self) -> bool {
        self.required
            || self.min_length.is_some()
            || self.max_length.is_some()
            || self.pattern.is_some()
    }
}

/// Rules for the pull request description.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct DescriptionRules {
    /// Require a non-empty description.
    pub required: bool,
    /// Minimum length in characters.
    pub min_length: Option<usize>,
    /// Maximum length in characters.
    pub max_length: Option<usize>,
    /// Regular expression the description must match.
    pub pattern: Option<String>,
    /// Headings that must appear, matched case-insensitively against
    /// trimmed lines.
    pub required_sections: Vec<String>,
    /// Severity carried by a violation of these rules.
    pub severity: Option<Severity>,
}

impl DescriptionRules {
    /// True when at least one rule is declared.
    pub fn declared(&self) -> bool {
        self.required
            || self.min_length.is_some()
            || self.max_length.is_some()
            || self.pattern.is_some()
            || !self.required_sections.is_empty()
    }
}

/// Severity a metadata violation carries when the rule does not name one.
/// Warning rather than blocker: declaring a rule should not silently start
/// blocking merges.
pub const DEFAULT_RULE_SEVERITY: Severity = Severity::Warning;

/// A named provider endpoint.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProviderDef {
    /// Provider family dialect.
    pub family: Family,
    /// API base URL. Required for the openai and anthropic families,
    /// unused by claude-code.
    #[serde(default)]
    pub base_url: String,
    /// Environment variable holding the API key. Required for the openai
    /// and anthropic families, unused by claude-code.
    #[serde(default)]
    pub key_env: String,
    /// File holding the API key, for local runs.
    pub key_file: Option<PathBuf>,
    /// Extra body fields forwarded to the provider unmodified.
    #[serde(default)]
    #[schemars(with = "Option<std::collections::BTreeMap<String, serde_json::Value>>")]
    pub extra_body: Option<toml::Table>,
    /// Extra headers forwarded to the provider unmodified.
    #[serde(default)]
    pub extra_headers: Option<BTreeMap<String, String>>,
}

/// The model assigned to a pipeline role.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ModelDef {
    /// Name of the provider in the providers table.
    pub provider: String,
    /// Model identifier sent to the provider.
    pub name: String,
    /// Input price in USD per million tokens.
    pub input_price: f64,
    /// Output price in USD per million tokens.
    pub output_price: f64,
    /// Cached input price in USD per million tokens. Defaults to half the
    /// input price when absent.
    pub cached_input_price: Option<f64>,
    /// OpenAI reasoning effort: minimal, low, medium, or high.
    pub reasoning_effort: Option<String>,
    /// Anthropic thinking budget in tokens.
    pub thinking_budget: Option<u32>,
    /// Extra body fields forwarded to the provider unmodified.
    #[serde(default)]
    #[schemars(with = "Option<std::collections::BTreeMap<String, serde_json::Value>>")]
    pub extra_body: Option<toml::Table>,
    /// Extra headers forwarded to the provider unmodified.
    #[serde(default)]
    pub extra_headers: Option<BTreeMap<String, String>>,
}

/// The model role assignments. Every role is required.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Models {
    /// Cheap model for the triage pass.
    pub triage: ModelDef,
    /// Deep model for deep dives and cross-examination.
    pub deep: ModelDef,
    /// Model for verdict synthesis.
    pub verdict: ModelDef,
}

impl Config {
    /// True when the role's provider drives the keyless agent transport,
    /// which reports no token metering of its own and runs on a local
    /// subscription rather than a resolved key.
    pub fn role_is_agent(&self, model: &ModelDef) -> bool {
        self.providers
            .get(&model.provider)
            .is_some_and(|provider| provider.family == Family::ClaudeCode)
    }

    /// True when any model role names the keyless agent family. Such a
    /// configuration must never run on an untrusted head: the subprocess
    /// holds its own credentials, so a missing key guards nothing.
    pub fn selects_agent_family(&self) -> bool {
        [&self.models.triage, &self.models.deep, &self.models.verdict]
            .iter()
            .any(|model| self.role_is_agent(model))
    }

    /// Load and validate the configuration file at `path`.
    pub fn load(path: &Path) -> Result<Config, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|err| match err.kind() {
            std::io::ErrorKind::NotFound => ConfigError::Missing {
                path: path.display().to_string(),
            },
            _ => ConfigError::Parse {
                file: path.display().to_string(),
                message: format!("cannot be read: {err}"),
            },
        })?;
        Config::from_toml(&text)
    }

    /// Parse and validate configuration from TOML text.
    pub fn from_toml(text: &str) -> Result<Config, ConfigError> {
        let config: Config = toml::from_str(text).map_err(|err| {
            let mut message = err.message().to_string();
            if let Some(location) = syntax_location(text, err.span()) {
                message = format!("{message} at {location}");
            }
            if err.message().starts_with("missing field") {
                message.push('\n');
                message.push_str(REQUIRED_FIELDS);
                message.push_str("\nminimal example:\n");
                message.push_str(MINIMAL_EXAMPLE.trim());
            }
            ConfigError::Parse {
                file: CONFIG_FILE_NAME.to_string(),
                message,
            }
        })?;
        validate(&config)?;
        Ok(config)
    }
}

/// Human-readable line and column for a TOML span, when the parser has one.
fn syntax_location(text: &str, span: Option<std::ops::Range<usize>>) -> Option<String> {
    let start = span?.start;
    let before = &text[..start.min(text.len())];
    let line = before.matches('\n').count() + 1;
    let column = before
        .rfind('\n')
        .map_or(before.len(), |i| before.len() - i - 1)
        + 1;
    Some(format!("line {line}, column {column}"))
}

const REASONING_EFFORTS: &str = "minimal, low, medium, high";
const SEVERITY_VALUES: &str = "blocker, warning, note";

/// Enforce cross-field rules the schema cannot express.
fn validate(config: &Config) -> Result<(), ConfigError> {
    match config.budget.cap() {
        BudgetCap::Limited(amount) if amount < 0.0 => {
            return Err(ConfigError::Invalid {
                field: "budget.per_pr_usd".to_string(),
                message: "must be zero or positive".to_string(),
            });
        }
        _ => {}
    }
    if config.budget.per_pr_usd.is_some() && config.budget.unlimited {
        return Err(ConfigError::Invalid {
            field: "budget".to_string(),
            message: "set per_pr_usd or unlimited = true, not both".to_string(),
        });
    }
    validate_template(&config.review.template)?;
    // Compile declared patterns here so an unusable rule fails the run
    // before anything is spent, rather than at evaluation time.
    crate::rules::Rules::compile(&config.review)?;
    // A place to keep an authorization is meaningless without an
    // application to authorize, so a half-configuration fails closed rather
    // than silently never badging anything.
    if config.app.token_file.is_some() && config.app.client_id.is_none() {
        return Err(ConfigError::Invalid {
            field: "app.token_file".to_string(),
            message: "names where to keep an authorization but no app.client_id says what to authorize; set both or neither"
                .to_string(),
        });
    }
    if config.retrieval.enabled && config.retrieval.max_rounds == 0 {
        return Err(ConfigError::Invalid {
            field: "retrieval.max_rounds".to_string(),
            message: "must be at least one when retrieval is enabled".to_string(),
        });
    }
    if config.retrieval.enabled && config.retrieval.max_kb == 0 {
        return Err(ConfigError::Invalid {
            field: "retrieval.max_kb".to_string(),
            message: "must be at least one kilobyte when retrieval is enabled".to_string(),
        });
    }
    if config.cache.enabled && config.cache.max_mb == 0 {
        return Err(ConfigError::Invalid {
            field: "cache.max_mb".to_string(),
            message: "must be at least one megabyte when the cache is enabled".to_string(),
        });
    }
    if config.block_on.severities.is_empty() {
        return Err(ConfigError::Invalid {
            field: "block_on.severities".to_string(),
            message: format!("must name at least one of: {SEVERITY_VALUES}"),
        });
    }
    for (name, provider) in &config.providers {
        if provider.family == Family::ClaudeCode {
            // The subprocess holds its own login: no URL to reach and no
            // key to name. HTTP-dialect knobs would be silently ignored,
            // so naming one fails instead.
            let set = |field: &str| match field {
                "base_url" => !provider.base_url.is_empty(),
                "key_env" => !provider.key_env.trim().is_empty(),
                "key_file" => provider.key_file.is_some(),
                "extra_body" => provider.extra_body.is_some(),
                _ => provider.extra_headers.is_some(),
            };
            for field in [
                "base_url",
                "key_env",
                "key_file",
                "extra_body",
                "extra_headers",
            ] {
                if set(field) {
                    return Err(ConfigError::Invalid {
                        field: format!("providers.{name}.{field}"),
                        message: "is unused by the claude-code family and must not be set"
                            .to_string(),
                    });
                }
            }
            continue;
        }
        if !provider.base_url.starts_with("https://") && !provider.base_url.starts_with("http://") {
            return Err(ConfigError::Invalid {
                field: format!("providers.{name}.base_url"),
                message: "must be an http or https URL".to_string(),
            });
        }
        if provider.key_env.trim().is_empty() {
            return Err(ConfigError::Invalid {
                field: format!("providers.{name}.key_env"),
                message: "must name the environment variable holding the key".to_string(),
            });
        }
    }
    let roles = [
        ("models.triage", &config.models.triage),
        ("models.deep", &config.models.deep),
        ("models.verdict", &config.models.verdict),
    ];
    for (role, model) in roles {
        let provider = config.providers.get(&model.provider);
        let family = match provider {
            Some(provider) => provider.family,
            None => {
                let defined = config
                    .providers
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(ConfigError::Invalid {
                    field: format!("{role}.provider"),
                    message: format!(
                        "names unknown provider `{}`, defined providers: {defined}",
                        model.provider
                    ),
                });
            }
        };
        if model.input_price < 0.0 {
            return Err(ConfigError::Invalid {
                field: format!("{role}.input_price"),
                message: "must be zero or positive".to_string(),
            });
        }
        if model.output_price < 0.0 {
            return Err(ConfigError::Invalid {
                field: format!("{role}.output_price"),
                message: "must be zero or positive".to_string(),
            });
        }
        if config.limits.concurrency == 0 {
            return Err(ConfigError::Invalid {
                field: "limits.concurrency".to_string(),
                message: "must be at least one; use 1 to run passes serially".to_string(),
            });
        }
        if config.limits.max_tokens == 0 {
            return Err(ConfigError::Invalid {
                field: "limits.max_tokens".to_string(),
                message: "must be at least one token".to_string(),
            });
        }
        if model.cached_input_price.is_some_and(|price| price < 0.0) {
            return Err(ConfigError::Invalid {
                field: format!("{role}.cached_input_price"),
                message: "must be zero or positive".to_string(),
            });
        }
        if let Some(effort) = &model.reasoning_effort {
            if family != Family::OpenAi {
                return Err(ConfigError::Invalid {
                    field: format!("{role}.reasoning_effort"),
                    message:
                        "applies to openai-compatible providers; for anthropic use thinking_budget"
                            .to_string(),
                });
            }
            let allowed = ["minimal", "low", "medium", "high"];
            if !allowed.contains(&effort.as_str()) {
                return Err(ConfigError::Invalid {
                    field: format!("{role}.reasoning_effort"),
                    message: format!(
                        "`{effort}` is not valid, allowed values: {REASONING_EFFORTS}"
                    ),
                });
            }
        }
        if let Some(budget) = model.thinking_budget {
            if family != Family::Anthropic {
                return Err(ConfigError::Invalid {
                    field: format!("{role}.thinking_budget"),
                    message: "applies to the anthropic family; for openai-compatible providers use reasoning_effort".to_string(),
                });
            }
            if budget == 0 {
                return Err(ConfigError::Invalid {
                    field: format!("{role}.thinking_budget"),
                    message: "must be at least one token".to_string(),
                });
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal() -> String {
        MINIMAL_EXAMPLE.trim().to_string()
    }

    #[test]
    fn minimal_config_parses_with_defaults() {
        let config = Config::from_toml(&minimal()).unwrap();
        assert_eq!(config.profile, None);
        assert_eq!(config.block_on.severities, vec![Severity::Blocker]);
        assert_eq!(config.budget.cap(), BudgetCap::Limited(DEFAULT_BUDGET_USD));
        assert_eq!(config.limits.deep_calls, DEFAULT_DEEP_CALLS);
        assert_eq!(config.limits.comments, DEFAULT_COMMENTS);
        assert_eq!(config.limits.max_tokens, DEFAULT_MAX_TOKENS);
        assert_eq!(config.limits.concurrency, DEFAULT_CONCURRENCY);
        assert!(config.lenses.correctness);
        assert!(config.lenses.security);
        assert!(config.lenses.performance);
        assert!(!config.lenses.style);
        assert!(config.ignore.paths.is_empty());
    }

    #[test]
    fn all_sections_parse() {
        let text = format!(
            r#"
profile = "deep"

[lenses]
performance = false
style = true

[block_on]
severities = ["warning", "blocker"]

[ignore]
paths = ["**/Cargo.lock", "dist/**"]

[budget]
per_pr_usd = 2.50

[limits]
deep_calls = 4
comments = 5

{}
"#,
            minimal()
        );
        let config = Config::from_toml(&text).unwrap();
        assert_eq!(config.profile, Some(Profile::Deep));
        assert!(!config.lenses.performance);
        assert!(config.lenses.style);
        assert_eq!(
            config.block_on.severities,
            vec![Severity::Warning, Severity::Blocker]
        );
        assert_eq!(config.ignore.paths.len(), 2);
        assert_eq!(config.budget.cap(), BudgetCap::Limited(2.5));
        assert_eq!(config.limits.deep_calls, 4);
        assert_eq!(config.limits.comments, 5);
    }

    #[test]
    fn explicit_unlimited_budget() {
        let text = format!("[budget]\nunlimited = true\n\n{}", minimal());
        let config = Config::from_toml(&text).unwrap();
        assert_eq!(config.budget.cap(), BudgetCap::Unlimited);
    }

    #[test]
    fn missing_file_reports_required_fields_and_example() {
        let path = std::env::temp_dir().join("demur-no-such-config.toml");
        let err = Config::load(&path).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("required settings"));
        assert!(text.contains("[providers.<name>]"));
        assert!(text.contains("[models.verdict]"));
    }

    #[test]
    fn missing_providers_table_lists_fields_and_example() {
        let text = r#"
[models.triage]
provider = "openai"
name = "gpt-4o-mini"
input_price = 0.15
output_price = 0.60

[models.deep]
provider = "openai"
name = "gpt-4o"
input_price = 2.50
output_price = 10.00

[models.verdict]
provider = "openai"
name = "gpt-4o-mini"
input_price = 0.15
output_price = 0.60
"#;
        let text = err_text(text);
        assert!(text.contains("missing field `providers`"));
        assert!(text.contains("minimal example"));
        assert!(text.contains("key_env"));
    }

    #[test]
    fn missing_models_table_lists_fields_and_example() {
        let text = r#"
[providers.openai]
family = "openai"
base_url = "https://api.openai.com/v1"
key_env = "OPENAI_API_KEY"
"#;
        let text = err_text(text);
        assert!(text.contains("missing field `models`"));
        assert!(text.contains("minimal example"));
    }

    #[test]
    fn missing_role_names_the_role() {
        let text = r#"
[providers.openai]
family = "openai"
base_url = "https://api.openai.com/v1"
key_env = "OPENAI_API_KEY"

[models.triage]
provider = "openai"
name = "gpt-4o-mini"
input_price = 0.15
output_price = 0.60

[models.deep]
provider = "openai"
name = "gpt-4o"
input_price = 2.50
output_price = 10.00
"#;
        let text = err_text(text);
        assert!(text.contains("missing field `verdict`"));
        assert!(text.contains("minimal example"));
    }

    #[test]
    fn missing_model_price_names_the_field() {
        let text = minimal().replace("input_price = 2.50\n", "");
        let text = err_text(&text);
        assert!(text.contains("missing field `input_price`"));
    }

    #[test]
    fn missing_provider_key_env_names_the_field() {
        let text = minimal().replace("key_env = \"OPENAI_API_KEY\"\n", "");
        let text = err_text(&text);
        assert!(text.contains("providers.openai.key_env"));
    }

    #[test]
    fn malformed_toml_reports_syntax_location() {
        let text = format!("{}\nprofile = quick\n", minimal());
        let text = err_text(&text);
        assert!(text.contains(".demur.toml"));
        assert!(text.contains("line "));
    }

    #[test]
    fn unknown_severity_names_field_and_allowed_values() {
        let text = minimal().replace(
            "[models.triage]",
            "[block_on]\nseverities = [\"bloker\"]\n\n[models.triage]",
        );
        let text = err_text(&text);
        assert!(text.contains("bloker"));
        assert!(text.contains("expected one of"));
    }

    #[test]
    fn misspelled_budget_key_fails_naming_the_key() {
        let text = minimal().replace(
            "[models.triage]",
            "[budgett]\nper_pr_usd = 1.0\n\n[models.triage]",
        );
        let text = err_text(&text);
        assert!(text.contains("budgett"));
    }

    #[test]
    fn review_rules_parse_in_full() {
        let text = minimal().replace(
            "[models.triage]",
            r###"[review.title]
required = true
min_length = 10
max_length = 68
pattern = '^(feat|fix): .+'
severity = "warning"

[review.description]
required = true
min_length = 40
required_sections = ["## Why", "## Testing"]
severity = "blocker"

[models.triage]"###,
        );
        let config = Config::from_toml(&text).expect("review rules parse");
        assert!(config.review.title.required);
        assert_eq!(config.review.title.max_length, Some(68));
        assert_eq!(config.review.title.severity, Some(Severity::Warning));
        assert_eq!(
            config.review.description.required_sections,
            vec!["## Why".to_string(), "## Testing".to_string()]
        );
        assert_eq!(config.review.description.severity, Some(Severity::Blocker));
    }

    #[test]
    fn absent_review_section_declares_nothing() {
        let config = Config::from_toml(&minimal()).expect("minimal config parses");
        assert!(config.review.is_empty());
    }

    #[test]
    fn partial_review_section_leaves_the_rest_undeclared() {
        let text = minimal().replace(
            "[models.triage]",
            "[review.title]\nmax_length = 68\n\n[models.triage]",
        );
        let config = Config::from_toml(&text).expect("partial rules parse");
        assert!(config.review.title.declared());
        assert!(!config.review.description.declared());
        assert!(!config.review.title.required);
    }

    #[test]
    fn misspelled_review_key_is_rejected() {
        let text = minimal().replace(
            "[models.triage]",
            "[review.title]\nmax_lenght = 68\n\n[models.triage]",
        );
        let text = err_text(&text);
        assert!(text.contains("max_lenght"), "{text}");
    }

    #[test]
    fn required_sections_on_the_title_is_rejected() {
        // Sections are a description rule. Accepting them on the title
        // would silently declare a rule that never fires.
        let text = minimal().replace(
            "[models.triage]",
            "[review.title]\nrequired_sections = [\"## Why\"]\n\n[models.triage]",
        );
        let text = err_text(&text);
        assert!(text.contains("required_sections"), "{text}");
    }

    #[test]
    fn invalid_review_pattern_fails_validation() {
        let text = minimal().replace(
            "[models.triage]",
            "[review.title]\npattern = '(unclosed'\n\n[models.triage]",
        );
        let text = err_text(&text);
        assert!(text.contains("review.title.pattern"), "{text}");
    }

    fn with_template(body: &str) -> String {
        minimal().replace("[models.triage]", &format!("{body}\n\n[models.triage]"))
    }

    #[test]
    fn the_default_template_is_todays_body() {
        let config = Config::from_toml(&minimal()).expect("minimal config parses");
        let names: Vec<&str> = config
            .review
            .template
            .sections
            .iter()
            .map(|section| section.name())
            .collect();
        assert_eq!(
            names,
            vec![
                "verdict",
                "summary",
                "findings",
                "omitted",
                "beyond_budget",
                "coverage",
                "spend"
            ]
        );
        assert!(config.review.template.header.is_none());
    }

    #[test]
    fn sections_may_be_reordered_with_prose_around_them() {
        let text = with_template(
            "[review.template]\nheader = \"top\"\nfooter = \"bottom\"\nsections = [\"findings\", \"spend\", \"coverage\", \"omitted\", \"beyond_budget\", \"models\"]",
        );
        let config = Config::from_toml(&text).expect("a reordered template is valid");
        assert_eq!(config.review.template.header.as_deref(), Some("top"));
        assert_eq!(config.review.template.footer.as_deref(), Some("bottom"));
        assert_eq!(config.review.template.sections[0].name(), "findings");
    }

    #[test]
    fn an_unknown_section_is_rejected() {
        let text = with_template("[review.template]\nsections = [\"findings\", \"epilogue\"]");
        let text = err_text(&text);
        assert!(text.contains("epilogue"), "{text}");
    }

    #[test]
    fn a_repeated_section_is_rejected() {
        let text = with_template(
            "[review.template]\nsections = [\"findings\", \"omitted\", \"beyond_budget\", \"coverage\", \"spend\", \"spend\"]",
        );
        let text = err_text(&text);
        assert!(text.contains("`spend` more than once"), "{text}");
    }

    #[test]
    fn an_empty_section_list_is_rejected() {
        let text = with_template("[review.template]\nsections = []");
        let text = err_text(&text);
        assert!(text.contains("no sections"), "{text}");
    }

    #[test]
    fn every_required_section_is_individually_required() {
        // Checked one at a time so adding a required section later cannot
        // be forgotten here.
        let all = ["findings", "omitted", "beyond_budget", "coverage", "spend"];
        for omitted in all {
            let kept: Vec<String> = all
                .iter()
                .filter(|name| **name != omitted)
                .map(|name| format!("\"{name}\""))
                .collect();
            let text = with_template(&format!(
                "[review.template]\nsections = [{}]",
                kept.join(", ")
            ));
            let text = err_text(&text);
            assert!(
                text.contains(&format!("`{omitted}`")),
                "omitting {omitted} must be refused, got: {text}"
            );
        }
    }

    #[test]
    fn optional_sections_may_be_left_out() {
        let text = with_template(
            "[review.template]\nsections = [\"findings\", \"omitted\", \"beyond_budget\", \"coverage\", \"spend\"]",
        );
        let config = Config::from_toml(&text).expect("verdict and summary are not required");
        assert_eq!(config.review.template.sections.len(), 5);
    }

    #[test]
    fn no_application_is_configured_by_default() {
        let config = Config::from_toml(&minimal()).expect("minimal config parses");
        assert!(!config.app.is_configured());
        assert!(config.app.token_file.is_none());
    }

    #[test]
    fn a_half_configured_application_fails_closed() {
        let text = minimal().replace(
            "[models.triage]",
            "[app]\ntoken_file = \"/tmp/demur-token\"\n\n[models.triage]",
        );
        let text = err_text(&text);
        assert!(text.contains("app.token_file"), "{text}");
        assert!(text.contains("app.client_id"), "{text}");
    }

    #[test]
    fn an_application_without_a_token_file_is_valid() {
        // Authorizing every time is a supported choice.
        let text = minimal().replace(
            "[models.triage]",
            "[app]\nclient_id = \"Iv1.abc123\"\n\n[models.triage]",
        );
        let config = Config::from_toml(&text).expect("client_id alone is valid");
        assert!(config.app.is_configured());
        assert!(config.app.token_file.is_none());
    }

    #[test]
    fn retrieval_is_off_unless_asked_for() {
        let config = Config::from_toml(&minimal()).expect("minimal config parses");
        assert!(!config.retrieval.enabled);
    }

    #[test]
    fn an_untrusted_head_turns_retrieval_off() {
        let mut retrieval = Retrieval {
            enabled: true,
            ..Default::default()
        };
        assert!(retrieval.disable_for_untrusted_head());
        assert!(!retrieval.enabled);
        assert!(!retrieval.disable_for_untrusted_head());
    }

    #[test]
    fn enabled_retrieval_needs_usable_bounds() {
        for (key, field) in [
            ("max_rounds", "retrieval.max_rounds"),
            ("max_kb", "retrieval.max_kb"),
        ] {
            let text = minimal().replace(
                "[models.triage]",
                &format!("[retrieval]\nenabled = true\n{key} = 0\n\n[models.triage]"),
            );
            let text = err_text(&text);
            assert!(text.contains(field), "{text}");
        }
    }

    #[test]
    fn an_untrusted_head_turns_the_cache_off() {
        let mut cache = Cache {
            enabled: true,
            ..Default::default()
        };
        assert!(cache.disable_for_untrusted_head());
        assert!(!cache.enabled);
        // Idempotent, and it reports that nothing was turned off.
        assert!(!cache.disable_for_untrusted_head());
    }

    #[test]
    fn the_cache_is_off_unless_asked_for() {
        let config = Config::from_toml(&minimal()).expect("minimal config parses");
        assert!(!config.cache.enabled);
        assert!(config.cache.dir.is_none());
    }

    #[test]
    fn an_enabled_cache_needs_a_usable_size_bound() {
        let text = minimal().replace(
            "[models.triage]",
            "[cache]\nenabled = true\nmax_mb = 0\n\n[models.triage]",
        );
        let text = err_text(&text);
        assert!(text.contains("cache.max_mb"), "{text}");
    }

    #[test]
    fn zero_concurrency_fails() {
        let text = minimal().replace(
            "[models.triage]",
            "[limits]\nconcurrency = 0\n\n[models.triage]",
        );
        let text = err_text(&text);
        assert!(text.contains("limits.concurrency"), "{text}");
    }

    #[test]
    fn zero_max_tokens_fails() {
        let text = minimal().replace(
            "[models.triage]",
            "[limits]\nmax_tokens = 0\n\n[models.triage]",
        );
        let text = err_text(&text);
        assert!(text.contains("limits.max_tokens"));
    }

    #[test]
    fn empty_block_on_fails() {
        let text = minimal().replace(
            "[models.triage]",
            "[block_on]\nseverities = []\n\n[models.triage]",
        );
        let text = err_text(&text);
        assert!(text.contains("block_on.severities"));
        assert!(text.contains(SEVERITY_VALUES));
    }

    #[test]
    fn both_budget_settings_fail() {
        let text = minimal().replace(
            "[models.triage]",
            "[budget]\nper_pr_usd = 1.0\nunlimited = true\n\n[models.triage]",
        );
        let text = err_text(&text);
        assert!(text.contains("budget"));
        assert!(text.contains("not both"));
    }

    #[test]
    fn negative_price_fails_naming_the_field() {
        let text = minimal().replace("input_price = 2.50", "input_price = -1.0");
        let text = err_text(&text);
        assert!(text.contains("models.deep.input_price"));
    }

    #[test]
    fn unknown_provider_reference_fails() {
        let text = minimal().replace(
            "provider = \"openai\"\nname = \"gpt-4o\"\n",
            "provider = \"staging\"\nname = \"gpt-4o\"\n",
        );
        let text = err_text(&text);
        assert!(text.contains("models.deep.provider"));
        assert!(text.contains("staging"));
    }

    #[test]
    fn bad_base_url_fails_naming_the_field() {
        let text = minimal().replace(
            "base_url = \"https://api.openai.com/v1\"",
            "base_url = \"api.openai.com/v1\"",
        );
        let text = err_text(&text);
        assert!(text.contains("providers.openai.base_url"));
    }

    fn deep_anthropic(extra: &str) -> String {
        format!(
            r#"
[providers.openai]
family = "openai"
base_url = "https://api.openai.com/v1"
key_env = "OPENAI_API_KEY"

[providers.anthropic]
family = "anthropic"
base_url = "https://api.anthropic.com"
key_env = "ANTHROPIC_API_KEY"

[models.triage]
provider = "openai"
name = "gpt-4o-mini"
input_price = 0.15
output_price = 0.60

[models.deep]
provider = "anthropic"
name = "claude-sonnet-4-5"
input_price = 3.00
output_price = 15.00
{extra}

[models.verdict]
provider = "openai"
name = "gpt-4o-mini"
input_price = 0.15
output_price = 0.60
"#
        )
    }

    #[test]
    fn reasoning_effort_on_anthropic_fails() {
        let text = err_text(&deep_anthropic("reasoning_effort = \"high\""));
        assert!(text.contains("models.deep.reasoning_effort"));
        assert!(text.contains("thinking_budget"));
    }

    #[test]
    fn thinking_budget_on_openai_fails() {
        let text = minimal().replace(
            "provider = \"openai\"\nname = \"gpt-4o\"\ninput_price = 2.50\noutput_price = 10.00",
            "provider = \"openai\"\nname = \"gpt-4o\"\ninput_price = 2.50\noutput_price = 10.00\nthinking_budget = 4000",
        );
        let text = err_text(&text);
        assert!(text.contains("models.deep.thinking_budget"));
        assert!(text.contains("reasoning_effort"));
    }

    #[test]
    fn invalid_reasoning_effort_value_fails_with_allowed_values() {
        let text = minimal().replace(
            "name = \"gpt-4o\"",
            "name = \"gpt-4o\"\nreasoning_effort = \"maximum\"",
        );
        let text = err_text(&text);
        assert!(text.contains("models.deep.reasoning_effort"));
        assert!(text.contains(REASONING_EFFORTS));
    }

    #[test]
    fn valid_effort_and_budget_translate() {
        let config = Config::from_toml(&deep_anthropic("thinking_budget = 8000")).unwrap();
        assert_eq!(config.models.deep.thinking_budget, Some(8000));
        assert_eq!(config.models.deep.provider, "anthropic");

        let openai_effort = minimal().replace(
            "name = \"gpt-4o\"",
            "name = \"gpt-4o\"\nreasoning_effort = \"high\"",
        );
        let config = Config::from_toml(&openai_effort).unwrap();
        assert_eq!(config.models.deep.reasoning_effort, Some("high".into()));
    }

    /// Format a config error for assertions.
    fn err_text(text: &str) -> String {
        Config::from_toml(text)
            .expect_err("config should not parse")
            .to_string()
    }

    fn subscription_only() -> String {
        r#"
[providers.claude]
family = "claude-code"

[models.triage]
provider = "claude"
name = "claude-sonnet-4-5"
input_price = 3.00
output_price = 15.00

[models.deep]
provider = "claude"
name = "claude-sonnet-4-5"
input_price = 3.00
output_price = 15.00

[models.verdict]
provider = "claude"
name = "claude-sonnet-4-5"
input_price = 3.00
output_price = 15.00
"#
        .to_string()
    }

    #[test]
    fn claude_code_provider_needs_no_url_or_key() {
        let config = Config::from_toml(&subscription_only()).unwrap();
        assert_eq!(config.providers["claude"].family, Family::ClaudeCode);
        assert_eq!(config.models.triage.provider, "claude");
    }

    #[test]
    fn claude_code_rejects_reasoning_knobs() {
        let text = subscription_only().replace(
            "name = \"claude-sonnet-4-5\"\ninput_price = 3.00\noutput_price = 15.00\n\n[models.verdict]",
            "name = \"claude-sonnet-4-5\"\ninput_price = 3.00\noutput_price = 15.00\nthinking_budget = 8000\n\n[models.verdict]",
        );
        let text = err_text(&text);
        assert!(text.contains("models.deep.thinking_budget"));

        let text = subscription_only().replace(
            "name = \"claude-sonnet-4-5\"\ninput_price = 3.00\noutput_price = 15.00\n\n[models.verdict]",
            "name = \"claude-sonnet-4-5\"\ninput_price = 3.00\noutput_price = 15.00\nreasoning_effort = \"high\"\n\n[models.verdict]",
        );
        let text = err_text(&text);
        assert!(text.contains("models.deep.reasoning_effort"));
    }

    #[test]
    fn http_families_still_require_url_and_key() {
        let text = subscription_only().replace("family = \"claude-code\"", "family = \"openai\"");
        let text = err_text(&text);
        assert!(text.contains("providers.claude.base_url"));
    }

    #[test]
    fn claude_code_provider_rejects_http_dialect_knobs() {
        for (field, line) in [
            ("key_file", "key_file = \"/tmp/nope\""),
            ("extra_body", "extra_body = { temperature = 1 }"),
            (
                "extra_headers",
                "[providers.claude.extra_headers]\nX-Debug = \"1\"",
            ),
            ("base_url", "base_url = \"https://api.test\""),
            ("key_env", "key_env = \"CLAUDE_KEY\""),
        ] {
            let text = subscription_only().replace(
                "family = \"claude-code\"",
                &format!("family = \"claude-code\"\n{line}"),
            );
            let text = err_text(&text);
            assert!(
                text.contains(&format!("providers.claude.{field}")),
                "{field}: {text}"
            );
        }
    }
}

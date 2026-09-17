//! Repository configuration: the `.demur.toml` schema, defaults, loading,
//! and validation.

use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;

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
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Family {
    /// Any OpenAI-compatible endpoint with a configurable base URL.
    OpenAi,
    /// The native Anthropic API.
    Anthropic,
}

/// Review depth profiles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, Deserialize)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, Deserialize)]
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
}

/// The parsed `.demur.toml` configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    /// Named provider endpoints.
    pub providers: BTreeMap<String, ProviderDef>,
    /// Model assigned to each pipeline role.
    pub models: Models,
}

/// Deep dive lens toggles.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ignore {
    /// Glob patterns matched against repository-relative paths.
    #[serde(default)]
    pub paths: Vec<String>,
}

/// The per-pull-request spending cap.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Limits {
    /// Maximum deep dive calls per run.
    pub deep_calls: u32,
    /// Maximum findings published in one review.
    pub comments: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            deep_calls: DEFAULT_DEEP_CALLS,
            comments: DEFAULT_COMMENTS,
        }
    }
}

/// A named provider endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderDef {
    /// Provider family dialect.
    pub family: Family,
    /// API base URL.
    pub base_url: String,
    /// Environment variable holding the API key.
    pub key_env: String,
    /// File holding the API key, for local runs.
    pub key_file: Option<PathBuf>,
    /// Extra body fields forwarded to the provider unmodified.
    #[serde(default)]
    pub extra_body: Option<toml::Table>,
    /// Extra headers forwarded to the provider unmodified.
    #[serde(default)]
    pub extra_headers: Option<BTreeMap<String, String>>,
}

/// The model assigned to a pipeline role.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    pub extra_body: Option<toml::Table>,
    /// Extra headers forwarded to the provider unmodified.
    #[serde(default)]
    pub extra_headers: Option<BTreeMap<String, String>>,
}

/// The model role assignments. Every role is required.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    if config.block_on.severities.is_empty() {
        return Err(ConfigError::Invalid {
            field: "block_on.severities".to_string(),
            message: format!("must name at least one of: {SEVERITY_VALUES}"),
        });
    }
    for (name, provider) in &config.providers {
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
        assert!(text.contains("missing field `key_env`"));
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
}

//! Repository configuration: the `.demur.toml` schema, defaults, loading,
//! and validation.

use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;

use serde::Deserialize;
use thiserror::Error;

/// Name of the configuration file at the repository root.
pub const CONFIG_FILE_NAME: &str = ".demur.toml";

/// Built-in per-pull-request budget cap in USD when none is configured.
pub const DEFAULT_BUDGET_USD: f64 = 5.0;

/// Built-in maximum number of deep dive calls per run.
pub const DEFAULT_DEEP_CALLS: u32 = 12;

/// Built-in maximum number of published findings per review.
pub const DEFAULT_COMMENTS: u32 = 10;

const MINIMAL_EXAMPLE: &str = r#"
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Family {
    /// Any OpenAI-compatible endpoint with a configurable base URL.
    OpenAi,
    /// The native Anthropic API.
    Anthropic,
}

/// Review depth profiles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Profile {
    /// Triage and verdict only.
    Quick,
    /// Triage, deep dives, verdict.
    Standard,
    /// All passes including cross-examination.
    Deep,
}

/// Finding severities.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
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
#[derive(Debug, Clone, Deserialize)]
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
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
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
#[derive(Debug, Clone, Deserialize)]
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
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Ignore {
    /// Glob patterns matched against repository-relative paths.
    #[serde(default)]
    pub paths: Vec<String>,
}

/// The per-pull-request spending cap.
#[derive(Debug, Clone, Default, Deserialize)]
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
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
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
#[derive(Debug, Clone, Deserialize)]
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
#[derive(Debug, Clone, Deserialize)]
pub struct ModelDef {
    /// Name of the provider in the providers table.
    pub provider: String,
    /// Model identifier sent to the provider.
    pub name: String,
    /// Input price in USD per million tokens.
    pub input_price: f64,
    /// Output price in USD per million tokens.
    pub output_price: f64,
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
#[derive(Debug, Clone, Deserialize)]
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
            if message.starts_with("missing field") {
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
        Ok(config)
    }
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

    /// Format a config error for assertions.
    fn err_text(text: &str) -> String {
        Config::from_toml(text)
            .expect_err("config should not parse")
            .to_string()
    }
}

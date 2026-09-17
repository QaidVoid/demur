//! Documentation freshness gate: every configuration key in the schema
//! must appear in the configuration reference page, so a schema change
//! without a doc update fails verification.

use crate::config::Config;
use serde_json::Value;

fn full_config_key_paths() -> Vec<String> {
    let text = r#"
profile = "deep"

[lenses]
correctness = true
security = true
performance = true
style = true

[block_on]
severities = ["blocker"]

[ignore]
paths = ["a"]

[budget]
per_pr_usd = 1.0

[limits]
deep_calls = 1
comments = 1

[providers.test]
family = "openai"
base_url = "https://api.test/v1"
key_env = "KEY"
key_file = "/tmp/key"

[providers.test.extra_body]
custom = 1

[providers.test.extra_headers]
"X-Custom" = "value"

[models.triage]
provider = "test"
name = "t"
input_price = 0.15
output_price = 0.60
cached_input_price = 0.07
reasoning_effort = "high"

[models.triage.extra_body]
custom = 1

[models.triage.extra_headers]
"X-Custom" = "value"

[providers.ant]
family = "anthropic"
base_url = "https://api.anthropic.test"
key_env = "ANTHROPIC_KEY"

[models.deep]
provider = "ant"
name = "d"
input_price = 3.00
output_price = 15.00
thinking_budget = 8000

[models.verdict]
provider = "test"
name = "v"
input_price = 0.15
output_price = 0.60
"#;
    let config = Config::from_toml(text.trim()).unwrap();
    let value = serde_json::to_value(&config).unwrap();
    let mut paths = Vec::new();
    walk(&value, String::new(), &mut paths);
    paths
}

fn walk(value: &Value, prefix: String, out: &mut Vec<String>) {
    if let Value::Object(map) = value {
        for (key, child) in map {
            let path = if prefix.is_empty() {
                key.clone()
            } else {
                format!("{prefix}.{key}")
            };
            out.push(path.clone());
            walk(child, path, out);
        }
    }
}

#[test]
fn configuration_reference_documents_every_schema_key() {
    let manifest = env!("CARGO_MANIFEST_DIR");
    let doc = std::fs::read_to_string(format!("{manifest}/../../docs/reference/configuration.md"))
        .expect("configuration reference page exists");
    let key_paths = full_config_key_paths();
    let missing: Vec<&String> = key_paths
        .iter()
        // Provider names are user defined, so the path directly under the
        // providers table is a wildcard, not a schema key.
        .filter(|path| path.split('.').count() != 2 || !path.starts_with("providers."))
        .filter(|path| {
            let leaf = path.rsplit('.').next().unwrap_or(path);
            !doc.contains(leaf)
        })
        .collect();
    assert!(
        missing.is_empty(),
        "configuration keys missing from docs/reference/configuration.md: {missing:?}"
    );
}

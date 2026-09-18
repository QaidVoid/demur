//! Keeps the published configuration schema from drifting away from the
//! types it describes. A schema that disagrees with the validator is worse
//! than none: it tells a user their file is fine right up until the run
//! fails.

#[cfg(test)]
mod tests {
    /// Path of the published schema, relative to the repository root.
    const PUBLISHED: &str = "docs/public/demur.schema.json";

    #[test]
    fn published_schema_matches_the_configuration_types() {
        let manifest = env!("CARGO_MANIFEST_DIR");
        let path = format!("{manifest}/../../{PUBLISHED}");
        let published =
            std::fs::read_to_string(&path).unwrap_or_else(|err| panic!("read {PUBLISHED}: {err}"));
        let generated = crate::config::json_schema_text();
        assert_eq!(
            published, generated,
            "{PUBLISHED} no longer matches the configuration types. \
Regenerate it with `cargo run -p demur -- schema > {PUBLISHED}`."
        );
    }

    #[test]
    fn schema_enumerates_the_severity_taxonomy() {
        let schema = crate::config::json_schema();
        let severities = schema["$defs"]["Severity"]["oneOf"]
            .as_array()
            .map(|variants| {
                variants
                    .iter()
                    .filter_map(|variant| variant["const"].as_str().map(str::to_string))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        for expected in ["note", "warning", "blocker"] {
            assert!(
                severities.iter().any(|value| value == expected),
                "severity `{expected}` missing from the schema: {severities:?}"
            );
        }
    }

    #[test]
    fn schema_covers_the_review_rules() {
        let schema = crate::config::json_schema();
        let review = &schema["$defs"]["Review"]["properties"];
        assert!(review["title"].is_object(), "review.title missing");
        assert!(
            review["description"].is_object(),
            "review.description missing"
        );
        let title = &schema["$defs"]["TitleRules"]["properties"];
        for key in [
            "required",
            "min_length",
            "max_length",
            "pattern",
            "severity",
        ] {
            assert!(title[key].is_object(), "review.title.{key} missing");
        }
        let description = &schema["$defs"]["DescriptionRules"]["properties"];
        assert!(
            description["required_sections"].is_object(),
            "review.description.required_sections missing"
        );
    }

    #[test]
    fn schema_rejects_what_the_bot_rejects() {
        // The bot denies unknown keys everywhere, so the schema must too.
        // A schema that accepted them would validate a file the bot fails.
        let schema = crate::config::json_schema();
        assert_eq!(schema["additionalProperties"], serde_json::json!(false));
        for name in [
            "Review",
            "TitleRules",
            "DescriptionRules",
            "Limits",
            "Budget",
        ] {
            assert_eq!(
                schema["$defs"][name]["additionalProperties"],
                serde_json::json!(false),
                "{name} must reject unknown keys"
            );
        }
    }
}

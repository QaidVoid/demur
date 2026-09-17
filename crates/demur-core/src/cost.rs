//! Token estimation and cost computation from configured model prices.

use crate::config::ModelDef;
use crate::provider::TokenUsage;

/// Prices for one model in USD per million tokens.
#[derive(Debug, Clone, Copy)]
pub struct ModelPrice {
    /// Full input price.
    pub input: f64,
    /// Cached input price. Defaults to half the input price when
    /// configuration does not name one.
    pub cached_input: f64,
    /// Output price.
    pub output: f64,
}

impl ModelPrice {
    /// Build prices from a configured model.
    pub fn from_model(model: &ModelDef) -> ModelPrice {
        ModelPrice {
            input: model.input_price,
            cached_input: model.cached_input_price.unwrap_or(model.input_price / 2.0),
            output: model.output_price,
        }
    }

    /// Cost of actual reported usage in USD.
    pub fn cost_of_usage(&self, usage: &TokenUsage) -> f64 {
        usage.cost_usd(self.input, self.output, self.cached_input)
    }

    /// Cost of an estimated input token count in USD, used for pre-pass
    /// budget checks.
    pub fn cost_of_estimated_input(&self, estimated_input_tokens: u64) -> f64 {
        estimated_input_tokens as f64 / 1_000_000.0 * self.input
    }
}

/// Estimate the token count of a text. One token is taken as four
/// characters, the usual approximation for English and code. Estimates are
/// coarse by design: they gate budget decisions, never billing, which uses
/// provider-reported usage only.
pub fn estimate_tokens(text: &str) -> u64 {
    (text.chars().count() as u64).div_ceil(4)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ModelDef;

    fn model(input: f64, output: f64) -> ModelDef {
        ModelDef {
            provider: "p".to_string(),
            name: "m".to_string(),
            input_price: input,
            output_price: output,
            reasoning_effort: None,
            thinking_budget: None,
            extra_body: None,
            extra_headers: None,
            cached_input_price: None,
        }
    }

    #[test]
    fn estimation_is_within_documented_tolerance() {
        // 400 ASCII characters are about 100 tokens; the approximation must
        // land within 25 percent of that.
        let text = "word ".repeat(80);
        assert_eq!(text.chars().count(), 400);
        let estimate = estimate_tokens(&text) as f64;
        assert!((estimate - 100.0).abs() / 100.0 <= 0.25);
    }

    #[test]
    fn estimation_rounds_partial_tokens_up() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("a"), 1);
        assert_eq!(estimate_tokens("abcd"), 1);
        assert_eq!(estimate_tokens("abcde"), 2);
    }

    #[test]
    fn cached_input_defaults_to_half_price() {
        let price = ModelPrice::from_model(&model(2.0, 10.0));
        assert_eq!(price.cached_input, 1.0);
    }

    #[test]
    fn mixed_price_roles_produce_per_pass_costs() {
        let triage = ModelPrice::from_model(&model(0.15, 0.60));
        let deep = ModelPrice::from_model(&model(3.00, 15.00));
        let usage = TokenUsage {
            input_tokens: 100_000,
            cached_input_tokens: 0,
            output_tokens: 10_000,
        };
        let triage_cost = triage.cost_of_usage(&usage);
        let deep_cost = deep.cost_of_usage(&usage);
        assert!((triage_cost - 0.021).abs() < 1e-9);
        assert!((deep_cost - 0.45).abs() < 1e-9);
        assert!(deep_cost > triage_cost);
    }

    #[test]
    fn cached_usage_is_priced_at_the_cached_rate() {
        let price = ModelPrice::from_model(&model(2.0, 10.0));
        let usage = TokenUsage {
            input_tokens: 100_000,
            cached_input_tokens: 100_000,
            output_tokens: 0,
        };
        let cost = price.cost_of_usage(&usage);
        // 100k uncached at 2.0 plus 100k cached at 1.0 per million.
        assert!((cost - 0.3).abs() < 1e-9);
    }

    #[test]
    fn estimated_input_cost_uses_the_full_input_price() {
        let price = ModelPrice::from_model(&model(3.0, 15.0));
        let cost = price.cost_of_estimated_input(50_000);
        assert!((cost - 0.15).abs() < 1e-9);
    }
}

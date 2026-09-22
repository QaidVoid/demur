# Providers

demur supports any OpenAI-compatible endpoint with a configurable base URL
(covering hosted APIs, gateways, and self-hosted runtimes such as vLLM or
Ollama), the Anthropic API natively, and a local Claude Code installation
driven headlessly. Every pipeline role names a model, and every role may
use a different provider.

## Provider configuration

A named provider lives under `[providers.<name>]`:

```toml
[providers.openai]
family = "openai"                      # openai, anthropic, or claude-code
base_url = "https://api.openai.com/v1"
key_env = "OPENAI_API_KEY"             # environment variable holding the key
```

```toml
[providers.anthropic]
family = "anthropic"
base_url = "https://api.anthropic.com"
key_env = "ANTHROPIC_API_KEY"
```

The `claude-code` family takes neither a URL nor a key: each pass runs as
a local headless process that holds its own login. See
[Driving demur from Claude Code](/guide/claude-code).

## Keys

The key comes from the environment variable named by `key_env`. On a local
run you may instead name a file with `key_file`; the first line of the file
is used. The key is never logged, never published in a review or check run,
and never appears in an error message: error text that echoes request
material is redacted before it can reach any output.

When using `key_file`, give an absolute path, because `~` is not expanded.
Create the file with the key on the first line and restrict its
permissions:

```bash
mkdir -p ~/.secrets
printf '%s\n' 'sk-your-key' > ~/.secrets/ajam-key
chmod 600 ~/.secrets/ajam-key
```

The environment variable takes precedence: when the variable named by
`key_env` is set, the file is not read.

## Model roles and prices

Every role is required, and every model requires a price, because budgets
are denominated in money:

```toml
[models.triage]
provider = "openai"
name = "gpt-4o-mini"
input_price = 0.15    # USD per million input tokens
output_price = 0.60   # USD per million output tokens
```

There is no bundled price table. A stale table would silently understate
spend, which is the failure a cost-control feature must not have, so a
model without a price fails the run instead of being guessed at.

## Model-specific parameters

Reasoning effort and thinking budgets are translated into each provider's
dialect:

```toml
# OpenAI-compatible: reasoning effort
[models.deep]
provider = "openai"
name = "gpt-4o"
input_price = 2.50
output_price = 10.00
reasoning_effort = "high"   # minimal, low, medium, high
```

```toml
# Anthropic: thinking budget
[models.deep]
provider = "anthropic"
name = "claude-sonnet-4-5"
input_price = 3.00
output_price = 15.00
thinking_budget = 8000      # tokens
```

## Passthrough parameters

Anything the bot does not model is forwarded untouched, so provider-specific
controls are reachable without waiting for support:

```toml
[providers.openai.extra_body]
top_p = 0.9

[providers.openai.extra_headers]
"X-Custom-Header" = "value"

[models.deep.extra_body]
custom_sampling_control = 42
```

## Prompt caching

The pipeline sends the same pull request context across several passes,
which is the shape prompt caching is built for. Prompts put the stable
context first and mark it cacheable where the provider supports it
(Automatic Prompt Caching on OpenAI-compatible endpoints, cache control
markers on Anthropic). Reported cached tokens are priced at
`cached_input_price`, which defaults to half the input price and can be set
explicitly per model.

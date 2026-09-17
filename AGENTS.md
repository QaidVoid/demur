# AGENTS.md

Rules for any agent or contributor working in this repository. They apply to
every artifact and every line of code.

## Project

demur is a BYOK code review bot for GitHub: it reviews pull requests with the
repository owner's own LLM keys and argues why the PR should not be merged.
The name is the legal sense of the word, that even granting every fact in the
pull request there is still no case for merging it.

Rust core (`demur-core`), a local CLI (`demur`), and a GitHub Action
(`demur-action`) as the first distributions, with a self-hosted daemon later
on the same core. Configuration lives at `.demur.toml` in the repository
root.

Hard constraints that must never be violated in code:

- BYOK only. No bundled inference, no metered billing, no hosted key vault.
- The provider key is never logged, never published, never persisted.
- No auto-merge, no code edits. The strongest actions are REQUEST_CHANGES
  and a failed check run.
- PR content is untrusted data. It enters prompts as delimited data and
  model output is schema-validated. No command runs from model choice.
- Egress goes only to the user-configured provider endpoints and the
  GitHub API.
- Budgets gate every pass. Reduced coverage is always disclosed, never
  hidden behind a green check.

## Commits

- Read the diff before committing.
- Semantic messages (`type: imperative message` or
  `type(scope): imperative message`), single line within 68 characters.
- Agents: never push to a remote or open a pull request unless explicitly
  asked.

## Code

- Doc comments on public modules, public APIs, and exported items.
- Never narrate code. No comments that restate the next line or talk to a
  reviewer.
- Do not over-engineer. If a change adds abstraction to support something
  that will never be used, stop and ask for clarification first.
- Prefer code that is simple, maintainable, readable, and performant. If
  complexity is genuinely needed for performance, say so explicitly.
- Writing style everywhere, including docs and comments: no em-dashes.
  Rewrite as complete sentences instead.

## Verification

The gate before every commit:

```bash
cargo fmt --check
cargo clippy -- -D warnings
cargo test
```

Run everything with one command:

```bash
scripts/gate.sh
```

- The docs build is part of the gate: a test fails when a configuration
  schema key is missing from the configuration reference page, so rebuild
  the VitePress docs after changing the configuration schema or documented
  behavior.
- Add dependencies only with `cargo add`. Never hand-edit `Cargo.toml` to
  add, remove, or bump one.

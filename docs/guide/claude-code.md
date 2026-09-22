# Driving demur from Claude Code

<p class="verdict">A Claude Code subscription can power a demur review with
no API key anywhere in demur's configuration. The reviewer keeps its own
judgment, the budget still gates every pass, and the agent never touches a
credential.</p>

demur normally talks to provider endpoints itself, holding your key. The
`claude-code` family replaces that transport with a local headless Claude
Code process: each pass spawns one `claude` run, hands it the pass prompt
on standard input, and takes a schema-validated JSON answer back. The
credentials stay inside the process's own login. demur never sees, stores,
or transmits a key on this path, because there is none to see.

## Configuration

No `base_url`, no `key_env`, no key file. Prices stay required, because
the budget gate prices every pass before it runs:

```toml
profile = "standard"

[providers.claude]
family = "claude-code"

[models.triage]
provider = "claude"
name = "sonnet"
input_price = 3.00
output_price = 15.00

[models.deep]
provider = "claude"
name = "sonnet"
input_price = 3.00
output_price = 15.00

[models.verdict]
provider = "claude"
name = "sonnet"
input_price = 3.00
output_price = 15.00
```

Roles may mix families: triage on your OpenAI key, deep dives through the
subscription, and so on. The `name` field is the model alias passed to the
CLI; the model disclosure in the review names the model that actually
answered, which the CLI reports alongside its own cost figure.

## What the transport guarantees

The command is fixed in the bot. It never comes from configuration and
never from model output. Each pass:

- runs `claude -p` with exec-form arguments, no shell;
- sends the pass prompt on standard input, never on the command line;
- passes the output schema to `--json-schema`, so the answer validates
  before demur reads it;
- disables tools and follow-up turns (`--tools ""`, `--max-turns 1`): a
  pass is one prompt in and one answer out;
- is bounded by a 600 second wall clock, after which the pass fails as an
  ordinary request error and the degradation ladder applies.

A schema-invalid answer follows the same corrective retry path as any
other family. A run that exceeds its budget still skips or shrinks with a
disclosure, exactly as it would over HTTP.

## Cost and the resume cache

The CLI reports what the turn cost. demur records that figure as the
pass's spend and marks it agent-reported in the Spend section. When no
figure is reported, the pass falls back to token pricing at the
configured rates and the Spend section says so. Budget estimates always
use the configured prices, so the gate never waits for a child process to
learn it cannot afford a pass.

The family does not read or write the resume cache. The configured model
name cannot vouch for the model the login actually ran, so a cache hit
could answer with another model's work. Nothing is stored, so nothing
stale can be served.

## Fork pull requests

A GitHub Action run on a fork pull request refuses the family and
publishes the usual fork notice, because the subprocess would act on
input written by someone outside the repository. Locally the family runs
on any input: you are driving your own login on your own machine.

## Letting Claude Code run demur

The repository ships a skill at `skills/demur-review/SKILL.md` that
permits exactly two commands, `demur review` and `demur review-pr`, and
instructs the agent to present the verdict, findings, spend, and any
reduced coverage, and to never edit code in response to a finding. Link
it into the location Claude Code discovers:

```bash
mkdir -p .claude/skills
ln -s ../../skills/demur-review .claude/skills/demur-review
```

A Stop-hook wrapper can make the review gate a session: the hook runs the
review when Claude stops and blocks stopping with the findings until the
verdict approves. The wrapper stays thin, because the JSON output and the
exit status are the whole contract:

```bash
#!/bin/bash
# .claude/hooks/demur-stop.sh — stop-hook: block on demur's verdict.
out=$(demur review --format json 2>/dev/null)
status=$?
[ $status -eq 2 ] && exit 0   # a failed run is reported, not a blocker
[ $status -eq 0 ] && exit 0   # approved: let the session stop
python3 - "$out" <<'EOF'
import json, sys
review = json.loads(sys.argv[1])
lines = [f"- [{f['severity']}] {f['file']}:{f['start_line']} {f['message']}"
         for f in review["findings"]]
print("demur requested changes. Resolve these or override explicitly:")
print("\n".join(lines))
EOF
exit 1
```

demur gains no hook-specific subcommand. The JSON document above is all a
hook needs.

## Why not ACP

Driving the agent through the Agent Client Protocol instead of a headless
process was evaluated and rejected. The ACP route needs a Node-side
adapter process to translate between demur and the agent, adds a runtime
dependency to every distribution, and buys nothing for a one-shot review:
demur needs exactly one prompt in and one structured answer out per pass,
which is what `claude -p --json-schema` already does. ACP exists for long
lived, interactive agent sessions; a review pass is the opposite shape.

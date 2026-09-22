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

No `base_url`, no `key_env`, no key file. Naming any of those, or the
HTTP-dialect `extra_body` and `extra_headers` knobs, fails validation,
because a setting the transport cannot use is a setting that silently
does nothing. Prices stay required, because the budget gate prices every
pass before it runs:

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
- sends the untrusted pull request content on standard input, never on
  the command line; only the static instructions and the schema ride the
  arguments;
- passes the output schema to `--json-schema`, so the answer validates
  before demur reads it;
- disables tools and follow-up turns (`--tools ""`, `--max-turns 1`): a
  pass is one prompt in and one answer out;
- gives each spawned process 600 seconds. A process that exceeds the
  bound is killed and the request retried within the usual retry bound;
  a pass that keeps failing fails as an ordinary request error and the
  degradation ladder applies.

A schema-invalid answer follows the same corrective retry path as any
other family. A run that exceeds its budget still skips or shrinks with a
disclosure, exactly as it would over HTTP.

The CLI's own output ceiling is not carried per request. demur's
`limits.max_tokens` prices and gates the pass; the CLI applies whatever
ceiling its own configuration sets.

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

A GitHub Action run on a fork pull request, or on one whose head origin
cannot be identified, refuses the family before any pass, because the
subprocess would act on input written by someone outside the repository
while carrying its own credentials. The job summary explains this and no
review is attempted. Locally the family runs on any input: you are
driving your own login on your own machine.

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
verdict approves. The hook contract is exit status based: exit 0 lets the
session stop, and exit 2 blocks it with whatever the hook printed on
stderr fed back to Claude. The hook input on standard input tells the
wrapper when a stop was already blocked once (`stop_hook_active`), and
honoring it keeps one blocked stop from looping into a paid re-review.
The wrapper stays thin, because the JSON output and the exit status are
the whole contract:

```bash
#!/bin/bash
# .claude/hooks/demur-stop.sh, a stop hook that blocks on demur's verdict
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
# A stop that a previous block already caused must not re-run the review.
if python3 -c 'import json, sys
sys.exit(0 if json.load(sys.stdin).get("stop_hook_active") else 1)' 2>/dev/null; then
  exit 0
fi
demur review --format json >"$work/review.json" 2>"$work/error"
status=$?
case $status in
  0) exit 0 ;;                    # approved: let the session stop
  2) cat "$work/error" >&2        # a failed run is reported, not a blocker
     exit 0 ;;
esac
if [ ! -s "$work/review.json" ]; then
  echo "demur exited with $status but produced no review document" >&2
  cat "$work/error" >&2
  exit 2
fi
python3 - "$work/review.json" <<'EOF' >&2
import json, sys
review = json.load(open(sys.argv[1]))
lines = [f"- [{f['severity']}] {f['file']}:{f['start_line']} {f['message']}"
         for f in review["findings"]]
print("demur requested changes. Resolve these or override explicitly:")
print("\n".join(lines))
EOF
exit 2   # blocks the stop and feeds the findings back to Claude
```

The document travels through a file and the findings are printed to
stderr, so a large review cannot overflow an argument list, and a failed
run is reported instead of being silently swallowed.

## Why not ACP

Driving the agent through the Agent Client Protocol instead of a headless
process was evaluated and rejected. The ACP route needs a Node-side
adapter process to translate between demur and the agent, adds a runtime
dependency to every distribution, and buys nothing for a one-shot review:
demur needs exactly one prompt in and one structured answer out per pass,
which is what `claude -p --json-schema` already does. ACP exists for long
lived, interactive agent sessions; a review pass is the opposite shape.

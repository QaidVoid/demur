# What demur is

<p class="verdict">demur is a code reviewer with a fixed stance: it grants every
fact in the pull request and then argues why the change still must not be
merged. It runs on your own provider key, and it cannot merge anything.</p>

The name is the legal sense of the word. A demurrer does not dispute the facts.
It says that even if every one of them is true, there is no case. That is the
review demur performs.

## What it does

- Reads the pull request diff, the title, and the description.
- Triages the changed hunks, deep dives the risky clusters through the lenses
  you enabled, and on the deep profile cross-examines the change as a whole.
- Publishes one review event and one check run, with findings ranked by the
  harm they argue and anchored to exact lines.
- Records what it spent, what it covered, and anything it had to cut.

## What it refuses to do

- **It never merges and never edits.** The strongest actions available to it
  are REQUEST_CHANGES and a failed check run. There is no auto-fix commit and
  no auto-merge, and no configuration setting adds one.
- **It never praises.** A finding that cannot name a file, a line range, and a
  concrete harm is dropped at synthesis rather than padded into the review.
- **It never comments on style** unless you enable a style lens.
- **It never hides reduced coverage behind a green check.** If the budget cut a
  pass, the review says which pass and what went unreviewed.
- **It never takes custody of your key.** There is no hosted component, no
  proxy, and nowhere for a key to go except the endpoint you configured.

## What it is not

demur is not a linter and not a replacement for one. It will not tell you that
a name is unclear or that a function is long. Those are your formatter's job
and your own. demur exists to find the reason a change should not ship, and to
say so in a form a human reviewer can check against the diff.

It is also not a consensus machine. It argues one side on purpose. A finding
you disagree with is meant to be dismissed, and a dismissed finding stays
dismissed on every later run.

## Where to go next

- [Setup](/guide/setup) installs the GitHub Action.
- [Local CLI](/guide/local-cli) reviews a pull request or a local range from
  your terminal without publishing anything.
- [The review pipeline](/guide/pipeline) explains the passes and what each one
  costs you.

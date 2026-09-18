# Delta reviews

<p class="verdict">A second push reviews only what it added. What it must never
do is forget: an unresolved finding from an earlier run still sets the verdict
and still fails the check.</p>

## Where the state lives

demur keeps no database and no cache. Every review it publishes carries a hidden
marker in the review body, an inert HTML comment recording:

- the head commit it reviewed,
- what each pass spent,
- the fingerprint, severity, and resolution state of every finding it published.

On the next run demur reads its own prior reviews on the pull request. That is
the entire state mechanism. It works on a fork, it works after a cache
eviction, and it works the first time you install the Action on an open pull
request.

## How scope is derived

| Situation | Scope |
| --- | --- |
| No prior marker | Full review |
| Prior head is an ancestor of the current head | Delta since that commit |
| Prior head is not an ancestor (force-push or rebase) | Full review again |
| Marker missing, malformed, or edited away | Full review |

Missing state always widens scope. It never narrows it, because the failure
mode of a narrowed scope is a defect that never gets reviewed.

## Carry-forward

Narrowing the diff must not narrow the verdict. A finding published by an
earlier run carries into the current run's synthesis until it is actually
resolved, and it counts toward the verdict on equal terms with a fresh finding.

A carried finding is resolved only when one of these is true:

- a human resolved the review thread carrying it,
- the lines it cited were changed and a later pass did not reproduce it.

Pushing a commit that touches unrelated files resolves nothing. Without this,
the cheapest way to turn the check green would be to push a whitespace commit,
which would make the merge gate worthless.

## Dismissal

Dismissal is read from GitHub itself: the resolution state of the review thread
carrying the finding. Resolving the thread dismisses the finding, and its
fingerprint stays silent on every later run.

A reply does not dismiss. A reaction does not dismiss. If demur cannot read
thread resolution state at all, it treats every finding as unresolved and
carries it forward, because a repeated comment is a smaller failure than a
silently cleared gate.

## Fingerprints

Findings are fingerprinted by path, enclosing symbol context, and normalized
message, so a finding survives its lines shifting up or down. A different
message in the same area is a different finding and is raised normally.

After a large rebase, some fingerprints will not match and you may see a
finding repeat once. That is the intended direction of the trade: fingerprint
drift costs a duplicate comment, never a suppressed defect.

## Marker size

Markers accumulate, and a GitHub review body has a hard size limit. When the
marker approaches it, demur prunes resolved fingerprints first, then the oldest
notes, and never an unresolved finding at a blocking severity. If the bound
still cannot be met, the next run falls back to a full review rather than
publishing a truncated marker.

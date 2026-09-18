# Context retrieval

<p class="verdict">A pass can name what it needs to see, and demur decides
whether to fetch it. The model produces names; it never reaches anything
itself.</p>

## The problem it solves

A deep dive gets one cluster of hunks and the pull request description. It sees
a call and not the function called, a changed signature and not its callers, a
modified branch and not the test covering it.

That is the standing weakness of diff-only review. The findings a reviewer can
be most confident about are the local ones, and the defects that actually stop
a merge usually live in the relationship between the change and the code around
it.

## Turning it on

```toml
[retrieval]
enabled = true
max_rounds = 1
max_kb = 64
```

## How a request works

A pass returns findings and, optionally, a list of names:

```json
{
  "findings": [],
  "context_requests": ["symbol:verify_token", "file:src/auth/mod.rs"]
}
```

demur resolves each name, attaches what it found, and runs the pass again. The
second round produces the final findings.

**Naming is not executing.** This is the whole design. The model emits a
string; demur decides what that string means, whether it is allowed, and what
to do when it is not. A request like `file:$(rm -rf /)` is a name that matches
no file, so it fails exactly the way a misspelled path fails. Nothing has to
recognize it as hostile for it to be harmless.

## What demur will not fetch

| Request | Result |
| --- | --- |
| A file in the checkout, not ignored | Fetched |
| A symbol defined in such a file | Its definition and surrounding lines |
| Anything outside the checkout | Refused |
| A symlink pointing out of the checkout | Refused |
| A path you ignore | Refused |
| `.git`, `.demur.toml` | Refused |
| The file named by `key_file` | Refused, wherever it points |
| Anything over the network | Not possible; retrieval reads files |
| A command, a build, a test, a language server | Not possible; nothing runs |

The allowlist is deliberately the set a human reviewer could already open.
Retrieval should not let the bot see what a reviewer cannot, and it reuses your
`ignore.paths` rather than inventing a second idea of what is in scope.

The `key_file` refusal is worth understanding: nothing stops a repository from
keeping its key inside the tree, and a pull request that induced demur to read
it would have put a live credential into a prompt.

## Symbol lookup is approximate

`symbol:verify_token` searches allowed files for a line that looks like a
definition and returns it with surrounding lines. It is a textual search, not a
compiler's view. A real index means per-language parsing, which is a large
dependency for a feature whose value does not depend on being exact.

What matters is that failure is visible. A request demur cannot answer is
reported back to the pass:

```
These requests went unanswered, so argue without them or drop the finding
that depended on them:
symbol:verify_token: nothing found
```

A pass that asked for a definition, received nothing, and was not told so would
argue as though it had been answered. That is a worse failure than not having
retrieval at all.

## What it costs

A round is another call to the deep model with more input tokens, so a pass
that uses retrieval costs roughly twice what it would have. It is estimated,
authorized against your budget, and recorded like any other pass, and a round
the budget cannot fund is skipped and disclosed.

`max_rounds` bounds how many times one pass may ask. `max_kb` bounds total
retrieved content for the whole run; past it, further requests are answered as
unavailable rather than truncated into nonsense.

The review says when a pass retrieved anything:

```
- deep dive (security) on src/auth/token.rs: asked for 2 item(s) of repository
  context, 1 attached (symbol:verify_token, file:src/auth/mod.rs)
```

## Never on a fork

Retrieval is not performed for a pull request from a fork, whatever your
configuration says.

The allowlist stops a hostile diff reaching past it. What it does not stop is a
hostile diff *steering* which allowed file gets read, and therefore what lands
in a review you are going to read. Defending that properly would mean trusting
your ignore paths to be complete, which is not a property anyone has verified.
Removing the question is cheaper than answering it, and it costs a fork
contributor nothing, since a fork run has no provider key anyway.

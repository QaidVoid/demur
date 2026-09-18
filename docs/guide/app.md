# Publishing as yourself, marked as demur's

<p class="verdict">A review you publish can be attributed to you and carry
demur's mark beside your name. The mark says the review was generated, so a
reader weighs it accordingly. It is not a seal of approval.</p>

## What it looks like

GitHub renders an application acting on your behalf as your avatar with the
application's mark badged onto its corner. The author line stays you. That is
the honest picture: you published it, demur wrote it.

Without this, a review you publish looks exactly like something you typed, and
neither you nor a reader can tell the difference.

## Setting it up

**1. Create the application.** Use the manifest in `app/manifest.json`, which
asks for exactly the three permissions demur uses and subscribes to no events.
Upload `app/avatar.png` as its logo. Creating it from the manifest means
confirming a filled-in form rather than answering twenty questions, and a
permission missed by hand does not surface until a review has already been
paid for.

**2. Install it** on the repositories you want to review.

**3. Point demur at it.**

```toml
[app]
client_id = "Iv1.abc123def456"
token_file = "~/.config/demur/authorization.json"
```

**4. Authorize once.** The first publish prints a code:

```
$ demur pr 128 --publish
To publish as yourself marked as demur's work,
open https://github.com/login/device and enter: WDJB-MJHT
```

Enter it, and demur continues. No browser on the machine is required, so this
works over SSH.

## What happens afterwards

Before posting, demur says which identity the review will carry:

```
Publishing as @you, marked as demur's work: the review is yours and
carries demur's mark beside your name.
```

The authorization expires after a few hours. demur renews it without asking
you, because an expired token is a fact about elapsed time rather than a
problem you caused, and failing halfway through publishing a review you already
paid for would be the tool making its bookkeeping your interruption.

If you revoke demur's access, the kept authorization is discarded and you are
asked again.

## Not having one is fine

Publishing without an authorization works exactly as it always has, using
whatever token you already have, unbadged. If demur cannot obtain or renew an
authorization it says so and publishes unbadged rather than failing:

```
could not authorize demur (...); publishing unbadged under your own token
Publishing under your identity @you: findings will appear under your name,
not under a bot identity, and without demur's mark.
```

The mark is worth a one-time authorization. It is never worth a lost review.

## Why workflows are left alone

The Action keeps publishing with the token its workflow provides, and there is
no way to configure otherwise.

An authorization of this kind acts as *you*, across every repository you can
reach. Putting one into repository secrets, where every workflow run can read
it, would trade a genuinely narrow credential for a cosmetic gain. The workflow
token is scoped the way it is precisely because CI is the exposed surface.

A workflow that wants its own visible identity wants a bot identity, which is a
different credential and a different change.

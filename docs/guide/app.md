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

## One application, and what it asks of you

There is one demur application by default, because a mark is only recognizable
if there is one of it. If every user registered their own, every badge would be
whatever avatar that person uploaded, and the mark would mean nothing.

Two things are worth separating here, because they are different in kind.

**demur cannot act as the application.** It has no setting for an application
key, no code that reads one, and nothing that authenticates as the application
rather than as you. A test asserts this by absence, so it stays true.

**Its owner could.** Installing a GitHub App requires the owner to generate a
private key, and a key can mint credentials for every installation of that
application. Nothing in this design prevents that, and saying otherwise would
be an assurance that sounds like security without being it. Installing the
shared application means trusting the person who owns it.

**If you would rather not, register your own.** `client_id` is just
configuration. Create an application under your own account, point demur at it,
and everything works identically — you trade the shared mark for your own
avatar. That is a real alternative, not a theoretical one, and it is why the
shared application is a convenience rather than a requirement.

## Setting it up

**1. Create the application.** `app/manifest.json` records exactly what the
settings must be: three repository permissions, no organization or account
permissions, and no events. A test asserts it matches what the code uses, so
the two cannot drift.

Set it up by hand at **github.com/settings/apps/new**, matching that file.
Two settings are easy to miss:

- **Enable Device Flow** must be checked. Without it, authorizing fails.
- **Webhook → Active** must be unchecked. demur is triggered by you running it,
  never by GitHub pushing to it.

Leave **Expire user authorization tokens** checked: it is what provides the
refresh token demur uses to renew silently.

Upload `app/avatar.png` as the logo. That is the badge.

You will be asked to generate a private key before you can install the
application. That is GitHub's requirement, not demur's: nothing here reads or
accepts one. Generate it, keep it somewhere safe, and know that its existence
is what the trust above is about.

**2. Install it** on the repositories you want to review.

**3. Point demur at it.**

```toml
[app]
client_id = "Iv1.abc123def456"
token_file = "/home/you/.config/demur/authorization.json"
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

## GitHub Enterprise

Authorization runs against whichever GitHub you are pointed at. On an
enterprise instance demur derives the host from `GITHUB_SERVER_URL` when it is
set and from `GITHUB_API_URL` otherwise, so the device flow goes to your own
instance rather than to github.com.

Create the application on that instance, not on github.com.

## Why workflows are left alone

The Action keeps publishing with the token its workflow provides, and there is
no way to configure otherwise.

An authorization of this kind acts as *you*, across every repository you can
reach. Putting one into repository secrets, where every workflow run can read
it, would trade a genuinely narrow credential for a cosmetic gain. The workflow
token is scoped the way it is precisely because CI is the exposed surface.

A workflow that wants its own visible identity wants a bot identity, which is a
different credential and a different change.

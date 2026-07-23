---
sidebar_position: 2
title: Accounts
---

# Accounts

Your hub account is how the web UI knows who you are. It carries a password login, an optional enrolled
email, an optional second factor, and any number of device signing keys. This page covers getting an
account, logging in, and the self-service security controls on the Account page.

## Get an account

There are two ways an account is created, and which one applies depends on how the hub was deployed.

- **Self-service registration**, when the operator enabled it. The sign-in page offers a "create account"
  form. It creates a normal, non-admin account and logs you in. Registration can never grant admin.
- **Admin-created**, otherwise. The hub is invite-only by default, and an administrator runs
  `agit-hub user add <name>` to create your account and hand you the password.

Usernames are 2 to 32 lowercase characters from `[a-z0-9._-]`, with no leading dot. A username and an
organization name can never be the same string.

For how an operator turns registration on, see [Deploy the hub](../self-hosting/deploying.md).

## Log in and out

Sign in with your username and password. A successful login returns a session cookie; that cookie is the
credential for every web action. Logging out revokes the session.

The password prompt is deliberately slow (the hub hashes with argon2id) and does not say whether a failed
login was a wrong username or a wrong password. If your account has been disabled by an admin, a correct
password returns a clear "account is disabled" message rather than a session.

Tokens and device keys are for git and scripts, not the web UI, and admin actions such as issuing a token
always require a login session, never a token. See [Tokens](./tokens.md).

## Change your password

From the Account page, change your password by supplying your current password and a new one (at least 8
characters). Changing it revokes every other session for the account but keeps the tab you changed it
from signed in, so rotating your password kicks a stolen cookie without logging you out.

## Reset a forgotten password

If you are locked out, request a reset from the sign-in page. The hub mints a single-use, expiring reset
link. Delivery is operator-forwarded by default: the link is written to the server log for an
administrator to forward to you. Open the link and set a new password. Consuming the link revokes every
session for the account. An administrator can also reset a locked-out account's password directly.

The request endpoint answers the same way whether or not the account exists, so it never reveals which
usernames are registered.

## Verify your email

Your account's email of record is the committer email on your primary device key, self-asserted when you
enroll a key (see [Signing keys](./signing-keys.md)). Verifying it is the anti-squatting gate: a session's
committer email attributes to your account (a `VERIFIED AS` provenance verdict) only once you have proven
control of the address.

- The Account page shows a Verified or Unverified badge next to your email.
- Use **Resend** to mint a fresh verification link. As with password reset, delivery is
  operator-forwarded (the URL is logged for an administrator to forward), and the token is never returned
  in the page.
- Open the link to mark the address verified. The link is single-use and expires after 24 hours.
- Changing the enrolled email (by enrolling a key with a different address) resets the verified flag, so
  the new address must be proven again.

An administrator can also force-verify an address out of band with `agit-hub user verify-email <name>`.
The link base comes from the operator's `AGIT_HUB_BASE_URL`; see
[Hub configuration](../self-hosting/configuration.md).

## Two-factor authentication (TOTP)

Turn on a second factor from the Account page. Once active, a correct password alone no longer signs you
in; login also requires a current code or an unused backup code.

1. **Enroll.** The hub generates a secret and shows it as an `otpauth://` provisioning URI (and the raw
   secret) for an authenticator app. Enrolling does not turn 2FA on yet.
2. **Confirm.** Enter a 6-digit code from the app. On success 2FA goes active and the hub returns 10
   one-time backup codes. They are shown once; only their digests are stored. Save them somewhere safe.
3. **Log in.** From then on, login asks for a code. A current TOTP or any unused backup code works.
4. **Disable.** Turn 2FA off with any one of a current code, an unused backup code, or your account
   password.

Re-enrolling while 2FA is already active is refused; disable it first. If you lose your authenticator and
your backup codes, an administrator can clear your 2FA so you can enroll again.

## Related

- [Signing keys](./signing-keys.md): enroll a device key and attribute your sessions to you.
- [Organizations](./organizations.md): shared ownership and invitation-only membership.
- [Tokens](./tokens.md): credentials for git and scripts.

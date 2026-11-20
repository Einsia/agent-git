# Organization invitations

An organization owner can invite an existing account using their normal `agit login`:

```sh
agit org invite acme --user carol
```

Both names are required. The command uses the selected Hub and signed-in account and does
not infer an organization or recipient from the current folder, a repository, or an email address.
Personal access tokens, including sessions created by `agit login --with-token`, cannot invite.

The recipient is invited as a member and joins only after accepting in the Hub. The output names
the Hub, account, organization, recipient, pending state and server expiry. `--json` returns the
same information in the CLI's JSON envelope. No email is sent by this username invitation.

Repeating a pending member invitation returns its existing expiry unchanged. An existing member
or a pending invitation with another role is refused without changing it. Review and revoke
invitations at the organization settings URL printed by the command; recipients accept through
the existing Hub invitation flow.

If the Hub does not support this operation, upgrade it or use its website. The command never
falls back to changing membership. If a response is lost or cannot be confirmed, check organization
settings before trying again; the CLI does not automatically resend an uncertain invitation.

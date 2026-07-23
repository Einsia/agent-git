---
sidebar_position: 3
title: Organizations
---

# Organizations

An organization is a shared owner. Agents owned by an org are reachable by every org member according to
their role, so a team's agents outlive any one person's account. Org names use the same rules as usernames
and share the same namespace: a name is either a user or an org, never both.

## Create an org

Any signed-in user can create an org from the web UI. The creator becomes its first and only admin.
Members can create agents under the org by default; an admin can restrict creation to admins in the org
settings.

## The org overview page

`/orgs/<name>` is the org's overview. It lists the org's members and every agent the org owns, plus each
member's personal agents that you are allowed to read. Each agent row shows its session count and the code
environments it has worked in.

The page is limited to org members and site admins. It lists only agents you may read, and it filters
before it counts, so it never reveals an agent, or even a session count, that you could not already see. A
member's private personal agent does not inherit org grants and simply does not appear for anyone without
an explicit grant on it.

## Roles

A member is either `member` or `admin`.

- **Members** read and (with a write grant on an org-owned agent) push. By default they can also create
  agents under the org.
- **Admins** additionally manage the roster: invite people, change roles, remove members, edit settings,
  transfer ownership, and delete the org.

An org must always keep at least one admin. Demoting or removing the last admin is refused.

## Membership is invitation-only

You cannot add a stranger to an org directly. The only ways in are an accepted invitation and an ownership
transfer to an existing member.

**Invite.** An org admin invites an existing user with a target role (`member` or `admin`). This creates a
pending invitation; it does not grant anything yet. From the CLI an operator can invite with
`agit-hub org invite <org> <user> [--role R]` and list pending invitations with
`agit-hub org invitations <org>`.

**Accept or decline.** The invited person sees their pending invitations on their own account
(`GET /api/me/invitations`) and accepts or declines. Only the named invitee can act on an invitation.
Accepting mints the membership with the offered role; declining grants nothing. Either resolution is final,
so an invitation cannot be replayed.

Changing an existing member's role is a separate admin action on the roster and does not go through the
invitation flow.

## Settings

Org admins control:

- **Member-create policy.** Whether members may create agents under the org (the default) or only admins
  can.
- Encryption recovery and hub-assist escrow options, when your team uses them.

## Transfer ownership

An org admin can hand ownership to another member. The new owner must already be a member. Transfer
promotes them to admin and demotes the caller to member in one step. Transfer is management-grade: it
requires a login session and cannot be done with a token, even an admin's write token.

## Delete an org

An org admin can delete an org, but only once it owns no agents. Transfer or delete the org's agents first,
or the delete is refused so no repository or blob data is orphaned. Deleting an org removes it, every
membership, and every invitation for it. Like transfer, delete requires a login session, not a token.

## Related

- [Accounts](./accounts.md): how a person becomes an invitable user.
- [Repositories](./repositories.md): the code repositories an org's agents have touched.
- [Tokens](./tokens.md): scoped credentials for git and scripts against org-owned agents.

//! Hub authorization — **the only decision point**.
//!
//! Pure functions here: `decide(caller, agent, action) -> Decision`. No IO, no HTTP, never looks at
//! token plaintext, and so it can be tested exhaustively. Every entry point into the Hub (the JSON
//! API, git smart-http, the CLI) must clear this gate — git smart-http most of all, since the old
//! code there only checked `path.contains(".git/")` before throwing the request at
//! `git http-backend` (with `GIT_HTTP_EXPORT_ALL=1` on, no less), so once past the "read gate",
//! **any** repo under root could be pulled. A real authorization point has to know **which agent**,
//! which is why `AgentAcl` is part of the decision's input.
//!
//! Ordering (important): a token's grant is an **upper bound** — cap first, then look at the user's
//! own identity. A read-only token in an admin's hands still only reads: this is an *intersection*,
//! not a *maximum*.

/// What the caller wants to do to an agent. Three tiers, mapped to real entry points:
///   Read   — view sessions / metadata; `git fetch` / `clone` (upload-pack)
///   Write  — `git push` (receive-pack)
///   Manage — change visibility / members / name, delete the repo: destructive actions
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Read,
    Write,
    Manage,
}

/// A member's role on an agent. Ordered: Admin > Write > Read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Role {
    Read,
    Write,
    Admin,
}

impl Role {
    pub fn parse(s: &str) -> Option<Role> {
        match s {
            "read" => Some(Role::Read),
            "write" => Some(Role::Write),
            "admin" => Some(Role::Admin),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Role::Read => "read",
            Role::Write => "write",
            Role::Admin => "admin",
        }
    }

    /// Whether this role may take this action. Manage is admin-only — write can push code, but not
    /// delete the repo or change its visibility.
    fn allows(self, action: Action) -> bool {
        match action {
            Action::Read => true,
            Action::Write => self >= Role::Write,
            Action::Manage => self == Role::Admin,
        }
    }
}

/// Where an agent is in its life. **Orthogonal to visibility**: a public archived agent is still
/// public, it has just stopped accepting pushes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lifecycle {
    Active,
    /// Read-only, still listed and still readable. The memory is finished, not gone.
    Archived,
    /// Soft-deleted: invisible and unusable, but the record survives so a restore is possible — and
    /// so the **name stays taken**, since a name handed out again would silently redirect every
    /// `.agit.toml` and token that still points at it.
    Deleted,
}

impl Lifecycle {
    pub fn parse(s: &str) -> Option<Lifecycle> {
        match s {
            "active" => Some(Lifecycle::Active),
            "archived" => Some(Lifecycle::Archived),
            "deleted" => Some(Lifecycle::Deleted),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Lifecycle::Active => "active",
            Lifecycle::Archived => "archived",
            Lifecycle::Deleted => "deleted",
        }
    }
}

/// An agent's visibility. **Private by default** — transcripts carry prompts, paths, and sometimes
/// secrets, so the only direction failure may take is "invisible".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Visibility {
    Private,
    Public,
}

impl Visibility {
    pub fn parse(s: &str) -> Option<Visibility> {
        match s {
            "private" => Some(Visibility::Private),
            "public" => Some(Visibility::Public),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Visibility::Private => "private",
            Visibility::Public => "public",
        }
    }
}

/// A token's grant scope. Read/write only — a token can never Manage (see `decide`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Read,
    Write,
}

impl Scope {
    pub fn parse(s: &str) -> Option<Scope> {
        match s {
            "read" => Some(Scope::Read),
            "write" => Some(Scope::Write),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Scope::Read => "read",
            Scope::Write => "write",
        }
    }
}

/// The **grant upper bound** carried by the token presented on this request: which agent it is bound
/// to (None = unbound) plus the read/write cap.
#[derive(Debug, Clone)]
pub struct TokenGrant {
    /// Some(name) = this token is only valid for that one agent.
    pub agent: Option<String>,
    pub scope: Scope,
}

/// Who is making this request.
///
/// `user` is the **already authenticated** identity (a cookie session, or a token's owner); None =
/// anonymous. A present `token` means the request arrived with a token — and is therefore
/// additionally constrained by that token's upper bound.
#[derive(Debug, Clone, Default)]
pub struct Caller {
    pub user: Option<String>,
    pub is_admin: bool,
    pub token: Option<TokenGrant>,
}

impl Caller {
    pub fn anonymous() -> Caller {
        Caller::default()
    }

    pub fn user(name: &str) -> Caller {
        Caller { user: Some(name.to_string()), is_admin: false, token: None }
    }

    pub fn admin(name: &str) -> Caller {
        Caller { user: Some(name.to_string()), is_admin: true, token: None }
    }

    /// Hang a token upper bound on this caller (chainable; convenient in tests).
    pub fn with_token(mut self, agent: Option<&str>, scope: Scope) -> Caller {
        self.token = Some(TokenGrant { agent: agent.map(|s| s.to_string()), scope });
        self
    }
}

/// The access-control facts for one agent. Comes from agents.json (see `super::store`).
#[derive(Debug, Clone)]
pub struct AgentAcl {
    pub name: String,
    /// None = unowned (an old repo migrated in, not yet claimed) — only the site admin can touch it.
    pub owner: Option<String>,
    pub visibility: Visibility,
    pub lifecycle: Lifecycle,
    pub members: Vec<(String, Role)>,
}

impl AgentAcl {
    /// Unowned and private — the **fail-safe** value for an unknown repo.
    pub fn unowned(name: &str) -> AgentAcl {
        AgentAcl {
            name: name.to_string(),
            owner: None,
            visibility: Visibility::Private,
            lifecycle: Lifecycle::Active,
            members: vec![],
        }
    }

    fn member_role(&self, user: &str) -> Option<Role> {
        self.members.iter().find(|(n, _)| n == user).map(|(_, r)| *r)
    }
}

/// The **reason** for a denial — not just false. The audit log records it, and the HTTP layer uses
/// it to tell 401/403/404 apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Deny {
    /// Anonymous, and this agent is not public (or the action is not a read). → 401, one chance to
    /// authenticate.
    Anonymous,
    /// Authenticated, but without enough grant on this agent.
    NoGrant,
    /// The token is bound to a different agent.
    TokenOtherAgent,
    /// The token only has read, but a write was asked for.
    TokenScope,
    /// Management actions do not accept tokens — deleting a repo or changing visibility must be the
    /// person themselves, on a login session.
    TokenCannotManage,
    /// The agent is archived: read-only until someone unarchives it. → 403; they can still see it,
    /// and the state is the answer to "why was my push refused".
    Archived,
    /// The agent is soft-deleted. → 404 for everyone: a deleted agent is not a thing you can find,
    /// only a thing its owner can restore.
    Deleted,
}

impl Deny {
    /// A one-liner for humans (also goes into the audit log's detail).
    pub fn reason(self) -> &'static str {
        match self {
            Deny::Anonymous => "authentication required",
            Deny::NoGrant => "no permission on this agent",
            Deny::TokenOtherAgent => "this token is bound to another agent",
            Deny::TokenScope => "this token only has read permission",
            Deny::TokenCannotManage => "management actions cannot use a token; use a login session",
            Deny::Archived => "this agent is archived; unarchive it before writing to it",
            Deny::Deleted => "this agent is deleted",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny(Deny),
}

impl Decision {
    pub fn allowed(self) -> bool {
        self == Decision::Allow
    }
}

/// **The one authorization decision**. A pure function: same input, same output, always, no IO.
///
/// The rules, in order:
///  0. Lifecycle cap: archived denies Write, deleted denies everything but Manage. An upper bound on
///     the *agent's* side, exactly as the token is one on the caller's.
///  1. Token cap: wrong binding / scope exceeded / attempting Manage — deny first. A token is an
///     upper bound, not a source of power.
///  2. Site admin: allow (still under 0's and 1's cap).
///  3. Owner: allow.
///  4. Explicit member: allow per role.
///  5. public: allow Read only (anonymous included).
///  6. Everything else is denied — deny by default, not allow by default.
pub fn decide(caller: &Caller, agent: &AgentAcl, action: Action) -> Decision {
    // 0. The agent's own upper bound. Same shape as the token cap below and for the same reason —
    //    intersection, not maximum: being the owner does not make an archived agent writable, it
    //    makes you the person who can unarchive it.
    //
    //    Manage survives both states deliberately: unarchiving and restoring *are* Manage, and a
    //    state you cannot leave is not a state, it is a wall. Every other entry point — the JSON
    //    API, git smart-http, the agent list — reads its answer from here, so "archived" and
    //    "deleted" mean the same thing at all of them without any of them being told about it.
    match agent.lifecycle {
        Lifecycle::Active => {}
        Lifecycle::Archived if action == Action::Write => return Decision::Deny(Deny::Archived),
        Lifecycle::Archived => {}
        Lifecycle::Deleted if action != Action::Manage => return Decision::Deny(Deny::Deleted),
        Lifecycle::Deleted => {}
    }

    // 1. The token's upper bound. First, because an admin holding a read-only token still only
    //    reads (intersection, not maximum).
    if let Some(t) = &caller.token {
        if let Some(bound) = &t.agent {
            if bound != &agent.name {
                return Decision::Deny(Deny::TokenOtherAgent);
            }
        }
        match action {
            Action::Manage => return Decision::Deny(Deny::TokenCannotManage),
            Action::Write if t.scope != Scope::Write => return Decision::Deny(Deny::TokenScope),
            _ => {}
        }
    }

    // 2..4 all require an identity first. Anonymous can only reach rule 5.
    if let Some(user) = &caller.user {
        if caller.is_admin {
            return Decision::Allow;
        }
        if agent.owner.as_deref() == Some(user.as_str()) {
            return Decision::Allow;
        }
        if let Some(role) = agent.member_role(user) {
            if role.allows(action) {
                return Decision::Allow;
            }
        }
    }

    // 5. public buys read only — making an agent public is not an invitation to write to it.
    if agent.visibility == Visibility::Public && action == Action::Read {
        return Decision::Allow;
    }

    // 6. Deny by default.
    match caller.user {
        None => Decision::Deny(Deny::Anonymous),
        Some(_) => Decision::Deny(Deny::NoGrant),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn public_agent() -> AgentAcl {
        AgentAcl {
            name: "shared".into(),
            owner: Some("alice".into()),
            visibility: Visibility::Public,
            lifecycle: Lifecycle::Active,
            members: vec![],
        }
    }

    fn private_agent() -> AgentAcl {
        AgentAcl {
            name: "secret".into(),
            owner: Some("alice".into()),
            visibility: Visibility::Private,
            lifecycle: Lifecycle::Active,
            members: vec![("bob".into(), Role::Read), ("carol".into(), Role::Write), ("dave".into(), Role::Admin)],
        }
    }

    fn at(lifecycle: Lifecycle) -> AgentAcl {
        AgentAcl { lifecycle, ..private_agent() }
    }

    // ── anonymous ──

    #[test]
    fn anonymous_reads_public_only() {
        let a = Caller::anonymous();
        assert!(decide(&a, &public_agent(), Action::Read).allowed());
        assert_eq!(decide(&a, &private_agent(), Action::Read), Decision::Deny(Deny::Anonymous));
    }

    #[test]
    fn anonymous_never_writes_even_public() {
        // public = readable, not writable. The old Hub's "reads are open" gate must not wave writes
        // through along with them.
        let a = Caller::anonymous();
        assert_eq!(decide(&a, &public_agent(), Action::Write), Decision::Deny(Deny::Anonymous));
        assert_eq!(decide(&a, &public_agent(), Action::Manage), Decision::Deny(Deny::Anonymous));
    }

    // ── owner / admin ──

    #[test]
    fn owner_does_everything() {
        let alice = Caller::user("alice");
        for act in [Action::Read, Action::Write, Action::Manage] {
            assert!(decide(&alice, &private_agent(), act).allowed(), "{act:?}");
        }
    }

    #[test]
    fn site_admin_does_everything_even_unowned() {
        let root = Caller::admin("root");
        for act in [Action::Read, Action::Write, Action::Manage] {
            assert!(decide(&root, &AgentAcl::unowned("orphan"), act).allowed(), "{act:?}");
        }
    }

    #[test]
    fn unowned_private_agent_is_invisible_to_everyone_else() {
        // A migrated-in old repo has no owner: nobody but the site admin should see it.
        let orphan = AgentAcl::unowned("orphan");
        assert_eq!(decide(&Caller::user("alice"), &orphan, Action::Read), Decision::Deny(Deny::NoGrant));
        assert_eq!(decide(&Caller::anonymous(), &orphan, Action::Read), Decision::Deny(Deny::Anonymous));
    }

    // ── member roles ──

    #[test]
    fn member_roles_ladder() {
        let agent = private_agent();
        let bob = Caller::user("bob"); // read
        assert!(decide(&bob, &agent, Action::Read).allowed());
        assert_eq!(decide(&bob, &agent, Action::Write), Decision::Deny(Deny::NoGrant));
        assert_eq!(decide(&bob, &agent, Action::Manage), Decision::Deny(Deny::NoGrant));

        let carol = Caller::user("carol"); // write
        assert!(decide(&carol, &agent, Action::Read).allowed());
        assert!(decide(&carol, &agent, Action::Write).allowed());
        assert_eq!(decide(&carol, &agent, Action::Manage), Decision::Deny(Deny::NoGrant));

        let dave = Caller::user("dave"); // admin
        assert!(decide(&dave, &agent, Action::Read).allowed());
        assert!(decide(&dave, &agent, Action::Write).allowed());
        assert!(decide(&dave, &agent, Action::Manage).allowed());
    }

    #[test]
    fn stranger_gets_nothing_from_a_private_agent() {
        let eve = Caller::user("eve");
        for act in [Action::Read, Action::Write, Action::Manage] {
            assert_eq!(decide(&eve, &private_agent(), act), Decision::Deny(Deny::NoGrant), "{act:?}");
        }
    }

    #[test]
    fn logged_in_stranger_reads_public_but_cannot_push() {
        let eve = Caller::user("eve");
        assert!(decide(&eve, &public_agent(), Action::Read).allowed());
        assert_eq!(decide(&eve, &public_agent(), Action::Write), Decision::Deny(Deny::NoGrant));
    }

    // ── token upper bounds: this is where "a token is no longer a site-wide pass" lives ──

    #[test]
    fn token_bound_to_one_agent_cannot_touch_another() {
        // This one is the old model's disease: one token = the whole host.
        let alice = Caller::user("alice").with_token(Some("other"), Scope::Write);
        assert_eq!(decide(&alice, &private_agent(), Action::Read), Decision::Deny(Deny::TokenOtherAgent));
        assert_eq!(decide(&alice, &private_agent(), Action::Write), Decision::Deny(Deny::TokenOtherAgent));
    }

    #[test]
    fn token_bound_to_this_agent_works_within_scope() {
        let alice = Caller::user("alice").with_token(Some("secret"), Scope::Write);
        assert!(decide(&alice, &private_agent(), Action::Read).allowed());
        assert!(decide(&alice, &private_agent(), Action::Write).allowed());
    }

    #[test]
    fn read_token_never_writes_even_for_the_owner() {
        let alice = Caller::user("alice").with_token(None, Scope::Read);
        assert!(decide(&alice, &private_agent(), Action::Read).allowed());
        assert_eq!(decide(&alice, &private_agent(), Action::Write), Decision::Deny(Deny::TokenScope));
    }

    #[test]
    fn read_token_never_writes_even_for_a_site_admin() {
        // Intersection, not maximum: being an admin cannot stretch a token's scope.
        let root = Caller::admin("root").with_token(None, Scope::Read);
        assert!(decide(&root, &private_agent(), Action::Read).allowed());
        assert_eq!(decide(&root, &private_agent(), Action::Write), Decision::Deny(Deny::TokenScope));
    }

    #[test]
    fn token_never_manages() {
        // Deleting a repo or changing visibility must be the person's own login session — leaking a
        // CI token must not get the repo deleted.
        for caller in [
            Caller::user("alice").with_token(None, Scope::Write),
            Caller::admin("root").with_token(None, Scope::Write),
        ] {
            assert_eq!(decide(&caller, &private_agent(), Action::Manage), Decision::Deny(Deny::TokenCannotManage));
        }
    }

    #[test]
    fn token_does_not_grant_what_the_user_lacks() {
        // A write token in a read-only member's hands — still read-only. A token is an upper bound,
        // not a source of power.
        let bob = Caller::user("bob").with_token(Some("secret"), Scope::Write);
        assert!(decide(&bob, &private_agent(), Action::Read).allowed());
        assert_eq!(decide(&bob, &private_agent(), Action::Write), Decision::Deny(Deny::NoGrant));
    }

    #[test]
    fn token_on_public_agent_still_needs_a_grant_to_push() {
        let eve = Caller::user("eve").with_token(None, Scope::Write);
        assert!(decide(&eve, &public_agent(), Action::Read).allowed());
        assert_eq!(decide(&eve, &public_agent(), Action::Write), Decision::Deny(Deny::NoGrant));
    }

    // ── lifecycle: an upper bound on the agent's side ──

    #[test]
    fn archived_is_read_only_for_everyone_including_the_owner() {
        // Intersection, not maximum — the same rule the token cap follows. Owning an archived agent
        // does not make it writable; it makes you the person who can unarchive it.
        let archived = at(Lifecycle::Archived);
        for caller in [Caller::user("alice"), Caller::admin("root"), Caller::user("carol")] {
            assert!(decide(&caller, &archived, Action::Read).allowed());
            assert_eq!(decide(&caller, &archived, Action::Write), Decision::Deny(Deny::Archived));
        }
    }

    #[test]
    fn archiving_is_a_state_you_can_leave() {
        // Manage must survive the cap, or unarchive is unreachable and the state is a wall.
        assert!(decide(&Caller::user("alice"), &at(Lifecycle::Archived), Action::Manage).allowed());
        assert!(decide(&Caller::user("dave"), &at(Lifecycle::Archived), Action::Manage).allowed());
    }

    #[test]
    fn archiving_does_not_widen_anything() {
        // The cap only ever subtracts: a stranger gets no more from an archived agent than a live
        // one, and read-members do not become writers by way of a state change.
        assert_eq!(decide(&Caller::user("eve"), &at(Lifecycle::Archived), Action::Read), Decision::Deny(Deny::NoGrant));
        assert_eq!(decide(&Caller::user("bob"), &at(Lifecycle::Archived), Action::Manage), Decision::Deny(Deny::NoGrant));
    }

    #[test]
    fn a_deleted_agent_is_invisible_to_everyone() {
        // Not 403-with-a-name: Read is denied outright, which is what makes it drop out of the agent
        // list and answer 404 — the same shape as "this agent does not exist".
        let deleted = at(Lifecycle::Deleted);
        for caller in [Caller::anonymous(), Caller::user("eve"), Caller::user("bob"), Caller::user("alice"), Caller::admin("root")] {
            assert_eq!(decide(&caller, &deleted, Action::Read), Decision::Deny(Deny::Deleted), "{caller:?}");
            assert_eq!(decide(&caller, &deleted, Action::Write), Decision::Deny(Deny::Deleted), "{caller:?}");
        }
    }

    #[test]
    fn a_deleted_agent_can_still_be_restored_by_whoever_could_manage_it() {
        // Manage is the one door left open — restore is a Manage action, so closing it would make
        // soft-delete indistinguishable from destruction.
        assert!(decide(&Caller::user("alice"), &at(Lifecycle::Deleted), Action::Manage).allowed());
        assert!(decide(&Caller::admin("root"), &at(Lifecycle::Deleted), Action::Manage).allowed());
        // ...and only for them. Deletion is not an opening.
        assert_eq!(decide(&Caller::user("eve"), &at(Lifecycle::Deleted), Action::Manage), Decision::Deny(Deny::NoGrant));
        assert_eq!(decide(&Caller::anonymous(), &at(Lifecycle::Deleted), Action::Manage), Decision::Deny(Deny::Anonymous));
    }

    #[test]
    fn a_token_cannot_restore_or_unarchive() {
        // Both are Manage, and rule 1 still refuses tokens for Manage. A leaked CI token must not be
        // able to bring a deleted agent back, any more than it could delete one.
        let alice = Caller::user("alice").with_token(None, Scope::Write);
        assert_eq!(decide(&alice, &at(Lifecycle::Deleted), Action::Manage), Decision::Deny(Deny::TokenCannotManage));
        assert_eq!(decide(&alice, &at(Lifecycle::Archived), Action::Manage), Decision::Deny(Deny::TokenCannotManage));
    }

    #[test]
    fn a_public_archived_agent_is_still_public() {
        // Lifecycle and visibility are orthogonal: archiving is not a way to hide something, and it
        // must not quietly become one.
        let a = AgentAcl { lifecycle: Lifecycle::Archived, ..public_agent() };
        assert!(decide(&Caller::anonymous(), &a, Action::Read).allowed());
        assert_eq!(decide(&Caller::anonymous(), &a, Action::Write), Decision::Deny(Deny::Archived));
    }

    // ── role / visibility parsing ──

    #[test]
    fn lifecycle_parses_roundtrip_and_rejects_junk() {
        for l in [Lifecycle::Active, Lifecycle::Archived, Lifecycle::Deleted] {
            assert_eq!(Lifecycle::parse(l.as_str()), Some(l));
        }
        assert_eq!(Lifecycle::parse("Active"), None);
        assert_eq!(Lifecycle::parse("purged"), None);
        assert_eq!(Lifecycle::parse(""), None);
    }

    #[test]
    fn role_and_visibility_parse_roundtrip() {
        for r in [Role::Read, Role::Write, Role::Admin] {
            assert_eq!(Role::parse(r.as_str()), Some(r));
        }
        for v in [Visibility::Private, Visibility::Public] {
            assert_eq!(Visibility::parse(v.as_str()), Some(v));
        }
        for s in [Scope::Read, Scope::Write] {
            assert_eq!(Scope::parse(s.as_str()), Some(s));
        }
        assert_eq!(Role::parse("owner"), None);
        assert_eq!(Role::parse(""), None);
        assert_eq!(Visibility::parse("Public"), None);
        assert_eq!(Scope::parse("admin"), None);
    }
}

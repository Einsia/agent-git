//! Hub 授权 —— **唯一的判定点**。
//!
//! 这里是纯函数：`decide(caller, agent, action) -> Decision`。不碰 IO、不碰 HTTP、不看 token 明文，
//! 于是可以被穷举测试。Hub 的每个入口（JSON API、git smart-http、CLI）都必须过这一关，
//! 尤其是 git smart-http —— 老代码在那里只看 `path.contains(".git/")` 就把请求丢给
//! `git http-backend`（还开着 `GIT_HTTP_EXPORT_ALL=1`），于是"读闸"一过，root 下**任何**
//! 仓库都能拉走。真正的授权点必须知道**是哪个 agent**，所以判定的输入里带着 `AgentAcl`。
//!
//! 判定顺序（重要）：token 的授权是**上界**，先封顶，再看用户自己的身份。一个只读 token
//! 落在管理员手里，也只能读 —— 这是"交集"，不是"取最大"。

/// 调用方想对某个 agent 做的事。三档，映射到真实入口：
///   Read   —— 看 session / 元数据；`git fetch` / `clone`（upload-pack）
///   Write  —— `git push`（receive-pack）
///   Manage —— 改可见性 / 改成员 / 改名 / 删库：破坏性动作
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Read,
    Write,
    Manage,
}

/// 成员在某个 agent 上的角色。有序：Admin > Write > Read。
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

    /// 这个角色能不能做这个动作。Manage 只给 admin —— write 能推代码，但不能把库删了或改可见性。
    fn allows(self, action: Action) -> bool {
        match action {
            Action::Read => true,
            Action::Write => self >= Role::Write,
            Action::Manage => self == Role::Admin,
        }
    }
}

/// agent 的可见性。**默认 Private** —— 转录里有 prompt、路径、有时还有密钥，失败方向只能是"看不见"。
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

/// token 的授权范围。只有读/写两档 —— token 永远做不了 Manage（见 `decide`）。
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

/// 这次请求出示的 token 所带的**授权上界**：绑哪个 agent（None = 不绑）+ 读写封顶。
#[derive(Debug, Clone)]
pub struct TokenGrant {
    /// Some(name) = 这个 token 只对这一个 agent 有效。
    pub agent: Option<String>,
    pub scope: Scope,
}

/// 谁在发这个请求。
///
/// `user` 是**已经认证过**的身份（cookie 会话，或 token 的属主）；None = 匿名。
/// `token` 有值表示这次是拿 token 来的 —— 于是要额外受 token 的上界约束。
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

    /// 给这个 caller 挂上一个 token 上界（链式，测试里好写）。
    pub fn with_token(mut self, agent: Option<&str>, scope: Scope) -> Caller {
        self.token = Some(TokenGrant { agent: agent.map(|s| s.to_string()), scope });
        self
    }
}

/// 一个 agent 的访问控制事实。从 agents.json 来（见 `super::store`）。
#[derive(Debug, Clone)]
pub struct AgentAcl {
    pub name: String,
    /// None = 无主（老仓库迁移过来、还没认领）—— 只有站点管理员碰得到。
    pub owner: Option<String>,
    pub visibility: Visibility,
    pub members: Vec<(String, Role)>,
}

impl AgentAcl {
    /// 无主私有 —— 未知仓库的**失败安全**取值。
    pub fn unowned(name: &str) -> AgentAcl {
        AgentAcl { name: name.to_string(), owner: None, visibility: Visibility::Private, members: vec![] }
    }

    fn member_role(&self, user: &str) -> Option<Role> {
        self.members.iter().find(|(n, _)| n == user).map(|(_, r)| *r)
    }
}

/// 拒绝的**原因** —— 不只是 false。审计日志要写它，HTTP 层要靠它区分 401/403/404。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Deny {
    /// 匿名，而这个 agent 不是 public（或动作不是读）。→ 401，给一次认证机会。
    Anonymous,
    /// 认证过了，但在这个 agent 上没有足够的授权。
    NoGrant,
    /// token 绑在别的 agent 上。
    TokenOtherAgent,
    /// token 只有 read，却要写。
    TokenScope,
    /// 管理动作不接受 token —— 删库/改可见性这种事，必须是登录会话本人。
    TokenCannotManage,
}

impl Deny {
    /// 给人看的一句话（也进审计日志的 detail）。
    pub fn reason(self) -> &'static str {
        match self {
            Deny::Anonymous => "需要认证",
            Deny::NoGrant => "在这个 agent 上没有权限",
            Deny::TokenOtherAgent => "这个 token 绑定在另一个 agent 上",
            Deny::TokenScope => "这个 token 只有读权限",
            Deny::TokenCannotManage => "管理动作不能用 token，请用登录会话",
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

/// **唯一的授权判定**。纯函数：同样的输入永远同样的输出，没有 IO。
///
/// 规则（按序）：
///  1. token 封顶：绑定不符 / 越权 scope / 想做 Manage —— 先拒。token 是上界，不是权力来源。
///  2. 站点管理员：放行（仍受 1 的封顶）。
///  3. owner：放行。
///  4. 显式成员：按角色放行。
///  5. public：只放行 Read（匿名也算）。
///  6. 其余一律拒绝 —— 默认拒绝，不是默认允许。
pub fn decide(caller: &Caller, agent: &AgentAcl, action: Action) -> Decision {
    // 1. token 上界。放在最前面：管理员拿只读 token 也只能读（交集，不是取最大）。
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

    // 2..4 都要先有身份。匿名只能走到第 5 条。
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

    // 5. public 只换来读权限 —— 公开一个 agent 不等于让人往里写。
    if agent.visibility == Visibility::Public && action == Action::Read {
        return Decision::Allow;
    }

    // 6. 默认拒绝。
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
            members: vec![],
        }
    }

    fn private_agent() -> AgentAcl {
        AgentAcl {
            name: "secret".into(),
            owner: Some("alice".into()),
            visibility: Visibility::Private,
            members: vec![("bob".into(), Role::Read), ("carol".into(), Role::Write), ("dave".into(), Role::Admin)],
        }
    }

    // ── 匿名 ──

    #[test]
    fn anonymous_reads_public_only() {
        let a = Caller::anonymous();
        assert!(decide(&a, &public_agent(), Action::Read).allowed());
        assert_eq!(decide(&a, &private_agent(), Action::Read), Decision::Deny(Deny::Anonymous));
    }

    #[test]
    fn anonymous_never_writes_even_public() {
        // public = 可读，不是可写。老 Hub 的"读开放"闸门不该顺手把写也放了。
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
        // 迁移过来的老仓库没有 owner：除了站点管理员，谁都不该看见。
        let orphan = AgentAcl::unowned("orphan");
        assert_eq!(decide(&Caller::user("alice"), &orphan, Action::Read), Decision::Deny(Deny::NoGrant));
        assert_eq!(decide(&Caller::anonymous(), &orphan, Action::Read), Decision::Deny(Deny::Anonymous));
    }

    // ── 成员角色 ──

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

    // ── token 上界：这是"token 不再是全站通行证"的地方 ──

    #[test]
    fn token_bound_to_one_agent_cannot_touch_another() {
        // 这条就是老模型的病灶：一个 token = 整个 host。
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
        // 交集，不是取最大：管理员身份不能把 token 的 scope 撑大。
        let root = Caller::admin("root").with_token(None, Scope::Read);
        assert!(decide(&root, &private_agent(), Action::Read).allowed());
        assert_eq!(decide(&root, &private_agent(), Action::Write), Decision::Deny(Deny::TokenScope));
    }

    #[test]
    fn token_never_manages() {
        // 删库/改可见性必须是本人登录会话 —— 泄一个 CI token 不该把库删了。
        for caller in [
            Caller::user("alice").with_token(None, Scope::Write),
            Caller::admin("root").with_token(None, Scope::Write),
        ] {
            assert_eq!(decide(&caller, &private_agent(), Action::Manage), Decision::Deny(Deny::TokenCannotManage));
        }
    }

    #[test]
    fn token_does_not_grant_what_the_user_lacks() {
        // 写 token 落在只读成员手里 —— 还是只读。token 是上界，不是权力来源。
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

    // ── 角色/可见性解析 ──

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

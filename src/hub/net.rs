//! HTTP 边界上的**纯解析**：git smart-http 路径 → (哪个 agent, 读还是写)，以及代理后的真实客户端 IP。
//!
//! 这两件事都必须是纯函数、可穷举测试 —— 它们决定"授权判定拿到的是哪个 agent""限流数的是谁"。
//! 老代码在这两处都没有函数：路径判断是行内的 `path.contains(".git/")`，限流数的是 raw peer IP。

use super::acl::Action;
use std::net::IpAddr;

/// 一次 git smart-http 请求打到哪个 agent、要什么权限。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitRoute {
    pub agent: String,
    pub action: Action,
}

/// agent 名规则：只允许 [A-Za-z0-9._-]，禁止 `..`、前导 `.`、路径分隔符与 NUL。
pub fn valid_agent_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !name.starts_with('.')
        && !name.contains("..")
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// smart-http 路径 → 路由。`/<name>.git/...` 之外的一律 None（不是 git 请求）。
///
/// 这是**授权点的输入**：老代码只问"路径里有没有 `.git/`"，然后连着 `GIT_HTTP_EXPORT_ALL=1`
/// 把请求丢给 http-backend —— 于是"读闸"一过，root 下任何库都能拉。要授权就必须先知道**是哪个库**。
pub fn parse_git_path(path: &str, query: &str) -> Option<GitRoute> {
    let rest = path.strip_prefix('/')?;
    let (first, tail) = rest.split_once('/')?; // `/x.git` 自己不是 git 端点，必须有后续段
    let name = first.strip_suffix(".git")?;
    if !valid_agent_name(name) || tail.is_empty() {
        return None;
    }
    let action = match needs_write(path, query) {
        true => Action::Write,
        false => Action::Read,
    };
    Some(GitRoute { agent: name.to_string(), action })
}

/// 这次请求会不会**写**仓库。
///
/// 判据故意保守：路径命中 `git-receive-pack`，**或** query（百分号解码后）里出现
/// `git-receive-pack` 都算写。宁可多要一次写权限，也不能把一次 push 当成读放过去 ——
/// 误判方向只允许朝"要求更高权限"。
fn needs_write(path: &str, query: &str) -> bool {
    path.ends_with("/git-receive-pack") || percent_decode_lossy(query).contains("git-receive-pack")
}

/// 百分号解码。解不出来的转义原样留着 —— 这里只为"看看里面提没提 receive-pack"，不追求还原。
fn percent_decode_lossy(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Some(v) = hex_pair(b[i + 1], b[i + 2]) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_pair(a: u8, b: u8) -> Option<u8> {
    let d = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    };
    Some(d(a)? << 4 | d(b)?)
}

/// 限流该数谁的账。
///
/// 默认（没声明可信代理）：**就用 raw peer IP**，和老行为一致 —— 任何人都能伪造
/// `X-Forwarded-For`，直连场景信它等于把 per-IP 上限白送。
/// 声明了可信代理后：peer 是代理时，从 XFF 右往左取第一个"不是可信代理"的地址 ——
/// 右边那个是最近的（可信的）代理刚加的，左边的可以由客户端随便伪造。
pub fn client_ip(peer: IpAddr, xff: Option<&str>, trusted: &[IpAddr]) -> IpAddr {
    if !trusted.contains(&peer) {
        return peer;
    }
    let Some(xff) = xff else {
        return peer;
    };
    for hop in xff.rsplit(',') {
        // 解析不了 = XFF 被写坏/被投毒：不猜，退回 peer（代价是这些请求共用代理的配额，
        // 保守但不会把限流白送出去）。
        let Ok(ip) = hop.trim().parse::<IpAddr>() else {
            return peer;
        };
        if !trusted.contains(&ip) {
            return ip;
        }
    }
    peer // 整条链都是可信代理 —— 数代理自己
}

/// `--trusted-proxy 10.0.0.1,10.0.0.2` → IP 表。认不出来的项报错（别静默忽略一个写错的
/// 代理地址 —— 那会让人以为限流按真实 IP 走，其实没有）。
pub fn parse_trusted_proxies(s: &str) -> Result<Vec<IpAddr>, String> {
    s.split(',')
        .map(|x| x.trim())
        .filter(|x| !x.is_empty())
        .map(|x| x.parse::<IpAddr>().map_err(|_| format!("不是合法 IP: {x}")))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    // ── agent 名 ──

    #[test]
    fn agent_name_rejects_traversal_and_seps() {
        assert!(valid_agent_name("alice"));
        assert!(valid_agent_name("team-store_2"));
        assert!(valid_agent_name("a.b"));
        assert!(!valid_agent_name(""));
        assert!(!valid_agent_name(".."));
        assert!(!valid_agent_name("../etc/passwd"));
        assert!(!valid_agent_name("a/b"));
        assert!(!valid_agent_name(".hidden"));
        assert!(!valid_agent_name("a..b"));
        assert!(!valid_agent_name("a\0b"));
        assert!(!valid_agent_name(&"x".repeat(65)));
    }

    // ── git 路径 → 路由 ──

    #[test]
    fn fetch_paths_need_read() {
        let r = parse_git_path("/alice.git/info/refs", "service=git-upload-pack").unwrap();
        assert_eq!(r, GitRoute { agent: "alice".into(), action: Action::Read });
        assert_eq!(parse_git_path("/alice.git/git-upload-pack", "").unwrap().action, Action::Read);
        // 哑协议路径也是读
        assert_eq!(parse_git_path("/alice.git/HEAD", "").unwrap().action, Action::Read);
        assert_eq!(parse_git_path("/alice.git/objects/info/packs", "").unwrap().action, Action::Read);
    }

    #[test]
    fn push_paths_need_write() {
        assert_eq!(parse_git_path("/alice.git/git-receive-pack", "").unwrap().action, Action::Write);
        // info/refs?service=git-receive-pack 是 push 的第一步 —— 它也必须要写权限，
        // 否则只读的人能靠它探到私有分支布局。
        assert_eq!(
            parse_git_path("/alice.git/info/refs", "service=git-receive-pack").unwrap().action,
            Action::Write
        );
    }

    #[test]
    fn percent_encoded_receive_pack_still_needs_write() {
        // http-backend 自己会解码 query。这里不解码就会把一次 push 当成读放过去。
        let r = parse_git_path("/alice.git/info/refs", "service=git%2Dreceive%2Dpack").unwrap();
        assert_eq!(r.action, Action::Write);
        let r = parse_git_path("/alice.git/info/refs", "service=git%2dreceive%2dpack").unwrap();
        assert_eq!(r.action, Action::Write);
    }

    #[test]
    fn write_classification_errs_toward_write() {
        // 多塞一个参数骗不到"读"这一档。
        let r = parse_git_path("/alice.git/info/refs", "a=1&service=git-receive-pack&b=2").unwrap();
        assert_eq!(r.action, Action::Write);
    }

    #[test]
    fn non_git_paths_are_not_git_routes() {
        assert_eq!(parse_git_path("/api/agents", ""), None);
        assert_eq!(parse_git_path("/", ""), None);
        assert_eq!(parse_git_path("/alice", ""), None);
        assert_eq!(parse_git_path("/alice.git", ""), None); // 没有后续段
        assert_eq!(parse_git_path("/alice.git/", ""), None);
    }

    #[test]
    fn traversal_in_git_path_is_refused() {
        // 老代码的 `path.contains(".git/")` 会把这些一路送进 http-backend。
        assert_eq!(parse_git_path("/../etc.git/info/refs", ""), None);
        assert_eq!(parse_git_path("/..%2f.git/info/refs", ""), None);
        assert_eq!(parse_git_path("/a/b.git/info/refs", ""), None, "只认根下的 <name>.git");
        assert_eq!(parse_git_path("/.hidden.git/info/refs", ""), None);
        assert_eq!(parse_git_path("/a..b.git/info/refs", ""), None);
    }

    #[test]
    fn nested_repo_path_cannot_escape_to_another_repo() {
        // `/alice.git/../bob.git/info/refs` —— 名字取的是第一段，`..` 会被 valid_agent_name 挡住；
        // 而且 HTTP 层还有一道 `..` 总闸。这里确认解析本身不会把它认成 alice。
        assert_eq!(parse_git_path("/alice.git/../bob.git/info/refs", "").unwrap().agent, "alice");
    }

    #[test]
    fn percent_decode_basics() {
        assert_eq!(percent_decode_lossy("a%2Db"), "a-b");
        assert_eq!(percent_decode_lossy("a+b"), "a+b");
        assert_eq!(percent_decode_lossy("%zz"), "%zz");
        assert_eq!(percent_decode_lossy("%2"), "%2");
        assert_eq!(percent_decode_lossy(""), "");
    }

    // ── 代理后的真实 IP ──

    #[test]
    fn without_trusted_proxy_xff_is_ignored() {
        // 谁都能伪造 XFF。没声明代理就必须按 raw peer 算，否则 per-IP 上限等于没有。
        let peer = ip("203.0.113.9");
        assert_eq!(client_ip(peer, Some("1.2.3.4"), &[]), peer);
        assert_eq!(client_ip(peer, Some("1.2.3.4, 5.6.7.8"), &[ip("10.0.0.1")]), peer);
    }

    #[test]
    fn trusted_proxy_yields_the_real_client() {
        // 代理后每个用户共用一个 peer IP —— 不看 XFF 的话他们会互相把对方挤下线。
        let proxy = ip("10.0.0.1");
        let trusted = vec![proxy];
        assert_eq!(client_ip(proxy, Some("203.0.113.9"), &trusted), ip("203.0.113.9"));
        assert_eq!(client_ip(proxy, Some("203.0.113.9, 10.0.0.1"), &trusted), ip("203.0.113.9"));
    }

    #[test]
    fn spoofed_left_hand_hops_are_ignored() {
        // 客户端自己塞的 XFF 会排在左边；最右那个才是可信代理刚加的。
        let proxy = ip("10.0.0.1");
        let trusted = vec![proxy];
        assert_eq!(client_ip(proxy, Some("6.6.6.6, 203.0.113.9"), &trusted), ip("203.0.113.9"));
    }

    #[test]
    fn chain_of_trusted_proxies_walks_left() {
        let (p1, p2) = (ip("10.0.0.1"), ip("10.0.0.2"));
        let trusted = vec![p1, p2];
        assert_eq!(client_ip(p1, Some("203.0.113.9, 10.0.0.2"), &trusted), ip("203.0.113.9"));
        // 整条链都是代理 → 数代理自己
        assert_eq!(client_ip(p1, Some("10.0.0.2, 10.0.0.1"), &trusted), p1);
    }

    #[test]
    fn junk_xff_falls_back_to_peer() {
        let proxy = ip("10.0.0.1");
        let trusted = vec![proxy];
        assert_eq!(client_ip(proxy, Some("not-an-ip"), &trusted), proxy);
        assert_eq!(client_ip(proxy, Some(""), &trusted), proxy);
        assert_eq!(client_ip(proxy, None, &trusted), proxy);
        assert_eq!(client_ip(proxy, Some("203.0.113.9, bogus"), &trusted), proxy);
    }

    #[test]
    fn trusted_proxy_parsing() {
        assert_eq!(parse_trusted_proxies("10.0.0.1").unwrap(), vec![ip("10.0.0.1")]);
        assert_eq!(parse_trusted_proxies("10.0.0.1, ::1").unwrap(), vec![ip("10.0.0.1"), ip("::1")]);
        assert!(parse_trusted_proxies("").unwrap().is_empty());
        assert!(parse_trusted_proxies("10.0.0.1,nope").is_err(), "写错的代理地址要报错，不能静默忽略");
    }
}

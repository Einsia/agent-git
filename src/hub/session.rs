//! 浏览器会话（cookie）—— 给人用的认证。脚本/git 走 token，不碰这里。
//!
//! 设计取舍：**不签名，用不可猜的随机 id + 服务端表**。签名 cookie（JWT 那套）省一张表，
//! 代价是登出/踢人做不到（签名还在有效期内就一直有效）。会话表在内存里：进程重启 = 全体重登，
//! 这是可接受的，换来的是"撤销立即生效"和"服务器上没有一份可离线爆破的东西"。
//!
//! 表里存的是 sid 的 **sha256**，不是 sid 本身 —— 内存被 dump（core dump / swap）也拿不到能用的 cookie。

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

/// cookie 名。带 `__Host-` 前缀本来更好（浏览器强制 Secure+Path=/+无 Domain），
/// 但那要求必须是 HTTPS，而本地 http://localhost 开发是主路径 —— 会话直接失效。
pub const COOKIE: &str = "agit_session";

/// 会话有效期。够一个工作日，不够久到"忘了登出就一直开着"。
pub const TTL: Duration = Duration::from_secs(12 * 3600);

/// 会话总数上限。挡住"无限登录 → 无限内存"。到顶时先清过期的，还满就拒绝。
const MAX_SESSIONS: usize = 4096;

struct Sess {
    user: String,
    expires: SystemTime,
}

#[derive(Default)]
pub struct Sessions {
    inner: Mutex<HashMap<String, Sess>>,
}

impl Sessions {
    pub fn new() -> Sessions {
        Sessions::default()
    }

    /// 发一个新会话，返回**明文 sid**（只此一次，之后服务端只有它的摘要）。
    pub fn create(&self, user: &str) -> std::io::Result<String> {
        let sid = super::kdf::gen_secret()?; // 32 字节 CSPRNG = 256 bit，猜不动
        let mut m = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if m.len() >= MAX_SESSIONS {
            let now = SystemTime::now();
            m.retain(|_, s| s.expires > now);
            if m.len() >= MAX_SESSIONS {
                return Err(std::io::Error::other("会话数到上限，请稍后再试"));
            }
        }
        m.insert(
            crate::convo::sha256_hex(&sid),
            Sess { user: user.to_string(), expires: SystemTime::now() + TTL },
        );
        Ok(sid)
    }

    /// sid → 用户名。过期的当场清掉并返回 None。
    pub fn lookup(&self, sid: &str) -> Option<String> {
        let key = crate::convo::sha256_hex(sid);
        let mut m = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let s = m.get(&key)?;
        if s.expires <= SystemTime::now() {
            m.remove(&key);
            return None;
        }
        Some(s.user.clone())
    }

    /// 登出：撤销立即生效。
    pub fn revoke(&self, sid: &str) {
        let key = crate::convo::sha256_hex(sid);
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).remove(&key);
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.inner.lock().unwrap().len()
    }
}

/// `Set-Cookie` 的值。
///   HttpOnly —— JS 读不到，XSS 偷不走会话
///   SameSite=Lax —— 挡跨站 POST（CSRF）；Lax 仍允许顶层导航带上 cookie，不影响点链接进来
///   Secure —— 只在 TLS 下加；本地 http 开发加了会让浏览器直接丢掉这个 cookie
///   Max-Age —— 和服务端 TTL 对齐
pub fn set_cookie(sid: &str, secure: bool) -> String {
    format!(
        "{COOKIE}={sid}; HttpOnly; SameSite=Lax; Path=/; Max-Age={}{}",
        TTL.as_secs(),
        if secure { "; Secure" } else { "" }
    )
}

/// 登出用：同名空值 + Max-Age=0，让浏览器立刻扔掉。
pub fn clear_cookie(secure: bool) -> String {
    format!("{COOKIE}=; HttpOnly; SameSite=Lax; Path=/; Max-Age=0{}", if secure { "; Secure" } else { "" })
}

/// 从 `Cookie:` 头里抠出 sid。格式是 `a=1; b=2`。
pub fn parse_cookie(header: &str) -> Option<String> {
    header.split(';').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k.trim() == COOKIE).then(|| v.trim().to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_lookup_revoke() {
        let s = Sessions::new();
        let sid = s.create("alice").unwrap();
        assert_eq!(s.lookup(&sid).as_deref(), Some("alice"));
        s.revoke(&sid);
        assert_eq!(s.lookup(&sid), None, "登出后立刻失效");
    }

    #[test]
    fn sids_are_unique_and_unguessable() {
        let s = Sessions::new();
        let a = s.create("alice").unwrap();
        let b = s.create("alice").unwrap();
        assert_ne!(a, b);
        assert_eq!(a.len(), 64, "256 bit hex");
    }

    #[test]
    fn plaintext_sid_is_not_stored() {
        // 表里只该有摘要 —— 内存/core dump 里捞不到能直接用的 cookie。
        let s = Sessions::new();
        let sid = s.create("alice").unwrap();
        let m = s.inner.lock().unwrap();
        assert!(!m.contains_key(&sid));
        assert!(m.contains_key(&crate::convo::sha256_hex(&sid)));
    }

    #[test]
    fn unknown_sid_is_rejected() {
        let s = Sessions::new();
        s.create("alice").unwrap();
        assert_eq!(s.lookup("deadbeef"), None);
        assert_eq!(s.lookup(""), None);
    }

    #[test]
    fn expired_session_is_rejected_and_dropped() {
        let s = Sessions::new();
        let sid = "manual";
        s.inner.lock().unwrap().insert(
            crate::convo::sha256_hex(sid),
            Sess { user: "alice".into(), expires: SystemTime::now() - Duration::from_secs(1) },
        );
        assert_eq!(s.lookup(sid), None);
        assert_eq!(s.len(), 0, "过期的顺手清掉");
    }

    #[test]
    fn sessions_are_independent() {
        let s = Sessions::new();
        let a = s.create("alice").unwrap();
        let b = s.create("bob").unwrap();
        s.revoke(&a);
        assert_eq!(s.lookup(&a), None);
        assert_eq!(s.lookup(&b).as_deref(), Some("bob"), "撤一个不该影响别人");
    }

    #[test]
    fn cookie_flags() {
        let c = set_cookie("abc", false);
        assert!(c.contains("HttpOnly"));
        assert!(c.contains("SameSite=Lax"));
        assert!(c.contains("Max-Age=43200"));
        assert!(!c.contains("Secure"), "无 TLS 时不能标 Secure，否则浏览器直接丢掉");
        assert!(set_cookie("abc", true).contains("; Secure"));
        assert!(clear_cookie(false).contains("Max-Age=0"));
    }

    #[test]
    fn cookie_parsing() {
        assert_eq!(parse_cookie("agit_session=abc").as_deref(), Some("abc"));
        assert_eq!(parse_cookie("x=1; agit_session=abc; y=2").as_deref(), Some("abc"));
        assert_eq!(parse_cookie(" agit_session = abc ").as_deref(), Some("abc"));
        assert_eq!(parse_cookie("other=abc"), None);
        assert_eq!(parse_cookie(""), None);
        // 别把 agit_session_x 当成 agit_session
        assert_eq!(parse_cookie("agit_session_other=abc"), None);
    }
}

//! 只追加的审计日志：谁、什么时候、对哪个 agent、做了什么。
//!
//! JSONL，一行一条，`O_APPEND` 打开 —— 内核保证每次 write 落在文件末尾，多线程并发追加
//! 不会互相截断（前提是一次 write 写完一整行，所以这里先把整行拼好再写一次）。
//!
//! 只追加意味着**没有删除接口**：轮转/归档交给外面的 logrotate。Hub 自己不会回头改它。
//! 被拒绝的请求也记 —— "谁试过但没进去"往往比"谁进去了"更有价值。

use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};

/// 查询时最多回看的字节数。日志无限长，但"最近发生了什么"只需要看尾巴。
const TAIL_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub when: String,
    /// 用户名；匿名请求写 "anonymous"。
    pub actor: String,
    /// 动作名，见下面的常量。
    pub action: String,
    /// 相关 agent；跟具体 agent 无关的动作（login 等）为 null。
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub detail: String,
}

// 动作名 —— 固定字符串，前端/grep 都靠它。
pub const LOGIN: &str = "login";
pub const LOGIN_FAILED: &str = "login.failed";
pub const LOGOUT: &str = "logout";
pub const USER_ADD: &str = "user.add";
pub const AGENT_CREATE: &str = "agent.create";
pub const AGENT_DELETE: &str = "agent.delete";
pub const AGENT_RENAME: &str = "agent.rename";
pub const AGENT_VISIBILITY: &str = "agent.visibility";
pub const MEMBER_ADD: &str = "member.add";
pub const MEMBER_REMOVE: &str = "member.remove";
pub const TOKEN_CREATE: &str = "token.create";
pub const TOKEN_REVOKE: &str = "token.revoke";
pub const GIT_FETCH: &str = "git.fetch";
pub const GIT_PUSH: &str = "git.push";
pub const DENIED: &str = "denied";

pub fn log_path(root: &Path) -> PathBuf {
    root.join("audit.log")
}

/// 追加一条。写失败**不打断请求**（审计坏了不该让 Hub 罢工），但会往 stderr 喊一声。
pub fn append(root: &Path, actor: &str, action: &str, agent: Option<&str>, detail: &str) {
    let e = Entry {
        when: super::store::now_iso(),
        actor: actor.to_string(),
        action: action.to_string(),
        agent: agent.map(|s| s.to_string()),
        detail: detail.to_string(),
    };
    if let Err(err) = try_append(root, &e) {
        eprintln!("审计日志写不进去（{}）: {err}", log_path(root).display());
    }
}

fn try_append(root: &Path, e: &Entry) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    super::store::ensure_root(root)?;
    // 一次拼好一整行再写一次：O_APPEND + 单次 write = 并发追加不交错。
    let mut line = serde_json::to_string(e).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    line.push('\n');
    let mut f = std::fs::OpenOptions::new().append(true).create(true).mode(0o600).open(log_path(root))?;
    f.write_all(line.as_bytes())
}

/// 读尾巴：按 agent 过滤，最多 limit 条，最新的在前。
pub fn query(root: &Path, agent: Option<&str>, limit: usize) -> Vec<Entry> {
    let Some(text) = read_tail(&log_path(root)) else {
        return vec![];
    };
    let mut out: Vec<Entry> = text
        .lines()
        .filter_map(|l| serde_json::from_str::<Entry>(l).ok())
        .filter(|e| match agent {
            Some(a) => e.agent.as_deref() == Some(a),
            None => true,
        })
        .collect();
    out.reverse(); // 最新的在前
    out.truncate(limit);
    out
}

/// 只读末尾 TAIL_BYTES，并丢掉可能被切断的第一行。日志涨到几个 G 也不会把它整个读进内存。
fn read_tail(path: &Path) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let truncated = len > TAIL_BYTES;
    if truncated {
        f.seek(SeekFrom::Start(len - TAIL_BYTES)).ok()?;
    }
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf).into_owned();
    match truncated {
        // 从中间切进去的第一行多半是半截，丢掉。
        true => text.split_once('\n').map(|(_, rest)| rest.to_string()),
        false => Some(text),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appends_and_queries_newest_first() {
        let d = tempfile::tempdir().unwrap();
        append(d.path(), "alice", AGENT_CREATE, Some("x"), "visibility=private");
        append(d.path(), "bob", GIT_PUSH, Some("y"), "");
        append(d.path(), "alice", LOGIN, None, "");

        let all = query(d.path(), None, 10);
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].action, LOGIN, "最新的在前");
        assert_eq!(all[2].action, AGENT_CREATE);

        let only_x = query(d.path(), Some("x"), 10);
        assert_eq!(only_x.len(), 1);
        assert_eq!(only_x[0].actor, "alice");
        assert_eq!(only_x[0].detail, "visibility=private");

        assert_eq!(query(d.path(), None, 2).len(), 2, "limit 生效");
        assert!(query(d.path(), Some("nope"), 10).is_empty());
    }

    #[test]
    fn log_file_is_0600() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        append(d.path(), "alice", LOGIN, None, "");
        let mode = std::fs::metadata(log_path(d.path())).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn append_only_never_rewrites_history() {
        let d = tempfile::tempdir().unwrap();
        append(d.path(), "alice", LOGIN, None, "one");
        let first = std::fs::read_to_string(log_path(d.path())).unwrap();
        append(d.path(), "bob", LOGIN, None, "two");
        let second = std::fs::read_to_string(log_path(d.path())).unwrap();
        assert!(second.starts_with(&first), "老行必须原样留在开头");
        assert_eq!(second.lines().count(), 2);
    }

    #[test]
    fn missing_log_is_empty_not_an_error() {
        let d = tempfile::tempdir().unwrap();
        assert!(query(d.path(), None, 10).is_empty());
    }

    #[test]
    fn detail_with_newline_cannot_forge_a_row() {
        // detail 是用户可控的（agent 名、用户名）。JSON 编码会把换行转义掉,
        // 否则 "\n{...}" 就能往审计里伪造一整条记录。
        let d = tempfile::tempdir().unwrap();
        append(d.path(), "eve", LOGIN, None, "x\n{\"actor\":\"root\",\"action\":\"login\"}");
        let raw = std::fs::read_to_string(log_path(d.path())).unwrap();
        assert_eq!(raw.lines().count(), 1, "一条就是一条，不能被换行拆成两条");
        let q = query(d.path(), None, 10);
        assert_eq!(q.len(), 1);
        assert_eq!(q[0].actor, "eve");
    }
}

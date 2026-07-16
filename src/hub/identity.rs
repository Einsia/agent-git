//! 从 store 里读 agent 身份 —— `agent.toml`。
//!
//! aid（`agt_<uuid>`）是**客户端铸造、提交进 store 历史**的：它跟着仓库走，改名、换 Hub、
//! 换机器都不变。Hub 不铸造 aid，只是把 `git show <ref>:agent.toml` 的结果读出来。
//! 这意味着一个刚 `POST /api/agents` 建出来、还没人推过东西的空库**没有 aid** —— 那就老实报 null。
//!
//! 只认 `agt_` 前缀。老 scaffold 写的是 `id = "unnamed-agent"`（见 `crate::init::scaffold`），
//! **每个 store 都是这个值** —— 把它当身份会让所有老库共享一个"身份"，比没有身份更糟。

/// agent.toml 里读出来的身份。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Identity {
    /// 有正经 aid。
    Aid(String),
    /// 有 agent.toml，但没有 `agt_` 身份（老 store / 手写的占位值）。
    Unidentified,
}

/// 解析 agent.toml 取身份。容忍两种写法：
///   `[agent]` 段里的 `id`（新格式），和顶层的 `id`（老 scaffold）。
pub fn parse_agent_toml(text: &str) -> Identity {
    let id = toml_string(text, Some("agent"), "id").or_else(|| toml_string(text, None, "id"));
    match id {
        Some(v) if is_aid(&v) => Identity::Aid(v),
        _ => Identity::Unidentified,
    }
}

/// aid 形状闸：`agt_` + 非空、只含 [A-Za-z0-9-]。这串会进 JSON、进日志，得先当不可信输入验一遍。
pub fn is_aid(s: &str) -> bool {
    let Some(rest) = s.strip_prefix("agt_") else {
        return false;
    };
    !rest.is_empty()
        && rest.len() <= 64
        && rest.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// 极简 TOML 取值：只够读 `key = "value"`。不引 toml 依赖 —— Hub 只需要认一个字符串键，
/// 为它拉一整个解析器不划算。认不出来就返回 None（然后调用方报 null，不猜）。
///
/// `section` = None 表示取顶层（第一个 `[...]` 之前）的键。
fn toml_string(text: &str, section: Option<&str>, key: &str) -> Option<String> {
    let mut cur: Option<String> = None;
    for line in text.lines() {
        let line = strip_comment(line).trim();
        if line.is_empty() {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            cur = Some(name.trim().to_string());
            continue;
        }
        if cur.as_deref() != section {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        if k.trim() != key {
            continue;
        }
        let v = v.trim();
        // 只认双引号/单引号包起来的字符串。
        for q in ['"', '\''] {
            if let Some(inner) = v.strip_prefix(q).and_then(|s| s.strip_suffix(q)) {
                return Some(inner.to_string());
            }
        }
        return None;
    }
    None
}

/// 丢掉 `#` 之后的注释。引号里的 `#` 不算 —— `id = "agt_#1"` 不该被切断。
fn strip_comment(line: &str) -> &str {
    let b = line.as_bytes();
    let (mut in_s, mut in_d) = (false, false);
    for i in 0..b.len() {
        match b[i] {
            b'\'' if !in_d => in_s = !in_s,
            b'"' if !in_s => in_d = !in_d,
            b'#' if !in_s && !in_d => return &line[..i],
            _ => {}
        }
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_new_layout() {
        let t = r#"
# Agent 身份
[agent]
id = "agt_9f1c2d3e-4b5a-6c7d-8e9f-0a1b2c3d4e5f"
name = "reviewer"
created = "2026-07-16T10:00:00Z"
"#;
        assert_eq!(
            parse_agent_toml(t),
            Identity::Aid("agt_9f1c2d3e-4b5a-6c7d-8e9f-0a1b2c3d4e5f".into())
        );
    }

    #[test]
    fn reads_top_level_id_too() {
        // 老 store 可能把 id 写在顶层 —— 两种布局都要认。
        assert_eq!(parse_agent_toml("id = \"agt_abc123\"\n"), Identity::Aid("agt_abc123".into()));
    }

    #[test]
    fn agent_section_wins_over_top_level() {
        let t = "id = \"agt_old\"\n[agent]\nid = \"agt_new\"\n";
        assert_eq!(parse_agent_toml(t), Identity::Aid("agt_new".into()));
    }

    #[test]
    fn legacy_placeholder_is_not_an_identity() {
        // crate::init::scaffold 给**每个** store 都写这一行。当身份用 = 所有库同名。
        assert_eq!(parse_agent_toml("# Agent 身份\nid = \"unnamed-agent\"\n"), Identity::Unidentified);
        assert_eq!(parse_agent_toml("[agent]\nid = \"unnamed-agent\"\n"), Identity::Unidentified);
    }

    #[test]
    fn missing_or_junk_is_unidentified() {
        assert_eq!(parse_agent_toml(""), Identity::Unidentified);
        assert_eq!(parse_agent_toml("[agent]\nname = \"x\"\n"), Identity::Unidentified);
        assert_eq!(parse_agent_toml("id = agt_unquoted\n"), Identity::Unidentified);
        assert_eq!(parse_agent_toml("这不是 toml"), Identity::Unidentified);
        assert_eq!(parse_agent_toml("[other]\nid = \"agt_x\"\n"), Identity::Unidentified);
    }

    #[test]
    fn aid_shape_is_enforced() {
        assert!(is_aid("agt_abc-123"));
        assert!(!is_aid("agt_"));
        assert!(!is_aid("unnamed-agent"));
        assert!(!is_aid("AGT_abc"));
        // 这串要进 JSON / 日志：形状不对的一律不认
        assert!(!is_aid("agt_a/b"));
        assert!(!is_aid("agt_a b"));
        assert!(!is_aid("agt_\"x"));
        assert!(!is_aid(&format!("agt_{}", "x".repeat(65))));
    }

    #[test]
    fn trailing_comment_is_stripped() {
        assert_eq!(parse_agent_toml("id = \"agt_abc\"  # 这是身份\n"), Identity::Aid("agt_abc".into()));
        assert_eq!(parse_agent_toml("# id = \"agt_x\"\n"), Identity::Unidentified, "注释掉的键不算数");
    }

    #[test]
    fn comment_stripper_respects_quotes() {
        // 引号里的 `#` 不是注释起点。（这里的值过不了 is_aid 的形状闸，所以直接测切注释这一步。）
        assert_eq!(strip_comment("id = \"a#b\"  # 注释").trim(), "id = \"a#b\"");
        assert_eq!(strip_comment("id = 'a#b'").trim(), "id = 'a#b'");
        assert_eq!(strip_comment("# 整行注释"), "");
        assert_eq!(strip_comment("no comment here"), "no comment here");
    }

    #[test]
    fn single_quotes_work() {
        assert_eq!(parse_agent_toml("[agent]\nid = 'agt_abc'\n"), Identity::Aid("agt_abc".into()));
    }
}

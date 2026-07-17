//! 密钥扫描。
//!
//! 关键认知：**这道防线之所以必须存在，恰恰是因为 context 会被 push 和 clone。**
//! Shepherd / Zed / Claude Code 都没有它，不是疏忽 —— 是它们不分享 context。
//!
//! 扫描范围必须同时覆盖 claim 正文**和证据快照**，因为证据会把源文件内容抄进来。
//!
//! 两条设计线:
//!  1. **能校验就校验**:GitHub token 的 CRC32、JWT 头部能否解出带 alg 的 JSON —— 校验不过就不报。
//!  2. **熵只在"值"上跑**:session dump 是 JSONL,把它当 JSON 解析、只扫**字符串值**,再按字段名和
//!     shape(uuid / sha / 路径 / 时间戳 / 内容寻址哈希)挡掉已知噪声。
//!     旧代码对整行跑熵 —— 行里混着 JSON 结构、路径、requestId,3096 份真实 session 能炸出 38 万条,
//!     于是只能对 jsonl 把熵关掉,"不匹配任何已知格式的凭据"就此漏过。这里把它修好。

use anyhow::Result;
use once_cell::sync::Lazy;
use regex::Regex;
use std::path::Path;

pub struct Finding {
    pub rule: &'static str,
    pub line: usize,
    pub excerpt: String,
}

/// 行内豁免:这一行(jsonl 里即这一条记录)上出现该字面量就整行跳过。
/// 没有豁免口子的闸门,最后一定会被 `--no-verify` 整个绕过 —— 那才是真正的失败模式。
pub const ALLOW_PRAGMA: &str = "agit:allow-secret";

/// 仓库级豁免清单文件名(放在被扫的根下)。每行一条:字面子串,或 `re:` 开头的正则。
pub const ALLOW_FILE: &str = ".agit-allow-secrets";

struct Rule {
    name: &'static str,
    re: Regex,
    /// 越大越具体。命中重叠时只保留最具体的那条 —— 否则 `sk-ant-…` 会同时被
    /// anthropic-key 和宽松的 openai-key 命中,凭空把数字翻倍。
    spec: u8,
    /// 结构校验:返回 false 表示"长得像但不是"。
    validate: Option<fn(&str) -> bool>,
}

static RULES: Lazy<Vec<Rule>> = Lazy::new(|| {
    let r = |name, spec, pat: &str| Rule {
        name,
        re: Regex::new(pat).expect("built-in rule regex must compile"),
        spec,
        validate: None,
    };
    let v = |name, spec, pat: &str, f: fn(&str) -> bool| Rule {
        name,
        re: Regex::new(pat).expect("built-in rule regex must compile"),
        spec,
        validate: Some(f),
    };
    vec![
        // ── 带自校验 ──
        // GitHub: 4 字符前缀 + 30 字符熵 + 6 字符 base62(CRC32(熵))。校验和**只覆盖熵段,不含前缀**。
        v("github-token", 95, r"\bgh[pousr]_[A-Za-z0-9]{36}\b", github_crc_ok),
        v(
            "jwt",
            90,
            r"\beyJ[A-Za-z0-9_-]{6,}\.[A-Za-z0-9_-]{4,}\.[A-Za-z0-9_-]{4,}",
            jwt_header_ok,
        ),
        // fine-grained PAT 也带校验和,但校验范围未见权威说明 —— 不臆造,只认格式。
        r(
            "github-fine-grained-pat",
            95,
            r"\bgithub_pat_[A-Za-z0-9]{22}_[A-Za-z0-9]{59}\b",
        ),
        // ── 高精度前缀 ──
        r("anthropic-key", 92, r"\bsk-ant-[A-Za-z0-9_-]{20,}"),
        r("openai-project-key", 92, r"\bsk-proj-[A-Za-z0-9_-]{20,}"),
        r("openai-key", 50, r"\bsk-[A-Za-z0-9]{20,}\b"),
        // glpat- = personal access token;glrt- = runner token(两者都在真实转录里出现过)。
        r("gitlab-token", 90, r"\bgl(?:pat|rt)-[A-Za-z0-9_-]{20,}"),
        r("google-api-key", 90, r"\bAIza[0-9A-Za-z_-]{35}\b"),
        r("stripe-secret-key", 95, r"\b[sr]k_live_[0-9a-zA-Z]{24,}"),
        r("slack-token", 90, r"\bxox[baprs]-[0-9A-Za-z-]{10,}"),
        r(
            "slack-webhook",
            95,
            r"https://hooks\.slack\.com/services/[A-Za-z0-9_/-]{20,}",
        ),
        r(
            "discord-webhook",
            95,
            r"https://(?:canary\.|ptb\.)?discord(?:app)?\.com/api/webhooks/[0-9]{5,}/[A-Za-z0-9_-]{20,}",
        ),
        r("sendgrid-key", 95, r"\bSG\.[A-Za-z0-9_-]{22}\.[A-Za-z0-9_-]{43}\b"),
        r("npm-token", 90, r"\bnpm_[A-Za-z0-9]{36}\b"),
        // PyPI token = 前缀 + macaroon;`AgEIcHlwaS5vcmc` 是其中 "pypi.org" 那段的固定 base64。
        r("pypi-token", 95, r"\bpypi-AgEIcHlwaS5vcmc[A-Za-z0-9_-]{20,}"),
        r("huggingface-token", 90, r"\bhf_[A-Za-z0-9]{34,}\b"),
        r("twilio-api-key-sid", 80, r"\bSK[0-9a-f]{32}\b"),
        r("cloudflare-token", 90, r"\bcf(?:ut|at|k)_[A-Za-z0-9_-]{40,}"),
        r("datadog-app-key", 90, r"\bddapp_[A-Za-z0-9]{20,}"),
        r(
            "sentry-dsn",
            95,
            r"https://[0-9a-f]{32}(?::[0-9a-f]{32})?@[A-Za-z0-9.-]*sentry\.io/[0-9]+",
        ),
        r("aws-access-key-id", 85, r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b"),
        // GCP service account:JSON 里 private_key 紧挨着 BEGIN 块就是铁证。
        r("gcp-service-account", 96, r#""private_key"\s*:\s*"-----BEGIN"#),
        // ── 值本身没精度,只有被上下文锚定时才报 ──
        r(
            "datadog-api-key",
            85,
            r"(?i)\b(?:dd|datadog)[_-]?(?:api[_-]?)?key\b\W{0,4}\b[0-9a-f]{32}\b",
        ),
        // 结尾不加 `\b`:`TWILIO_AUTH_TOKEN=` 里 `_` 也是词字符,`\btwilio\b` 反而匹配不上。
        r("twilio-auth-token", 85, r"(?i)\btwilio[^\n]{0,40}?\b[0-9a-f]{32}\b"),
        // AWS secret access key 是没有前缀的 40 字符 base64:只有挨着字段名或 AKIA 时才有精度。
        r(
            "aws-secret-access-key",
            88,
            r"(?i)\baws[_-]?secret[_-]?access[_-]?key\b\W{0,4}\b[A-Za-z0-9/+=]{40}\b",
        ),
    ]
});

/// 通用 `scheme://user:pass@host`,含 https 基本认证 —— 旧规则只认 postgres|mysql|… 那几个 scheme。
static CONN_STRING: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b([a-z][a-z0-9+.-]{2,15})://([^\s:@/]{1,64}):([^\s:@/]{1,128})@").unwrap()
});

/// 关键字赋值。**光有正则远远不够**:真实转录里 `password: string`(TS 类型标注)、
/// `token: TOKEN`、`password = process.env.X` 铺天盖地,值必须再过一遍 `value_is_opaque`。
static ASSIGNED: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r#"(?i)\b(password|passwd|pwd|secret|token|api[_-]?key|access[_-]?key|private[_-]?key|client[_-]?secret|auth[_-]?token)\b\s*[:=]\s*["']?([^\s"'#,;)\]}]{6,})"#,
    )
    .unwrap()
});

/// 跨行私钥块。JSON 值里私钥是 `\n` 转义的一整行,解析还原后变成真换行,两种形态都要吃下。
static PK_BEGIN: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"-----BEGIN (?:[A-Z0-9 ]+ )?PRIVATE KEY(?: BLOCK)?-----").unwrap());
static PK_END: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"-----END (?:[A-Z0-9 ]+ )?PRIVATE KEY(?: BLOCK)?-----").unwrap());

// ─────────────────────── 校验:CRC32 / base62 / base64url ───────────────────────

const B62: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

fn b62_decode(s: &str) -> Option<u64> {
    let mut v: u64 = 0;
    for c in s.bytes() {
        let i = B62.iter().position(|&x| x == c)? as u64;
        v = v.checked_mul(62)?.checked_add(i)?;
    }
    Some(v)
}

/// `ghp_` + 30 熵 + 6 位 base62(CRC32(熵))。真 token 必过;示例/抄错/随机串基本必挂。
fn github_crc_ok(tok: &str) -> bool {
    let rest = &tok[4..];
    if rest.len() != 36 {
        return false;
    }
    let (body, chk) = rest.split_at(30);
    b62_decode(chk) == Some(crc32(body.as_bytes()) as u64)
}

fn b64url_decode(s: &str) -> Option<Vec<u8>> {
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    let mut out = Vec::new();
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            b'=' => break,
            _ => return None,
        } as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

/// JWT 必须是三段,且**头部能 base64url 解出带 alg 的 JSON** —— 只看 `eyJ` 前缀会把任何
/// base64 过的 JSON(转录里很多)都当成 token。
fn jwt_header_ok(tok: &str) -> bool {
    let mut parts = tok.split('.');
    let (Some(h), Some(p), Some(s)) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    if p.is_empty() || s.is_empty() || parts.next().is_some() {
        return false;
    }
    let Some(raw) = b64url_decode(h) else {
        return false;
    };
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(&raw) else {
        return false;
    };
    v.get("alg").and_then(|a| a.as_str()).is_some()
}

// ─────────────────────── 熵与 shape 白名单 ───────────────────────

/// Shannon 熵。用来抓那些不匹配任何已知格式、但明显是随机密钥的长串。
fn shannon_entropy(s: &str) -> f64 {
    let n = s.len() as f64;
    if n == 0.0 {
        return 0.0;
    }
    let mut counts = [0usize; 256];
    for b in s.bytes() {
        counts[b as usize] += 1;
    }
    -counts
        .iter()
        .filter(|&&c| c > 0)
        .map(|&c| {
            let p = c as f64 / n;
            p * p.log2()
        })
        .sum::<f64>()
}

/// `=` 只当结尾的 base64 padding,不能出现在中间 —— 否则 `API_KEY=<值>` 会被当成**一个**
/// 候选串,key 和 value 粘在一起,既抬高了熵又让摘要面目全非。
static HIGH_ENTROPY_CANDIDATE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"[A-Za-z0-9+/_-]{24,}={0,2}").unwrap());
static UUID: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$").unwrap()
});
static HEXISH: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)^[0-9a-f]+$").unwrap());
static ISO_TS: Lazy<Regex> = Lazy::new(|| Regex::new(r"^\d{4}-\d{2}-\d{2}T[\d:.-]+Z?$").unwrap());
/// npm / SRI 内容寻址完整性哈希:`sha512-…`。
static INTEGRITY: Lazy<Regex> = Lazy::new(|| Regex::new(r"^sha(?:1|256|384|512)-").unwrap());
/// 全小写词块(可带数字/点/下划线/连字符)—— 标识符、slug、MCP 工具名、路径片段。
static WORDISH: Lazy<Regex> = Lazy::new(|| Regex::new(r"^[a-z0-9._-]+$").unwrap());
/// Anthropic / OpenAI 的对象 ID:`toolu_01…`、`msg_011C…`、`req_011C…`。
/// 它们高熵、大小写数字齐全,而且会**以正文形式**出现在 content/stdout 里(不只是在 id 字段),
/// 所以光靠字段名挡不住 —— 必须按形状挡。这些是对象标识符,永远不是凭据。
static VENDOR_OBJECT_ID: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^(?:toolu|msg|req|chatcmpl|asst|thread|run|call|evt|acct)_[A-Za-z0-9]+$").unwrap()
});
/// base64 过的媒体/文档:PNG、JPEG、GIF、PDF、zip 的 magic。截图是转录里最大的 base64 来源之一。
static B64_MEDIA: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^(?:iVBORw0KGgo|/9j/|R0lGOD|JVBERi0|UEsDB)").unwrap());
/// SSH **公**钥的线格式 base64 头(`AAAAB3NzaC1yc2E` = ssh-rsa,`AAAAC3NzaC1lZDI1NTE5` = ssh-ed25519,
/// `AAAAE2VjZHNhLXNoYTIt` = ecdsa-sha2-)。公钥不是密钥;私钥另有 PEM 块规则兜着。
static SSH_PUBKEY: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^(?:AAAAB3NzaC1yc2E|AAAAC3NzaC1lZDI1NTE5|AAAAE2VjZHNhLXNoYTIt)").unwrap()
});

const ENTROPY_THRESHOLD: f64 = 4.2;
const ENTROPY_MIN_LEN: usize = 24;

/// JSON 里"按字段名就知道是噪声"的键:值再高熵也不是凭据。
/// **`content` / `text` / `command` / `input` 绝不在此列** —— agent `cat` 出来的 .env 正落在那里。
fn noise_field(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    matches!(
        k.as_str(),
        "uuid"
            | "parentuuid"
            | "leafuuid"
            | "sessionid"
            | "session_id"
            | "requestid"
            | "request_id"
            | "id"
            | "messageid"
            | "message_id"
            | "conversationid"
            | "userid"
            | "user_id"
            // Claude 转录里 thinking 块的加密签名:Anthropic 自己的 blob,不是用户凭据。
            | "signature"
            | "cwd"
            | "path"
            | "file_path"
            | "filepath"
            | "originalfilepath"
            | "gitbranch"
            | "git_branch"
            | "version"
            | "timestamp"
            | "created_at"
            | "updated_at"
            | "model"
            | "tooluseid"
            | "tool_use_id"
            | "sourcetooluseid"
            // 截图 / 附件的 base64 载荷
            | "data"
            | "base64"
            | "base64_data"
    )
}

/// 路径形状:带 `/` 且多数片段是"词"就是路径。
/// 这条最值钱 —— 候选正则含 `/` 和 `-`,不挡的话会把整条文件路径当高熵串吞下去。
fn looks_like_path(tok: &str) -> bool {
    if tok.starts_with('/') || tok.starts_with("./") || tok.starts_with("~/") {
        return true;
    }
    let segs: Vec<&str> = tok.split('/').filter(|s| !s.is_empty()).collect();
    if segs.len() < 2 {
        return false;
    }
    segs.iter().filter(|s| WORDISH.is_match(s)).count() * 2 >= segs.len()
}

/// base64 解出来是可打印 ASCII(小整数、短字符串、SVG/JSON 片段)—— 不是不透明凭据。
/// 但**解出来本身就是一枚凭据**时(密钥被 base64 包了一层)不能算噪声,否则等于开后门 ——
/// 判据是"解出来仍然只由凭据字符组成且高熵",带空格/尖括号的文本不算。
fn is_base64_of_text(tok: &str) -> bool {
    let Some(raw) = b64url_decode(&tok.replace('+', "-").replace('/', "_")) else {
        return false;
    };
    if raw.is_empty() || !raw.iter().all(|&b| (0x20..0x7f).contains(&b)) {
        return false;
    }
    let Ok(s) = std::str::from_utf8(&raw) else {
        return false;
    };
    let credential_shaped = s.len() >= 16
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'=' | b'_' | b'-'));
    !(credential_shaped && has_mixed_classes(s) && shannon_entropy(s) > ENTROPY_THRESHOLD)
}

/// 字符**全不重复** = 字母表常量(base62/base58/nanoid 的 charset 字面量),不是随机数据。
/// 它的熵恰恰是最高的(每个符号只出现一次),所以熵检测天生对它最没有抵抗力 —— 但它零秘密含量。
/// 只在 len>=32 上判:那时随机串还能全不重复的概率已低到可以忽略。
fn is_alphabet_constant(tok: &str) -> bool {
    if tok.len() < 32 {
        return false;
    }
    let mut seen = [false; 256];
    for b in tok.bytes() {
        if seen[b as usize] {
            return false;
        }
        seen[b as usize] = true;
    }
    true
}

/// 已知噪声形状 —— 真实转录里高熵但无害的东西。
fn is_known_noise_shape(tok: &str) -> bool {
    UUID.is_match(tok)
        || HEXISH.is_match(tok) // md5 / sha1(git sha40)/ sha256 / 内容寻址十六进制
        || ISO_TS.is_match(tok)
        || INTEGRITY.is_match(tok)
        || VENDOR_OBJECT_ID.is_match(tok)
        || B64_MEDIA.is_match(tok)
        || SSH_PUBKEY.is_match(tok)
        || is_alphabet_constant(tok)
        || looks_like_path(tok)
        || WORDISH.is_match(tok) // 标识符 / slug / 工具名
        || is_base64_of_text(tok)
}

/// 字符类多样性:真随机的 base62/base64 凭据几乎必然同时含大写、小写、数字。
/// 路径、slug、纯小写 id 过不了这关。
fn has_mixed_classes(tok: &str) -> bool {
    tok.chars().any(|c| c.is_ascii_uppercase())
        && tok.chars().any(|c| c.is_ascii_lowercase())
        && tok.chars().any(|c| c.is_ascii_digit())
}

fn is_high_entropy_secret(tok: &str) -> bool {
    tok.len() >= ENTROPY_MIN_LEN
        && has_mixed_classes(tok)
        && !is_known_noise_shape(tok)
        && shannon_entropy(tok) > ENTROPY_THRESHOLD
}

// ─────────────────────── 赋值 / 连接串的值质量闸门 ───────────────────────

/// 类型名、占位符、已脱敏标记 —— `password: …` 右边最常见的**非**密钥。
const NOT_SECRET_VALUES: &[&str] = &[
    "string", "str", "number", "int", "integer", "boolean", "bool", "any", "unknown", "null",
    "none", "nil", "true", "false", "object", "array", "uint8array", "buffer", "bytes", "text",
    "varchar", "secret", "token", "password", "passwd", "apikey", "api_key", "value", "generate",
    "required", "optional", "example", "placeholder", "changeme", "change_me", "redacted",
    "hidden", "masked", "dummy", "sample", "test", "todo", "foo", "bar", "baz", "pass", "user",
    "username", "admin", "root", "postgres", "mysql",
];

fn looks_placeholder(v: &str) -> bool {
    let low = v.to_ascii_lowercase();
    if NOT_SECRET_VALUES.contains(&low.as_str()) {
        return true;
    }
    // 已脱敏:*** / …… / xxxx
    if v.chars().all(|c| matches!(c, '*' | '.' | '•' | 'x' | 'X' | '-' | '_')) {
        return true;
    }
    low.starts_with("your")
        || low.starts_with("my-")
        || low.starts_with('<')
        || low.contains("changeme")
        || low.contains("change_me")
        || low.contains("redact")
        || low.contains("example")
        || low.contains("placeholder")
}

/// 值是否"不透明"——真的像一份凭据,而不是代码。
///
/// 这是 assigned-secret 从 3052 条误报降到可用的唯一原因:真实转录里那条正则右边
/// 绝大多数是 TS 类型标注(`password: string`)、环境变量引用(`process.env.X`)、
/// 变量名(`token: TOKEN`)和占位符,一个真密钥都没有。
fn value_is_opaque(v: &str) -> bool {
    let v = v.trim().trim_matches(|c| c == '"' || c == '\'' || c == '`');
    let v = v.trim_end_matches([',', ';', ')', ']', '}', '.', '\\']);
    if v.len() < 8 || v.len() > 200 {
        return false;
    }
    // 插值 / 命令替换 / 模板 / 代码
    if v.contains("${") || v.contains("$(") || v.contains("{{") || v.starts_with('$') {
        return false;
    }
    if v.contains(['(', ')', '<', '>', '{', '}', '|', '\\', '/', ' ']) {
        return false;
    }
    if looks_placeholder(v) {
        return false;
    }
    // 点号代码引用:process.env.X / var.node_password / fx.deviceToken / z.string
    if v.contains('.') && v.split('.').all(|s| WORDISH.is_match(&s.to_ascii_lowercase())) {
        return false;
    }
    // 全大写蛇形 = 环境变量**名**,不是值(`token: TOKEN`、`password: SEED_USER_PASSWORD`)
    if v.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_') {
        return false;
    }
    has_mixed_classes(v) || shannon_entropy(v) >= 3.5
}

/// 连接串里的口令是否值得报。`postgres://postgres:postgres@`、`://${USER}:${PASS}@`、
/// `://bolusi:***@` 在真实转录里成百上千,零信息量。
fn conn_password_is_real(user: &str, pass: &str) -> bool {
    if pass.contains("${") || pass.contains("$(") || pass.starts_with('$') || pass.contains("{{") {
        return false;
    }
    if pass.eq_ignore_ascii_case(user) {
        return false; // postgres:postgres / bolusi:bolusi —— 开发约定,不是秘密
    }
    pass.len() >= 6 && !looks_placeholder(pass)
}

// ─────────────────────── 豁免清单 ───────────────────────

#[derive(Default)]
pub struct Allowlist {
    pats: Vec<Regex>,
    lits: Vec<String>,
}

impl Allowlist {
    pub fn empty() -> Self {
        Self::default()
    }

    /// 从 `<root>/.agit-allow-secrets` 读。文件不存在就是空表(不是错误)。
    pub fn load(root: &Path) -> Self {
        let mut me = Self::default();
        let Ok(text) = std::fs::read_to_string(root.join(ALLOW_FILE)) else {
            return me;
        };
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            match line.strip_prefix("re:") {
                Some(p) => {
                    if let Ok(re) = Regex::new(p) {
                        me.pats.push(re);
                    }
                }
                None => me.lits.push(line.to_string()),
            }
        }
        me
    }

    fn allows(&self, matched: &str) -> bool {
        self.lits.iter().any(|l| matched.contains(l.as_str()))
            || self.pats.iter().any(|p| p.is_match(matched))
    }
}

// ─────────────────────── 脱敏 ───────────────────────

fn redact(s: &str) -> String {
    let s: String = s.chars().take(48).collect();
    let n = s.chars().count();
    if n <= 10 {
        return "*".repeat(n);
    }
    // 按 **char** 取前缀,不是 `&s[..4]` —— 后者在第 4 字节落在多字节 UTF-8 中间时 panic,
    // 一条含 emoji/中日文的正常行就能让整轮扫描崩掉(而扫描是 commit/push 的安全闸门)。
    let prefix: String = s.chars().take(4).collect();
    format!("{prefix}…{}", "*".repeat(6))
}

// ─────────────────────── 扫描核心 ───────────────────────

struct Ctx<'a> {
    entropy: bool,
    allow: &'a Allowlist,
}

/// 一条待定命中:span 用于重叠去重。
struct Hit {
    start: usize,
    end: usize,
    spec: u8,
    rule: &'static str,
    text: String,
}

/// 重叠命中只留最具体的:`sk-ant-…` 只算 anthropic-key,不再被宽松的 openai-key 重复计一次。
fn dedup_overlaps(mut hits: Vec<Hit>) -> Vec<Hit> {
    hits.sort_by(|a, b| b.spec.cmp(&a.spec).then(a.start.cmp(&b.start)));
    let mut kept: Vec<Hit> = Vec::new();
    for h in hits {
        if kept.iter().any(|k| h.start < k.end && k.start < h.end) {
            continue;
        }
        kept.push(h);
    }
    kept.sort_by_key(|h| h.start);
    kept
}

/// The longest plausible PEM body, in lines. An unterminated BEGIN must NOT swallow the rest of the
/// chunk: every line it covers is marked `consumed` and therefore never scanned by any other rule, so a
/// truncated key would blind the scanner for everything after it — and a tool result that dumps a key is
/// exactly the kind that dumps the .env next to it. RSA-4096 armor is ~50 lines; 80 is generous.
const MAX_KEY_BODY_LINES: usize = 80;

fn key_blocks(lines: &[&str]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut start: Option<usize> = None;
    for (i, l) in lines.iter().enumerate() {
        if start.is_none() {
            if PK_BEGIN.is_match(l) {
                // BEGIN and END on one line: a single-line PEM, or a whole block recovered from a JSON value
                if PK_END.is_match(l) {
                    out.push((i, i));
                } else {
                    start = Some(i);
                }
            }
            continue;
        }
        if PK_END.is_match(l) {
            out.push((start.take().unwrap(), i));
        } else if i - start.unwrap() >= MAX_KEY_BODY_LINES {
            // no END within a plausible key length: report the block, but hand the rest of the chunk back
            out.push((start.take().unwrap(), i));
        }
    }
    // BEGIN with no END (a truncated transcript): still report it, but bound it.
    if let Some(s) = start {
        let last = lines.len().saturating_sub(1).max(s);
        out.push((s, last.min(s + MAX_KEY_BODY_LINES)));
    }
    out
}

/// 扫一段文本(可能多行)。返回 (相对行号从 0 起, rule, excerpt)。
fn scan_chunk(text: &str, ctx: &Ctx) -> Vec<(usize, &'static str, String)> {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    let mut consumed = vec![false; lines.len()];

    // 1) 先吃跨行私钥块。旧规则只匹配 BEGIN 头一行:知道"有",却从没看见、也从没脱敏过
    //    密钥体本身 —— 而密钥体正是要拦的东西。整块吃掉后也顺便挡住体内 base64 触发熵检测。
    for (s, e) in key_blocks(&lines) {
        // The pragma exempts ONE PHYSICAL LINE, never a whole block: `block.contains(ALLOW_PRAGMA)` let a
        // single pragma anywhere inside a key body suppress the entire block — an escape hatch that can
        // blanket-disable the gate is worse than `--no-verify`, which at least shows up in the command.
        let exempt = ctx.allow.allows(lines[s]) || lines[s].contains(ALLOW_PRAGMA);
        if !exempt {
            out.push((
                s,
                "private-key-block",
                // redact() the marker line too: printing it raw would make the scanner that exists to stop
                // a secret spreading the thing that copies it into a second place (logs, CI output).
                format!("{} …{} lines of key body hidden", redact(lines[s].trim()), e - s + 1),
            ));
        }
        for c in consumed.iter_mut().take(e + 1).skip(s) {
            *c = true;
        }
    }

    // 2) 逐行:规则 + 熵,收集成 Hit 后按 span 去重
    for (i, line) in lines.iter().enumerate() {
        if consumed[i] || line.contains(ALLOW_PRAGMA) {
            continue;
        }
        let mut hits: Vec<Hit> = Vec::new();

        for rule in RULES.iter() {
            // find_iter:一行里同一条规则可能有多个密钥,旧代码用 find 只取第一个。
            for m in rule.re.find_iter(line) {
                if rule.validate.map(|f| f(m.as_str())) == Some(false) {
                    continue;
                }
                hits.push(Hit {
                    start: m.start(),
                    end: m.end(),
                    spec: rule.spec,
                    rule: rule.name,
                    text: m.as_str().to_string(),
                });
            }
        }

        for c in CONN_STRING.captures_iter(line) {
            if conn_password_is_real(c.get(2).unwrap().as_str(), c.get(3).unwrap().as_str()) {
                let m = c.get(0).unwrap();
                hits.push(Hit {
                    start: m.start(),
                    end: m.end(),
                    spec: 60,
                    rule: "connection-string",
                    text: m.as_str().to_string(),
                });
            }
        }

        for c in ASSIGNED.captures_iter(line) {
            if value_is_opaque(c.get(2).unwrap().as_str()) {
                let m = c.get(0).unwrap();
                hits.push(Hit {
                    start: m.start(),
                    end: m.end(),
                    spec: 30,
                    rule: "assigned-secret",
                    text: m.as_str().to_string(),
                });
            }
        }

        if ctx.entropy {
            for m in HIGH_ENTROPY_CANDIDATE.find_iter(line) {
                if is_high_entropy_secret(m.as_str()) {
                    hits.push(Hit {
                        start: m.start(),
                        end: m.end(),
                        spec: 10,
                        rule: "high-entropy-string",
                        text: m.as_str().to_string(),
                    });
                }
            }
        }

        for h in dedup_overlaps(hits) {
            if !ctx.allow.allows(&h.text) {
                out.push((i, h.rule, redact(&h.text)));
            }
        }
    }
    out
}

// ─────────────────────── JSON 感知 ───────────────────────

/// 遍历 JSON,只扫字符串**值**;字段名决定要不要开熵。
fn walk_json(v: &serde_json::Value, key: &str, ctx: &Ctx, line: usize, out: &mut Vec<Finding>) {
    match v {
        serde_json::Value::Object(m) => {
            for (k, vv) in m {
                walk_json(vv, k, ctx, line, out);
            }
        }
        serde_json::Value::Array(a) => {
            for vv in a {
                walk_json(vv, key, ctx, line, out);
            }
        }
        serde_json::Value::String(s) => {
            // 字段感知:JSON 路径只看得到**值**,看不到 `"private_key":` 这层结构,
            // 所以 gcp-service-account 那条按行匹配的正则在这里永远不会响 —— 按字段名补上。
            if key.eq_ignore_ascii_case("private_key")
                && s.contains("-----BEGIN")
                && !ctx.allow.allows(s)
            {
                out.push(Finding {
                    rule: "gcp-service-account",
                    line,
                    excerpt: redact(s),
                });
            }
            let sub = Ctx {
                entropy: ctx.entropy && !noise_field(key),
                allow: ctx.allow,
            };
            for (_, rule, excerpt) in scan_chunk(s, &sub) {
                out.push(Finding { rule, line, excerpt });
            }
        }
        _ => {}
    }
}

/// 嗅探是不是 JSONL。只看开头若干条非空行:session dump 从第一行起就是 JSON 记录。
/// 先嗅探再扫,免得普通文本被整份扫两遍(先按 JSONL 试、失败再按纯文本重来)。
fn looks_like_jsonl(text: &str) -> bool {
    let (mut parsed, mut seen) = (0usize, 0usize);
    for line in text.lines() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        seen += 1;
        if (t.starts_with('{') || t.starts_with('['))
            && serde_json::from_str::<serde_json::Value>(t).is_ok()
        {
            parsed += 1;
        }
        if seen >= 20 {
            break;
        }
    }
    // 九成以上才算;否则普通文本里偶然一行 `{}` 就会让整份文件走错路径。
    seen > 0 && parsed * 10 >= seen * 9
}

/// JSONL:只扫字符串值。解析不了的行(如被截断的尾行)退回按纯文本扫,免得整条漏掉。
fn scan_jsonl(text: &str, ctx: &Ctx) -> Vec<Finding> {
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        match serde_json::from_str::<serde_json::Value>(t) {
            Ok(v) => {
                if !line.contains(ALLOW_PRAGMA) {
                    walk_json(&v, "", ctx, i + 1, &mut out);
                }
            }
            Err(_) => {
                for (_, rule, excerpt) in scan_chunk(line, ctx) {
                    out.push(Finding {
                        rule,
                        line: i + 1,
                        excerpt,
                    });
                }
            }
        }
    }
    out
}

pub fn scan_text(text: &str) -> Vec<Finding> {
    scan_text_opts(text, true)
}

/// `entropy=false` 彻底关掉泛化熵检测。
///
/// 注意:对 JSON/JSONL **不再需要**关熵。这里把每行当 JSON 记录解析、只对字符串**值**跑规则,
/// 并按字段名 + shape 白名单挡掉 uuid/sha/时间戳/路径/内容寻址哈希这些转录噪声,
/// 于是熵可以对 session dump 开着而不被淹没。
pub fn scan_text_opts(text: &str, entropy: bool) -> Vec<Finding> {
    scan_text_allow(text, entropy, &Allowlist::empty())
}

pub fn scan_text_allow(text: &str, entropy: bool, allow: &Allowlist) -> Vec<Finding> {
    let ctx = Ctx { entropy, allow };
    if looks_like_jsonl(text) {
        let mut out = scan_jsonl(text, &ctx);
        out.sort_by_key(|f| f.line);
        return out;
    }
    scan_chunk(text, &ctx)
        .into_iter()
        .map(|(i, rule, excerpt)| Finding {
            rule,
            line: i + 1,
            excerpt,
        })
        .collect()
}

// ─────────────────────── 文件 / 目录 ───────────────────────

/// 二进制就跳过 —— 比按扩展名放行稳:.env / .pem / .key / .sh / .yaml / .toml / 无扩展名
/// 的文件全都装得下密钥,旧的 (md|jsonl|json|txt) 白名单把它们统统漏掉。
fn is_probably_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8192).any(|&b| b == 0)
}

pub fn scan_file(path: &Path) -> Result<Vec<Finding>> {
    scan_file_allow(path, &Allowlist::empty())
}

pub fn scan_file_allow(path: &Path, allow: &Allowlist) -> Result<Vec<Finding>> {
    let bytes = std::fs::read(path)?;
    if is_probably_binary(&bytes) {
        return Ok(Vec::new());
    }
    let Ok(text) = String::from_utf8(bytes) else {
        return Ok(Vec::new());
    };
    // 熵对所有文本都开:JSON 走"只扫值 + shape 白名单",不会再被 session 噪声淹没。
    Ok(scan_text_allow(&text, true, allow))
}

/// 递归扫描一个目录树里的文本文件,打印命中,返回命中数。
/// session dump 是 jsonl,里面可能带 agent 见过的密钥。
pub fn scan_tree(root: &Path) -> Result<usize> {
    let allow = Allowlist::load(root);
    let mut total = 0;
    for e in walkdir::WalkDir::new(root)
        .into_iter()
        .filter_entry(|e| e.file_name() != ".git")
        .filter_map(|e| e.ok())
    {
        if !e.file_type().is_file() {
            continue;
        }
        let p = e.path();
        for f in scan_file_allow(p, &allow)? {
            eprintln!(
                "  {}:{}  [{}]  {}",
                p.strip_prefix(root).unwrap_or(p).display(),
                f.line,
                f.rule,
                f.excerpt
            );
            total += 1;
        }
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_does_not_panic_on_multibyte_boundary() {
        // 每个 € 占 3 字节:旧的 &s[..4] 会切在字符中间 panic。
        let out = redact("€€€€€€€€€€€€");
        assert!(out.starts_with('€'));
        assert!(out.contains('…'));
        // 混合:前缀多字节,同样不能 panic
        let _ = redact("café_secret_value_1234");
        // 短串走 '*' 分支,按 char 数
        assert_eq!(redact("日本語"), "***");
    }

    #[test]
    fn scan_text_survives_multibyte_lines() {
        // 一行含多字节 + 一个真密钥,既不 panic,又要命中密钥。
        let f = scan_text("日本語 password = caféSecret42x\nAKIAIOSFODNN7EXAMPLE\n");
        assert!(f.iter().any(|x| x.rule == "aws-access-key-id"));
    }

    fn rules_hit(text: &str) -> Vec<&'static str> {
        let mut v: Vec<&'static str> = scan_text(text).into_iter().map(|f| f.rule).collect();
        v.sort_unstable();
        v.dedup();
        v
    }

    // ─────────── 正例语料:每条规则都要真的响 ───────────

    /// 这枚 ghp_ 是 base62-token.js 文档里的公开示例,校验和自洽 —— 正好当 CRC 的测试向量。
    const GH_TOKEN: &str = "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";

    #[test]
    fn github_crc_validates_real_vector_and_rejects_tampering() {
        assert!(github_crc_ok(GH_TOKEN));
        // 改熵段一个字符,校验和就对不上 —— 这正是它能杀掉误报的原因。
        let bad = GH_TOKEN.replace("zQWB", "zQWC");
        assert!(!github_crc_ok(&bad));
        assert!(rules_hit(&format!("token: {GH_TOKEN}")).contains(&"github-token"));
        assert!(!rules_hit(&format!("token: {bad}")).contains(&"github-token"));
    }

    #[test]
    fn positive_corpus_every_rule_fires() {
        let cases: &[(&str, &str)] = &[
            ("aws-access-key-id", "AKIAIOSFODNN7EXAMPLE"),
            (
                "aws-secret-access-key",
                "aws_secret_access_key=wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            ),
            ("github-token", GH_TOKEN),
            (
                "github-fine-grained-pat",
                "github_pat_11ABCDEFG0123456789abc_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456",
            ),
            ("gitlab-token", "glpat-ABCDEFGHIJ1234567890"),
            ("google-api-key", "AIzau8jzPde0IgxLd6GncfBAepfJBd0Kh8oOOL8"),
            ("stripe-secret-key", "sk_live_4eC39HqLyjWDarjtT1zdp7dc"),
            ("slack-token", "xoxb-123456789012-1234567890123-AbCdEfGhIjKlMnOpQrStUvWx"),
            (
                "slack-webhook",
                "https://hooks.slack.com/services/T00000000/B00000000/XXXXXXXXXXXXXXXXXXXXXXXX",
            ),
            (
                "discord-webhook",
                "https://discord.com/api/webhooks/123456789012345678/AbCdEfGhIjKlMnOpQrStUvWxYz0123456789",
            ),
            (
                "sendgrid-key",
                "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",
            ),
            ("npm-token", "npm_abcdefghijklmnopqrstuvwxyz0123456789"),
            (
                "pypi-token",
                "pypi-AgEIcHlwaS5vcmcCJDcxNjJhZjE0LTk5MTUtNDNlMS05ZjJkLWEyMzQ1Njc4OTAxMg",
            ),
            ("huggingface-token", "hf_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefgh"),
            ("anthropic-key", "sk-ant-api03-AbCdEf1234567890GhIjKlMnOpQrStUvWx"),
            ("openai-project-key", "sk-proj-AbCdEf1234567890GhIjKlMnOpQrStUvWx"),
            ("openai-key", "sk-AbCdEf1234567890GhIjKlMnOpQrStUvWx"),
            ("twilio-api-key-sid", "SK0123456789abcdef0123456789abcdef"),
            ("twilio-auth-token", "TWILIO_AUTH_TOKEN=0123456789abcdef0123456789abcdef"),
            ("cloudflare-token", "cfut_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij0123456"),
            ("datadog-app-key", "ddapp_ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789"),
            ("datadog-api-key", "DD_API_KEY=0123456789abcdef0123456789abcdef"),
            (
                "sentry-dsn",
                "https://0123456789abcdef0123456789abcdef@o123.ingest.sentry.io/456",
            ),
            (
                "jwt",
                "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dBjftJeZ4CVPmB92K27uhbUJU1p1r_wW1gFWFOEjXk",
            ),
            ("connection-string", "postgres://svc:Xk9fQ2mVb7Lp@db.internal:5432/app"),
            ("assigned-secret", "api_key = 'Xk9fQ2mVb7LpZr3T'"),
        ];
        for (rule, text) in cases {
            let hits = rules_hit(text);
            assert!(hits.contains(rule), "规则 {rule} 没在 {text:?} 上命中(实得 {hits:?})");
        }
    }

    #[test]
    fn private_key_block_is_multiline_and_body_is_redacted() {
        let pem = "-----BEGIN RSA PRIVATE KEY-----\nMIIEowIBAAKCAQEAx7Kj9Qe2mVb7LpZr3TAbCdEf\nGhIjKlMnOpQrStUvWxYz0123456789abcdefGHI\n-----END RSA PRIVATE KEY-----";
        let f = scan_text(pem);
        let pk: Vec<_> = f.iter().filter(|x| x.rule == "private-key-block").collect();
        assert_eq!(pk.len(), 1, "one report per block");
        assert_eq!(pk[0].line, 1);
        assert!(pk[0].excerpt.contains("4 lines of key body hidden"), "{}", pk[0].excerpt);
        // the key body must never appear verbatim
        assert!(!pk[0].excerpt.contains("MIIEowIBAAKCAQEA"));
        // the marker line is redacted too — the scanner must not copy the secret it exists to stop
        assert!(!pk[0].excerpt.contains("BEGIN RSA PRIVATE KEY"), "marker printed raw: {}", pk[0].excerpt);
        // base64 inside the body must not be re-reported by the entropy rule
        assert!(!f.iter().any(|x| x.rule == "high-entropy-string"));
    }

    /// An unterminated BEGIN must not blind the scanner for the rest of the chunk: every line a key block
    /// covers is marked consumed, so an EOF-extending block would swallow every later secret. A truncated
    /// key dump is exactly the tool result that also dumps the .env next to it.
    #[test]
    fn unterminated_key_block_does_not_swallow_the_rest_of_the_chunk() {
        let mut t = String::from("-----BEGIN RSA PRIVATE KEY-----\n");
        for i in 0..200 {
            t.push_str(&format!("MIIEowIBAAKCAQEAx7Kj9Qe2mVb7LpZr3TAbCd{i:03}\n"));
        }
        t.push_str("aws_key = AKIAIOSFODNN7EXAMPLE\n"); // far past the truncated block
        let f = scan_text(&t);
        assert!(f.iter().any(|x| x.rule == "private-key-block"), "the truncated block must still fire");
        assert!(
            f.iter().any(|x| x.rule == "aws-access-key-id"),
            "a secret AFTER an unterminated key block was never scanned: {:?}",
            f.iter().map(|x| x.rule).collect::<Vec<_>>()
        );
    }

    /// The pragma exempts ONE PHYSICAL LINE. An escape hatch that can blanket-disable the gate is worse
    /// than --no-verify, which at least appears in the command someone typed.
    #[test]
    fn pragma_cannot_blanket_disable_a_key_block() {
        let pem = format!(
            "-----BEGIN RSA PRIVATE KEY-----\nMIIEowIBAAKCAQEAx7Kj9Qe2mVb7 {ALLOW_PRAGMA}\n-----END RSA PRIVATE KEY-----"
        );
        let f = scan_text(&pem);
        assert!(
            f.iter().any(|x| x.rule == "private-key-block"),
            "a pragma buried in the key BODY suppressed the whole block"
        );

        // on the marker line itself, it exempts that block only — and nothing else
        let ok = format!(
            "-----BEGIN RSA PRIVATE KEY----- {ALLOW_PRAGMA}\nMII\n-----END RSA PRIVATE KEY-----\naws_key = AKIAIOSFODNN7EXAMPLE"
        );
        let f2 = scan_text(&ok);
        assert!(!f2.iter().any(|x| x.rule == "private-key-block"), "marker-line pragma should exempt");
        assert!(f2.iter().any(|x| x.rule == "aws-access-key-id"), "it must not suppress other lines");
    }

    #[test]
    fn private_key_inside_json_value_is_caught() {
        // session dump 里私钥是被 `\n` 转义进一条 JSON 记录的 —— 按物理行看根本没有"跨行"。
        let line = r#"{"type":"user","message":{"content":"cat key.pem\n-----BEGIN PRIVATE KEY-----\nMIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQ\n-----END PRIVATE KEY-----\n"}}"#;
        assert!(rules_hit(line).contains(&"private-key-block"));
    }

    #[test]
    fn gcp_service_account_json_is_caught() {
        let line = r#"{"type":"service_account","project_id":"x","private_key":"-----BEGIN PRIVATE KEY-----\nMIIEvQIBADANBgkq\n-----END PRIVATE KEY-----\n","client_email":"a@b.iam.gserviceaccount.com"}"#;
        assert!(rules_hit(line).contains(&"gcp-service-account"));
    }

    #[test]
    fn bare_opaque_credential_fires_entropy_without_any_keyword() {
        // 没有任何关键字/前缀锚点,只有一串随机 base64 —— 熵规则必须自己抓住它。
        // (这也保证下面 dedup 的用例是"两条规则真的都命中"而非空跑。)
        let f = scan_text("wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY");
        assert!(
            f.iter().any(|x| x.rule == "high-entropy-string"),
            "实得 {:?}",
            f.iter().map(|x| x.rule).collect::<Vec<_>>()
        );
    }

    #[test]
    fn overlapping_rules_do_not_double_fire() {
        // aws-secret-access-key 的 span 把值整个包住,而值本身又会触发熵规则:
        // 同一处密钥只能算一次,且要算最具体的那条(这条是真正跑到 dedup_overlaps 的用例)。
        let f = scan_text("aws_secret_access_key=wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY");
        assert_eq!(
            f.len(),
            1,
            "重叠命中要去重:{:?}",
            f.iter().map(|x| (x.rule, &x.excerpt)).collect::<Vec<_>>()
        );
        assert_eq!(f[0].rule, "aws-secret-access-key");

        // sk-ant- 只算 anthropic-key,不会再被宽松的 openai 规则重复计一次。
        let f = scan_text("key: sk-ant-api03-AbCdEf1234567890GhIjKlMnOpQrStUvWx");
        assert_eq!(f.iter().filter(|x| x.rule.ends_with("-key")).count(), 1);
        assert!(f.iter().any(|x| x.rule == "anthropic-key"));
        assert!(!f.iter().any(|x| x.rule == "openai-key"));
    }

    #[test]
    fn multiple_secrets_on_one_line_all_fire() {
        // 旧代码 re.find 每行每规则只取第一个 —— 第二枚密钥直接漏。
        let f = scan_text("AKIAIOSFODNN7EXAMPLE and AKIAZZZZZZZZZZZZZZZZ");
        assert_eq!(f.iter().filter(|x| x.rule == "aws-access-key-id").count(), 2);
    }

    #[test]
    fn jwt_requires_decodable_header() {
        // 三段 base64url 但头部不是 JSON:不是 JWT。
        let fake = "eyJhbGciOi-not-json.aaaabbbb.ccccdddd";
        assert!(!rules_hit(fake).contains(&"jwt"));
    }

    // ─────────── 负例语料:真实转录噪声,必须一条不响 ───────────

    #[test]
    fn negative_corpus_real_transcript_noise_is_silent() {
        // 全部取自 ~/.claude/projects 里的真实 session dump 形态。
        let noise: &[&str] = &[
            // uuid / requestId / sessionId
            r#"{"uuid":"b64c4fe8-5946-4ee9-9173-a6ba3a13079e","parentUuid":"88e56b45-35f0-4042-bcc1-00e7ef806d06"}"#,
            r#"{"requestId":"req_011CTx8dK3nQvWq7ZrYpLmNb","sessionId":"36e0c3be-53f2-4860-9916-84d47248d400"}"#,
            // git sha40 / sha256
            r#"{"text":"commit 05b1263f8a9c4e2d1b3a5f7e9c0d2b4a6e8f0a1c and blob 8f14e45fceea167a5a36dedd4bea2543"}"#,
            // ISO 时间戳
            r#"{"timestamp":"2026-07-16T22-37-58-845Z","created_at":"2026-07-16T22:37:58.845Z"}"#,
            // 文件路径(熵很高,候选正则里 `/` 和 `-` 会把整条路径吞进来)
            r#"{"cwd":"/home/user/agent-git/.claude/worktrees/wf_8275b1c0-0d5-1"}"#,
            r#"{"text":"see packages/web/src/features/admin/components/SwitchField.tsx"}"#,
            r#"{"file_path":"/tmp/claude-1000/-home-user-broccoli/4bcaba12-fb3a-467a-9498/scratchpad/RECOVERY.md"}"#,
            // npm 完整性哈希(内容寻址)
            r#"{"text":"integrity sha512-bRISgCIjP20/tbWSPWMEi54QVPRZExkuD9lJLUIxUKtwVJA8wW1Trb1jMs1RFXo1CBTNZ5hpC9QvmKWdoJ"}"#,
            // Claude thinking 块的加密签名:高熵、含大小写数字,靠字段名挡
            r#"{"type":"thinking","thinking":"ok","signature":"EowRCokBCA8YAipA4LBF6MRltZkwBLSxl3HOj2l2AAYhSinK8yWNXzeweMExfxSHeBRu8XmTwPCrIy8C4tM42rFV"}"#,
            // MCP 工具名 / 标识符
            r#"{"name":"mcp__plugin_context7_context7__resolve-library-id"}"#,
            // TS 类型标注 —— 旧 assigned-secret 的头号误报源
            r#"{"text":"interface User { password: string; token: string; apiKey: string }"#,
            r#"{"text":"const password: String = String::new(); let token: &str = \"\";"}"#,
            // 环境变量引用 / 插值 / 命令替换
            r#"{"text":"password = process.env.SEED_USER_PASSWORD"}"#,
            r#"{"text":"token: ${MEDIA_CALLBACK_TOKEN:?set it}"}"#,
            r#"{"text":"password: var.node_password"}"#,
            r#"{"text":"api_key=$(curl -s https://example.com/key)"}"#,
            // 变量名 / 占位符 / 已脱敏
            r#"{"text":"token: TOKEN, secret: <secret>, password: changeme, apiKey: your-key-here"}"#,
            r#"{"text":"DATABASE_URL=postgres://bolusi:***@db:5432/app"}"#,
            // 开发默认口令 / 插值口令:零信息量
            r#"{"text":"postgres://postgres:postgres@localhost:5432/dev"}"#,
            r#"{"text":"postgres://${POSTGRES_USER}:${POSTGRES_PASSWORD}@db:5432/app"}"#,
            // base64 过的短文本(不是不透明凭据)
            r#"{"text":"echo MTIzNDU2Nzg5MDEyMzQ1Njc4OTAxMjM0NTY3ODkw | base64 -d"}"#,
            // Anthropic 的对象 ID:会**以正文形式**出现在 content/stdout 里,不只是在 id 字段
            r#"{"content":"tool_use toolu_0128tPyqU7qMYxWNpUgVvgLc failed for msg_011Cd5erk5iXeZuZXdecwqDB (req_011Cd5erj6vuZwVsuytd6dby)"}"#,
            // 截图的 base64 载荷
            r#"{"content":"iVBORw0KGgoAAAANSUhEUgAAAwwAAAOFCAIAAABIovI8AAAQAElEQVR4nOzdCUAT19oG4A"}"#,
            // 字母表常量:全不重复 = 熵最高,却零秘密含量
            r#"{"text":"const B62 = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789'"}"#,
            r#"{"text":"nanoid alphabet 123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz"}"#,
            // SSH 公钥(不是密钥)
            r#"{"text":"ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIB8SZkksLXpUPI8zxUyPDYABQfv2lCs0qOkiabcdefgh user@host"}"#,
            // base64 过的 SVG data URI
            r#"{"text":"url(data:image/svg+xml;base64,PHN2ZyB4bWxucz0naHR0cDovL3d3dy53My5vcmcvMjAwMC9zdmcnIHdpZHRoPScxNic)"}"#,
        ];
        let mut noisy: Vec<String> = Vec::new();
        for n in noise {
            for f in scan_text(n) {
                noisy.push(format!("[{}] {} ← {}", f.rule, f.excerpt, n));
            }
        }
        assert!(noisy.is_empty(), "负例语料必须零命中,实得:\n{}", noisy.join("\n"));
    }

    #[test]
    fn entropy_is_on_for_jsonl_values_and_catches_opaque_credential() {
        // 这就是当初关掉熵所付出的代价:一枚不匹配任何已知前缀的凭据,被 agent cat 进转录。
        let line = r#"{"type":"user","message":{"content":"cat .env\nINTERNAL_API_TOKEN=Zx8Z4pQ1mV7bLr3TnW2yJhKd5Fs6Gc9A"}}"#;
        let hits = rules_hit(line);
        assert!(
            hits.contains(&"high-entropy-string") || hits.contains(&"assigned-secret"),
            "不透明凭据必须被抓到(实得 {hits:?})"
        );
    }

    // ─────────── 豁免口子 ───────────

    #[test]
    fn inline_pragma_suppresses_the_line() {
        let text = format!("aws_key = AKIAIOSFODNN7EXAMPLE  # {ALLOW_PRAGMA} 文档示例");
        assert!(scan_text(&text).is_empty());
        // 没有 pragma 的同一行照报
        assert!(!scan_text("aws_key = AKIAIOSFODNN7EXAMPLE").is_empty());
    }

    #[test]
    fn allowlist_file_suppresses_matching_findings() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(ALLOW_FILE),
            "# AWS 文档里的示例 key\nAKIAIOSFODNN7EXAMPLE\nre:^glpat-TESTONLY",
        )
        .unwrap();
        let allow = Allowlist::load(dir.path());
        let text = "AKIAIOSFODNN7EXAMPLE\nglpat-TESTONLY1234567890\nglpat-REALLOOKING0987654321";
        let hits = scan_text_allow(text, true, &allow);
        assert_eq!(hits.len(), 1, "只剩没被豁免的那条:{:?}", hits.iter().map(|f| &f.excerpt).collect::<Vec<_>>());
        assert_eq!(hits[0].rule, "gitlab-token");
    }

    // ─────────── 文件层 ───────────

    #[test]
    fn scan_file_covers_dotenv_and_pem_not_just_md_jsonl() {
        // 旧的扩展名闸门 (md|jsonl|json|txt) 把 .env/.pem/.sh/无扩展名统统跳过。
        let dir = tempfile::tempdir().unwrap();
        for name in [".env", "id_rsa.pem", "deploy.sh", "config.yaml", "Makefile"] {
            let p = dir.path().join(name);
            std::fs::write(&p, "AWS_KEY=AKIAIOSFODNN7EXAMPLE\n").unwrap();
            assert!(
                !scan_file(&p).unwrap().is_empty(),
                "{name} 必须被扫到"
            );
        }
    }

    #[test]
    fn binary_files_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("blob.bin");
        std::fs::write(&p, b"\x00\x01AKIAIOSFODNN7EXAMPLE\x00").unwrap();
        assert!(scan_file(&p).unwrap().is_empty());
    }
}


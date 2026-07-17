//! HTTP wire helpers: the Req view over request headers, the Resp builder (with an axum
//! IntoResponse bridge), credential parsing, and git's denial response. Bodies verbatim from the
//! monolith; only the transport bridge is new.
#![allow(clippy::doc_overindented_list_items, clippy::doc_lazy_continuation)]
use axum::body::Body;
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode, Uri};
use axum::response::Response;

use agit::hub::acl::{Caller, Deny};
use agit::hub::session as websession;

pub(crate) struct Req {
    pub(crate) method: String,
    pub(crate) target: String,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) content_length: usize,
}

impl Req {
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
    pub(crate) fn host(&self) -> String {
        self.header("host").unwrap_or("localhost:8177").to_string()
    }
    pub(crate) fn query(&self) -> &str {
        self.target.split_once('?').map(|(_, q)| q).unwrap_or("")
    }
    pub(crate) fn sid(&self) -> Option<String> {
        self.header("cookie").and_then(websession::parse_cookie)
    }
}

/// Build a [`Req`] view from axum request parts, so the verbatim handlers keep working unchanged.
pub(crate) fn req_from_parts(method: &str, uri: &Uri, headers: &HeaderMap) -> Req {
    let target = match uri.query() {
        Some(q) => format!("{}?{}", uri.path(), q),
        None => uri.path().to_string(),
    };
    let mut hs = Vec::with_capacity(headers.len());
    let mut content_length = 0usize;
    for (k, v) in headers {
        let val = v.to_str().unwrap_or("").to_string();
        if k.as_str().eq_ignore_ascii_case("content-length") {
            content_length = val.parse().unwrap_or(0);
        }
        hs.push((k.as_str().to_string(), val));
    }
    Req { method: method.to_string(), target, headers: hs, content_length }
}

pub(crate) fn git_deny_resp(caller: &Caller, d: Deny) -> Resp {
    if d == Deny::Anonymous {
        return Resp::text(401, "credentials required. Put a token (`agit-hub token add`) in git's password field; the username can be anything.")
            .with("WWW-Authenticate", "Basic realm=\"agit-hub\"");
    }
    let _ = caller;
    Resp::text(403, &format!("denied: {}", d.reason()))
}

pub(crate) fn credentials(req: &Req) -> Vec<String> {
    let Some(v) = req.header("authorization") else {
        return vec![];
    };
    let v = v.trim();
    if let Some(b64) = v.strip_prefix("Basic ").or_else(|| v.strip_prefix("basic ")) {
        if let Some(dec) = b64_decode(b64.trim()) {
            if let Ok(s) = String::from_utf8(dec) {
                return match s.split_once(':') {
                    Some((u, p)) => vec![p.to_string(), u.to_string()],
                    None => vec![s],
                };
            }
        }
    }
    if let Some(t) = v.strip_prefix("Bearer ").or_else(|| v.strip_prefix("bearer ")) {
        return vec![t.trim().to_string()];
    }
    vec![]
}

pub(crate) fn b64_decode(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let bytes: Vec<u8> = s.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    let mut out = vec![];
    for chunk in bytes.chunks(4) {
        let mut buf = [0u8; 4];
        let mut n = 0;
        for &c in chunk {
            if c == b'=' {
                break;
            }
            buf[n] = val(c)?;
            n += 1;
        }
        if n >= 2 {
            out.push((buf[0] << 2) | (buf[1] >> 4));
        }
        if n >= 3 {
            out.push((buf[1] << 4) | (buf[2] >> 2));
        }
        if n >= 4 {
            out.push((buf[2] << 6) | buf[3]);
        }
    }
    Some(out)
}

pub(crate) struct Resp {
    pub(crate) status: u16,
    pub(crate) ctype: String,
    pub(crate) body: Vec<u8>,
    pub(crate) extra: Vec<(String, String)>,
}

impl Resp {
    pub(crate) fn new(status: u16, ctype: &str, body: Vec<u8>) -> Resp {
        Resp { status, ctype: ctype.to_string(), body, extra: vec![] }
    }
    pub(crate) fn text(status: u16, s: &str) -> Resp {
        Resp::new(status, "text/plain; charset=utf-8", s.as_bytes().to_vec())
    }
    pub(crate) fn json(v: serde_json::Value) -> Resp {
        Resp::json_status(200, v)
    }
    pub(crate) fn json_status(status: u16, v: serde_json::Value) -> Resp {
        Resp::new(status, "application/json", serde_json::to_vec(&v).unwrap_or_else(|_| b"{}".to_vec()))
    }
    pub(crate) fn err(status: u16, msg: &str) -> Resp {
        Resp::json_status(status, serde_json::json!({ "error": msg }))
    }
    pub(crate) fn no_content() -> Resp {
        Resp::new(204, "text/plain; charset=utf-8", vec![])
    }
    pub(crate) fn with(mut self, k: &str, v: &str) -> Resp {
        self.extra.push((k.to_string(), v.to_string()));
        self
    }
}

/// Bridge the hand-rolled [`Resp`] onto axum. The status/content-type/extra-headers/body map straight
/// across; header names/values are validated (a bad one is dropped rather than panicking).
impl axum::response::IntoResponse for Resp {
    fn into_response(self) -> Response {
        let Resp { status, ctype, body, extra } = self;
        let mut response = Response::new(Body::from(body));
        *response.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::OK);
        let headers = response.headers_mut();
        if let Ok(v) = HeaderValue::from_str(&ctype) {
            headers.insert(header::CONTENT_TYPE, v);
        }
        for (k, v) in extra {
            if let (Ok(name), Ok(val)) = (HeaderName::from_bytes(k.as_bytes()), HeaderValue::from_str(&v)) {
                headers.append(name, val);
            }
        }
        response
    }
}

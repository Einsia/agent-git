//! Read-only catalog requests for the desktop's bundled CLI boundary.
use super::Client;
use anyhow::{bail, ensure};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    operation: String,
    #[serde(default)]
    owner: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    reference: String,
    #[serde(default)]
    session: String,
    #[serde(default)]
    path: String,
    #[serde(default)]
    query: String,
    #[serde(default)]
    scope: String,
    #[serde(default)]
    page: usize,
    #[serde(default)]
    cursor: String,
    #[serde(default)]
    from: usize,
}

fn segment(value: &str) -> crate::Result<String> {
    ensure!(
        !value.is_empty()
            && value != "."
            && value != ".."
            && !value.contains(['/', '\\'])
            && !value.chars().any(char::is_control),
        "Invalid catalog path segment"
    );
    Ok(encode(value))
}
fn encode(value: &str) -> String {
    value
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

fn route(r: &Request) -> crate::Result<String> {
    let page = r.page.clamp(1, 100_000);
    if r.operation == "repositories" {
        let prefix = match r.scope.as_str() {
            "mine" => "api/me/agents",
            "" | "explore" => "api/agents",
            _ => bail!("Unsupported repository scope"),
        };
        return Ok(format!(
            "{prefix}?page={page}&per_page=24&q={}",
            encode(r.query.trim())
        ));
    }
    if r.operation == "search" {
        ensure!(!r.query.trim().is_empty(), "Enter a session search query");
        ensure!(
            matches!(r.scope.as_str(), "" | "explore"),
            "Session search covers all accessible repositories"
        );
        return Ok(format!(
            "api/search/sessions?q={}&page={page}&per=20",
            encode(r.query.trim())
        ));
    }
    let base = format!("api/agents/{}/{}", segment(&r.owner)?, segment(&r.name)?);
    let reference = encode(&r.reference);
    Ok(match r.operation.as_str() {
        "repository" => base,
        "metadata" => format!("{base}/metadata"),
        "refs" => format!("{base}/refs"),
        "sessions" => format!(
            "{base}/sessions?ref={}&page={page}&per=20{}",
            if reference.is_empty() {
                "*"
            } else {
                &reference
            },
            if r.cursor.is_empty() {
                String::new()
            } else {
                format!("&cursor={}", encode(&r.cursor))
            }
        ),
        "transcript" => {
            let from = r.from.clamp(1, 1_000_000);
            ensure!(
                !reference.is_empty() && r.reference != "*",
                "Select a saved reference before reading history"
            );
            format!(
                "{base}/sessions/{}?ref={reference}&from={from}&to={}&detail=inline",
                segment(&r.session)?,
                from + 10
            )
        }
        "files" => format!(
            "{base}/files{}",
            if reference.is_empty() {
                String::new()
            } else {
                format!("?ref={reference}")
            }
        ),
        "file" => {
            let path = r
                .path
                .split('/')
                .map(segment)
                .collect::<crate::Result<Vec<_>>>()?
                .join("/");
            format!(
                "{base}/files/{path}{}",
                if reference.is_empty() {
                    String::new()
                } else {
                    format!("?ref={reference}")
                }
            )
        }
        _ => bail!("Unsupported catalog operation"),
    })
}

impl Client {
    pub fn catalog_read(&self, request: Request) -> crate::Result<Value> {
        if request.operation == "status" {
            let mut result = json!({"hub": self.base(), "authenticated": false, "username": null});
            if self.has_token() {
                match self.me() {
                    Ok(me) => {
                        result["authenticated"] = json!(true);
                        result["username"] = json!(me.username);
                    }
                    Err(error) => result["error"] = json!(error.to_string()),
                }
            }
            return Ok(result);
        }
        let value: Value = self.get(&route(&request)?)?;
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn request(value: Value) -> Request {
        serde_json::from_value(value).unwrap()
    }
    #[test]
    fn catalog_cannot_route_to_arbitrary_paths() {
        for owner in ["..", "https://evil.test", "a/b", "a\\b", ""] {
            assert!(
                route(&request(
                    json!({"operation":"repository","owner":owner,"name":"repo"})
                ))
                .is_err()
            );
        }
        assert!(
            route(&request(
                json!({"operation":"file","owner":"alice","name":"repo","path":"a/../secret"})
            ))
            .is_err()
        );
        assert!(
            route(&request(
                json!({"operation":"delete","owner":"alice","name":"repo"})
            ))
            .is_err()
        );
    }
    #[test]
    fn refs_queries_and_file_names_cannot_inject_parameters() {
        let path = route(&request(json!({"operation":"transcript","owner":"alice","name":"repo","session":"native id","reference":"feature/a&detail=none","from":3}))).unwrap();
        assert_eq!(
            path,
            "api/agents/alice/repo/sessions/native%20id?ref=feature%2Fa%26detail%3Dnone&from=3&to=13&detail=inline"
        );
        let path = route(&request(
            json!({"operation":"repositories","query":"x&per_page=999","page":0}),
        ))
        .unwrap();
        assert_eq!(path, "api/agents?page=1&per_page=24&q=x%26per_page%3D999");
        let path = route(&request(
            json!({"operation":"sessions","owner":"alice","name":"repo"}),
        ))
        .unwrap();
        assert_eq!(path, "api/agents/alice/repo/sessions?ref=*&page=1&per=20");
    }
}

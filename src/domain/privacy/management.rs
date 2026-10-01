//! User policy references durable recovery records without deleting their originals.

use super::{
    dictionary::{Origin, Record, token_identity},
    policy::{self, Literal, Source},
    projector::Projector,
    storage::Store,
};
use anyhow::{Context, ensure};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::Path;

#[derive(Clone, Serialize, Deserialize)]
pub struct Decision {
    pub scope: String,
    pub token: String,
    pub name: String,
    pub block: Option<bool>,
    pub allow: Option<bool>,
}

#[derive(Deserialize, Serialize)]
pub struct Command {
    pub action: String,
    pub global: bool,
    pub id: Option<String>,
    pub name: Option<String>,
    pub secret: Option<String>,
}

impl Drop for Command {
    fn drop(&mut self) {
        use zeroize::Zeroize;
        if let Some(secret) = &mut self.secret {
            secret.zeroize();
        }
    }
}

pub fn scope(repo: Option<&Path>) -> crate::Result<String> {
    let Some(repo) = repo else {
        return Ok("global".into());
    };
    let common = crate::domain::repo::common_git_dir(repo).canonicalize()?;
    Ok(common.to_string_lossy().into_owned())
}

impl Store {
    pub fn policy_generation(&self) -> crate::Result<i64> {
        Ok(self.db.query_row(
            "SELECT CAST(value AS INTEGER) FROM metadata WHERE key='policy_generation'",
            [],
            |row| row.get(0),
        )?)
    }

    pub fn decisions(&self) -> crate::Result<Vec<Decision>> {
        let mut statement = self.db.prepare(
            "SELECT scope,token,name,block,allow FROM policy ORDER BY scope,token LIMIT 16385",
        )?;
        let rows = statement
            .query_map([], |row| {
                Ok(Decision {
                    scope: row.get(0)?,
                    token: row.get(1)?,
                    name: row.get(2)?,
                    block: row.get(3)?,
                    allow: row.get(4)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        ensure!(
            rows.len() <= 16384,
            "privacy policy exceeds its rule budget"
        );
        Ok(rows)
    }

    pub fn decide(&mut self, decision: &Decision) -> crate::Result<()> {
        ensure!(
            token_identity(&decision.token).is_some()
                && decision.name.len() <= 256
                && decision.scope.len() <= 4096,
            "invalid privacy policy decision"
        );
        let transaction = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let count: i64 =
            transaction.query_row("SELECT COUNT(*) FROM policy", [], |row| row.get(0))?;
        let exists: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM policy WHERE scope=?1 AND token=?2)",
            params![decision.scope, decision.token],
            |row| row.get(0),
        )?;
        ensure!(
            exists || count < 16384,
            "privacy policy exceeds its rule budget"
        );
        transaction.execute("INSERT INTO policy (scope,token,name,block,allow) VALUES (?1,?2,?3,?4,?5)
            ON CONFLICT(scope,token) DO UPDATE SET name=excluded.name,block=excluded.block,allow=excluded.allow",
            params![decision.scope, decision.token, decision.name, decision.block, decision.allow])?;
        transaction.execute(
            "UPDATE metadata SET value=CAST(value AS INTEGER)+1 WHERE key='policy_generation'",
            [],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn adopt_decision(&mut self, decision: &Decision) -> crate::Result<()> {
        let exists: Option<i64> = self
            .db
            .query_row(
                "SELECT 1 FROM policy WHERE scope=?1 AND token=?2",
                params![decision.scope, decision.token],
                |row| row.get(0),
            )
            .optional()?;
        if exists.is_none() {
            self.decide(decision)?;
        }
        Ok(())
    }
}

/// Explicit decisions override inherited policy within their own scope, independent of recovery.
pub fn compile(
    projector: &Projector,
    repo: Option<&Path>,
    inherited_blocks: &[Literal<'_>],
    inherited_allows: &[&str],
) -> crate::Result<policy::Snapshot> {
    let repository = scope(repo)?;
    let decisions = projector.store.decisions()?;
    let rules: Vec<_> = decisions
        .iter()
        .filter(|rule| rule.scope == "global" || rule.scope == repository)
        .filter_map(|rule| {
            projector
                .dictionary
                .get(&rule.token)
                .map(|record| (rule, record))
        })
        .collect();
    let mut blocks: Vec<_> = inherited_blocks
        .iter()
        .filter(|inherited| {
            !rules.iter().any(|(rule, record)| {
                record.original == inherited.value
                    && rule.block == Some(false)
                    && (rule.scope == "global") == (inherited.source == Source::GlobalUser)
            })
        })
        .map(|rule| Literal {
            value: rule.value,
            source: rule.source,
        })
        .collect();
    let mut allows: Vec<_> = inherited_allows
        .iter()
        .copied()
        .filter(|value| {
            !rules
                .iter()
                .any(|(rule, record)| record.original == *value && rule.allow == Some(false))
        })
        .collect();
    for (rule, record) in rules {
        if rule.block == Some(true) {
            blocks.push(Literal {
                value: &record.original,
                source: if rule.scope == "global" {
                    Source::GlobalUser
                } else {
                    Source::RepositoryUser
                },
            });
        }
        if rule.allow == Some(true) {
            allows.push(&record.original);
        }
    }
    policy::Snapshot::compile(&blocks, &allows, true)
}

pub fn execute(
    projector: &mut Projector,
    repo: Option<&Path>,
    command: Command,
) -> crate::Result<Value> {
    let scope = scope(if command.global { None } else { repo })?;
    let decisions = projector.store.decisions()?;
    if command.action == "status" {
        return Ok(
            json!({"initialized":true,"records":projector.dictionary.records().count(),
            "encrypted":projector.key.is_some(),"generation":projector.store.policy_generation()?}),
        );
    }
    if command.action == "fingerprint" {
        use sha2::{Digest, Sha256};
        let mut records = projector.dictionary.records().collect::<Vec<_>>();
        records.sort_by(|left, right| left.token.cmp(&right.token));
        let bytes = zeroize::Zeroizing::new(serde_json::to_vec(&(
            &projector.policy.fingerprint,
            records,
            &decisions,
            projector.complete,
        ))?);
        return Ok(json!(hex::encode(Sha256::digest(&*bytes))));
    }
    if command.action == "values" || command.action == "used_values" {
        return Ok(json!(
            projector
                .dictionary
                .records()
                .filter(|record| command.action == "values"
                    || command
                        .secret
                        .as_deref()
                        .is_some_and(|input| input.contains(&record.token)))
                .map(|record| record.original.as_str())
                .collect::<Vec<_>>()
        ));
    }
    if command.action == "remember" {
        let values: Vec<String> = serde_json::from_str(
            command
                .secret
                .as_deref()
                .context("missing recovery values")?,
        )?;
        for value in values {
            if projector.dictionary.for_value(&value).is_none() {
                let record = Record::new(&projector.store.dictionary_id, &value, Origin::Legacy)?;
                projector
                    .store
                    .append(std::slice::from_ref(&record), projector.key.as_ref())?;
                projector.dictionary.accept(record)?;
            }
        }
        return Ok(json!({"remembered":true}));
    }
    if command.action == "list" || command.action == "review" {
        let mut summaries: Vec<_> = projector.dictionary.records().filter_map(|record| {
            let rule = decisions.iter().find(|rule| rule.scope == scope && rule.token == record.token);
            if command.action == "list" && rule.is_none_or(|rule| rule.block != Some(true)) { return None; }
            Some(json!({"id":token_identity(&record.token).map(|(_,id)|id),"token":record.token,
                "name":rule.map(|rule|rule.name.as_str()).unwrap_or("detected secret"),
                "origins":record.origins,"explicit_block":rule.is_some_and(|rule|rule.block == Some(true)),
                "allowed":rule.is_some_and(|rule|rule.allow == Some(true))}))
        }).collect();
        summaries.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
        return Ok(Value::Array(summaries));
    }
    let token = if command.action == "add" {
        let original = command
            .secret
            .as_deref()
            .context("privacy rule needs a value")?;
        if let Some(record) = projector.dictionary.for_value(original) {
            record.token.clone()
        } else {
            let record = Record::new(
                &projector.store.dictionary_id,
                original,
                if command.global {
                    Origin::GlobalUser
                } else {
                    Origin::RepositoryUser
                },
            )?;
            projector
                .store
                .append(std::slice::from_ref(&record), projector.key.as_ref())?;
            let token = record.token.clone();
            projector.dictionary.accept(record)?;
            token
        }
    } else {
        let id = command
            .id
            .as_deref()
            .context("privacy rule needs an identifier")?;
        let matches: Vec<_> = projector
            .dictionary
            .records()
            .filter(|record| {
                record.token == id
                    || token_identity(&record.token).is_some_and(|(_, record_id)| record_id == id)
                    || decisions.iter().any(|rule| {
                        rule.scope == scope && rule.name == id && rule.token == record.token
                    })
            })
            .collect();
        ensure!(
            matches.len() == 1,
            "privacy identifier is missing or ambiguous"
        );
        matches[0].token.clone()
    };
    let mut decision = decisions
        .into_iter()
        .find(|rule| rule.scope == scope && rule.token == token)
        .unwrap_or(Decision {
            scope,
            token,
            name: "detected secret".into(),
            block: None,
            allow: None,
        });
    match command.action.as_str() {
        "add" => {
            decision.block = Some(true);
            decision.name = command
                .name
                .as_deref()
                .unwrap_or("registered secret")
                .to_owned();
        }
        "remove" => decision.block = Some(false),
        "allow" => decision.allow = Some(true),
        "unallow" => decision.allow = Some(false),
        _ => anyhow::bail!("unknown privacy management action"),
    }
    projector.store.decide(&decision)?;
    Ok(json!({"id":token_identity(&decision.token).map(|(_,id)|id),"name":decision.name}))
}

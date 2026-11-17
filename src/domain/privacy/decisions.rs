//! Explicit blocks form an observed-remove set; allows never leave this device.

use super::crypto::{self, Envelope, Kind, UserKey};
use super::dictionary::{Dictionary, token_id};
use super::management::{Decision, write_decision};
use super::storage::{Store, StoredPackage};
use anyhow::ensure;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

const MAX_EVENTS: usize = 4096;
const MAX_STORED_EVENTS: i64 = 65536;
const MAX_PACKAGES: i64 = 65536;
const MAX_BYTES: i64 = 64 * 1024 * 1024;

pub(super) fn initialize(db: &Connection) -> crate::Result<()> {
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS block_events (
            tag TEXT PRIMARY KEY, scope TEXT NOT NULL, token TEXT NOT NULL,
            name TEXT NOT NULL, is_add INTEGER NOT NULL, payload TEXT NOT NULL,
            local INTEGER NOT NULL, outbox TEXT, resolved INTEGER NOT NULL);
         CREATE INDEX IF NOT EXISTS block_pair ON block_events(scope,token);
         CREATE TABLE IF NOT EXISTS block_removals (
            event TEXT NOT NULL, removed TEXT NOT NULL, PRIMARY KEY(event,removed));
         CREATE INDEX IF NOT EXISTS block_removed ON block_removals(removed);
         CREATE TABLE IF NOT EXISTS policy_outbox (
            id TEXT PRIMARY KEY, payload BLOB NOT NULL, uploaded INTEGER NOT NULL DEFAULT 0);
         CREATE TABLE IF NOT EXISTS policy_receipts (
            id TEXT PRIMARY KEY, sequence INTEGER UNIQUE NOT NULL, payload BLOB NOT NULL);
         CREATE TABLE IF NOT EXISTS policy_members (
            package TEXT NOT NULL, event TEXT NOT NULL, PRIMARY KEY(package,event));
         CREATE TABLE IF NOT EXISTS policy_values (token TEXT PRIMARY KEY);
         CREATE TABLE IF NOT EXISTS policy_repositories (
            common TEXT NOT NULL, root TEXT NOT NULL, selector TEXT, scope TEXT,
            checked INTEGER NOT NULL DEFAULT 0, PRIMARY KEY(common,root));
         INSERT OR IGNORE INTO metadata VALUES ('policy_received','0');
         INSERT OR IGNORE INTO metadata VALUES ('policy_applied','0');
         INSERT OR IGNORE INTO metadata VALUES ('policy_receipt_complete','0');",
    )?;
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Op {
    Add,
    Remove,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Event {
    pub tag: String,
    pub op: Op,
    pub scope: String,
    pub token: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub removes: Vec<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Contents {
    format: u32,
    events: Vec<Event>,
}

pub fn repository_scope(hub: &str, repository: &str) -> crate::Result<String> {
    let repository = uuid::Uuid::parse_str(repository)?;
    let hub = crate::infra::hub_authority::HubAuthority::parse(hub)?.label();
    Ok(format!("repo:{hub}/{repository}"))
}

pub fn travels(scope: &str) -> bool {
    scope == "global"
        || scope.strip_prefix("repo:").is_some_and(|rest| {
            rest.rsplit_once('/').is_some_and(|(hub, id)| {
                repository_scope(&format!("https://{hub}"), id).is_ok_and(|valid| valid == scope)
            })
        })
}

impl Event {
    fn validate(&self) -> crate::Result<()> {
        ensure!(
            uuid::Uuid::parse_str(&self.tag).is_ok()
                && token_id(&self.token).is_some()
                && travels(&self.scope)
                && self.scope.len() <= 4096
                && self.name.len() <= 256
                && self.removes.len() <= MAX_EVENTS
                && self
                    .removes
                    .iter()
                    .all(|tag| uuid::Uuid::parse_str(tag).is_ok() && tag != &self.tag)
                && self.removes.iter().collect::<BTreeSet<_>>().len() == self.removes.len()
                && match self.op {
                    Op::Add => self.removes.is_empty(),
                    Op::Remove => !self.removes.is_empty() && self.name.is_empty(),
                },
            "invalid privacy policy event"
        );
        Ok(())
    }
}

fn budget(db: &Connection) -> crate::Result<()> {
    let bytes: i64 = db.query_row(
        "SELECT COALESCE((SELECT SUM(length(payload)) FROM block_events),0)
              + COALESCE((SELECT SUM(length(payload)) FROM policy_receipts),0)
              + COALESCE((SELECT SUM(length(payload)) FROM policy_outbox),0)",
        [],
        |row| row.get(0),
    )?;
    ensure!(bytes <= MAX_BYTES, "privacy policy journal is full");
    Ok(())
}

/// Identical replay stays a no-op even when the journal cannot accept another event.
fn record(db: &Connection, event: &Event, local: bool) -> crate::Result<()> {
    event.validate()?;
    let payload = serde_json::to_string(event)?;
    let existing: Option<String> = db
        .query_row(
            "SELECT payload FROM block_events WHERE tag=?1",
            [&event.tag],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(existing) = existing {
        ensure!(existing == payload, "conflicting privacy policy event");
        return Ok(());
    }
    let count: i64 = db.query_row("SELECT COUNT(*) FROM block_events", [], |row| row.get(0))?;
    ensure!(
        count < MAX_STORED_EVENTS,
        "privacy policy exceeds its event budget"
    );
    db.execute(
        "INSERT INTO block_events (tag,scope,token,name,is_add,payload,local,resolved)
                VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
        params![
            event.tag,
            event.scope,
            event.token,
            event.name,
            event.op == Op::Add,
            payload,
            local,
            event.op == Op::Remove
        ],
    )?;
    for removed in &event.removes {
        db.execute(
            "INSERT INTO block_removals VALUES (?1,?2)",
            params![event.tag, removed],
        )?;
    }
    Ok(())
}

fn live_adds(db: &Connection, scope: &str, token: &str) -> crate::Result<Vec<(String, String)>> {
    Ok(db
        .prepare(
            "SELECT a.tag,a.name FROM block_events a WHERE a.is_add=1 AND a.scope=?1 AND a.token=?2
         AND NOT EXISTS (SELECT 1 FROM block_removals r JOIN block_events e ON e.tag=r.event
                         WHERE r.removed=a.tag AND e.scope=a.scope AND e.token=a.token)
         ORDER BY a.tag",
        )?
        .query_map(params![scope, token], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<Result<_, _>>()?)
}

fn settle_pair(db: &Connection, scope: &str, token: &str) -> crate::Result<()> {
    let live = live_adds(db, scope, token)?;
    let known: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM policy_values WHERE token=?1)",
        [token],
        |row| row.get(0),
    )?;
    let existing: Option<Decision> = db
        .query_row(
            "SELECT name,block,allow FROM policy WHERE scope=?1 AND token=?2",
            params![scope, token],
            |row| {
                Ok(Decision {
                    scope: scope.into(),
                    token: token.into(),
                    name: row.get(0)?,
                    block: row.get(1)?,
                    allow: row.get(2)?,
                })
            },
        )
        .optional()?;
    let decision = Decision {
        scope: scope.into(),
        token: token.into(),
        name: live
            .last()
            .map(|(_, name)| name.clone())
            .or_else(|| existing.as_ref().map(|d| d.name.clone()))
            .unwrap_or_else(|| "registered secret".into()),
        block: Some(known && !live.is_empty()),
        allow: existing.as_ref().and_then(|d| d.allow),
    };
    if existing.as_ref().is_none_or(|d| {
        d.name != decision.name || d.block != decision.block || d.allow != decision.allow
    }) {
        write_decision(db, &decision)?;
    }
    // A removed addition needs no value. Later revocations can settle while unrelated adds wait.
    db.execute(
        "UPDATE block_events AS a SET resolved=(?3 OR NOT EXISTS (
                    SELECT 1 FROM block_events live WHERE live.tag=a.tag AND NOT EXISTS (
                        SELECT 1 FROM block_removals r JOIN block_events e ON e.tag=r.event
                        WHERE r.removed=live.tag AND e.scope=live.scope AND e.token=live.token)))
                WHERE a.scope=?1 AND a.token=?2 AND a.is_add=1",
        params![scope, token, known],
    )?;
    Ok(())
}

fn progress(db: &Connection) -> crate::Result<()> {
    // Server sequences are shared with dictionary packages; only offered policy entries count.
    db.execute(
        "UPDATE metadata SET value=COALESCE((SELECT MAX(sequence) FROM policy_receipts
        WHERE sequence < COALESCE((SELECT MIN(p.sequence) FROM policy_receipts p
            JOIN policy_members m ON m.package=p.id JOIN block_events e ON e.tag=m.event
            WHERE e.resolved=0),9223372036854775807)),0) WHERE key='policy_applied'",
        [],
    )?;
    Ok(())
}

pub(super) fn decide_in(db: &Connection, decision: &Decision) -> crate::Result<()> {
    if !travels(&decision.scope) || decision.block.is_none() {
        return write_decision(db, decision);
    }
    let mut decision = decision.clone();
    let current_allow: Option<Option<bool>> = db
        .query_row(
            "SELECT allow FROM policy WHERE scope=?1 AND token=?2",
            params![decision.scope, decision.token],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(allow) = current_allow {
        decision.allow = allow;
    }
    let live = live_adds(db, &decision.scope, &decision.token)?;
    if decision.block == Some(true) {
        db.execute(
            "INSERT OR IGNORE INTO policy_values VALUES (?1)",
            [&decision.token],
        )?;
        {
            record(
                db,
                &Event {
                    tag: uuid::Uuid::new_v4().to_string(),
                    op: Op::Add,
                    scope: decision.scope.clone(),
                    token: decision.token.clone(),
                    name: decision.name.clone(),
                    removes: vec![],
                },
                true,
            )?;
        }
    } else {
        for batch in live.chunks(MAX_EVENTS) {
            record(
                db,
                &Event {
                    tag: uuid::Uuid::new_v4().to_string(),
                    op: Op::Remove,
                    scope: decision.scope.clone(),
                    token: decision.token.clone(),
                    name: String::new(),
                    removes: batch.iter().map(|(tag, _)| tag.clone()).collect(),
                },
                true,
            )?;
        }
    }
    write_decision(db, &decision)?;
    settle_pair(db, &decision.scope, &decision.token)?;
    progress(db)?;
    budget(db)
}

#[derive(Default, Serialize)]
pub struct Progress {
    pub received: i64,
    pub applied: i64,
    pub unresolved_packages: usize,
    pub unresolved_rules: usize,
    pub pending_uploads: usize,
    pub receipt_complete: bool,
}

impl Store {
    pub fn policy_progress(&self) -> crate::Result<Progress> {
        let exists: bool = self.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='policy_receipts')",
            [],
            |row| row.get(0),
        )?;
        if !exists {
            return Ok(Progress::default());
        }
        Ok(Progress {
            receipt_complete:self.db.query_row("SELECT CAST(value AS INTEGER) FROM metadata WHERE key='policy_receipt_complete'",[],|row|row.get(0))?,
            received:self.db.query_row("SELECT CAST(value AS INTEGER) FROM metadata WHERE key='policy_received'",[],|row|row.get(0))?,
            applied:self.db.query_row("SELECT CAST(value AS INTEGER) FROM metadata WHERE key='policy_applied'",[],|row|row.get(0))?,
            unresolved_packages:self.db.query_row("SELECT COUNT(DISTINCT m.package) FROM policy_members m JOIN block_events e ON e.tag=m.event WHERE e.resolved=0",[],|row|row.get(0))?,
            unresolved_rules:self.db.query_row("SELECT COUNT(*) FROM (SELECT DISTINCT scope,token FROM block_events WHERE resolved=0)",[],|row|row.get(0))?,
            pending_uploads:self.db.query_row("SELECT (SELECT COUNT(*) FROM block_events e LEFT JOIN policy_outbox o ON e.outbox=o.id WHERE e.local=1 AND (e.outbox IS NULL OR o.uploaded=0)) + (SELECT COUNT(*) FROM policy p WHERE p.scope='global' AND p.block=1 AND NOT EXISTS(SELECT 1 FROM block_events e WHERE e.scope=p.scope AND e.token=p.token))",[],|row|row.get(0))?,
        })
    }

    /// Dictionary availability and the rules it enables become visible in one transaction.
    pub fn resolve_policy(&mut self, dictionary: &Dictionary) -> crate::Result<()> {
        let pairs: Vec<(String, String)> = self
            .db
            .prepare("SELECT DISTINCT scope,token FROM block_events WHERE resolved=0")?
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<Result<_, _>>()?;
        if !pairs
            .iter()
            .any(|(_, token)| dictionary.get(token).is_some())
        {
            return Ok(());
        }
        // Availability is checked before requesting a writer; settlement still reads the live
        // events inside its transaction so a concurrent revocation cannot be overwritten.
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        for (scope, token) in pairs {
            if dictionary.get(&token).is_some() {
                tx.execute("INSERT OR IGNORE INTO policy_values VALUES (?1)", [&token])?;
                settle_pair(&tx, &scope, &token)?;
            }
        }
        progress(&tx)?;
        tx.commit()?;
        Ok(())
    }

    pub fn seed_policy(&mut self) -> crate::Result<()> {
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let decisions: Vec<Decision> = tx.prepare("SELECT scope,token,name,allow FROM policy p
            WHERE block=1 AND scope='global' AND NOT EXISTS(SELECT 1 FROM block_events e WHERE e.scope=p.scope AND e.token=p.token) LIMIT 128")?
            .query_map([],|row|Ok(Decision {scope:row.get(0)?,token:row.get(1)?,name:row.get(2)?,block:Some(true),allow:row.get(3)?}))?
            .collect::<Result<_,_>>()?;
        for decision in decisions {
            decide_in(&tx, &decision)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Seal before sending. A lost acknowledgement retries the same identity and ciphertext.
    pub fn policy_outbox(
        &mut self,
        key: &UserKey,
        additions: bool,
    ) -> crate::Result<Vec<StoredPackage>> {
        self.bind(&key.owner)?;
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let candidates: Vec<String> = tx
            .prepare(
                "SELECT payload FROM block_events WHERE local=1 AND outbox IS NULL
            AND (?1 OR is_add=0) ORDER BY is_add,tag LIMIT ?2",
            )?
            .query_map(params![additions, MAX_EVENTS as i64], |row| row.get(0))?
            .collect::<Result<_, _>>()?;
        let mut contents = Contents {
            format: 1,
            events: vec![],
        };
        let mut bytes = 32;
        for payload in candidates {
            if bytes + payload.len() + 1 > crypto::MAX_PLAINTEXT_BYTES {
                break;
            }
            let event: Event = serde_json::from_str(&payload)?;
            if contents
                .events
                .first()
                .is_some_and(|first| first.op != event.op)
            {
                break;
            }
            bytes += payload.len() + 1;
            contents.events.push(event);
        }
        if !contents.events.is_empty() {
            let id = uuid::Uuid::new_v4().to_string();
            let plaintext = zeroize::Zeroizing::new(serde_json::to_vec(&contents)?);
            let envelope =
                crypto::seal_as(Kind::Policy, key, &self.dictionary_id, &id, &plaintext)?;
            tx.execute(
                "INSERT INTO policy_outbox(id,payload) VALUES (?1,?2)",
                params![id, serde_json::to_vec(&envelope)?],
            )?;
            for event in &contents.events {
                tx.execute(
                    "UPDATE block_events SET outbox=?1 WHERE tag=?2",
                    params![id, event.tag],
                )?;
            }
            budget(&tx)?;
        }
        let rows: Vec<(String,Vec<u8>)> = tx.prepare("SELECT id,payload FROM policy_outbox o WHERE uploaded=0
            AND (?1 OR NOT EXISTS(SELECT 1 FROM block_events e WHERE e.outbox=o.id AND e.is_add=1)) ORDER BY rowid LIMIT 100")?
            .query_map([additions],|row|Ok((row.get(0)?,row.get(1)?)))?.collect::<Result<_,_>>()?;
        let packages = rows
            .into_iter()
            .map(|(id, bytes)| {
                Ok(StoredPackage {
                    id,
                    envelope: serde_json::from_slice(&bytes)?,
                })
            })
            .collect::<crate::Result<_>>()?;
        tx.commit()?;
        Ok(packages)
    }

    pub fn acknowledge_policy(&mut self, id: &str, sequence: i64) -> crate::Result<()> {
        ensure!(sequence > 0, "invalid privacy policy acknowledgement");
        ensure!(
            self.db
                .execute("UPDATE policy_outbox SET uploaded=1 WHERE id=?1", [id])?
                == 1,
            "unknown privacy policy acknowledgement"
        );
        Ok(())
    }

    /// Receipt is atomic even if some additions cannot yet resolve their dictionary references.
    pub fn receive_policy(
        &mut self,
        id: &str,
        sequence: i64,
        after: i64,
        envelope: &Envelope,
        key: &UserKey,
        dictionary: &Dictionary,
    ) -> crate::Result<()> {
        ensure!(
            self.owner() == Some(&key.owner),
            "privacy policy belongs to another account"
        );
        let plaintext = crypto::open_as(Kind::Policy, key, id, envelope)?;
        let contents: Contents = serde_json::from_slice(&plaintext)?;
        ensure!(
            contents.format == 1
                && !contents.events.is_empty()
                && contents.events.len() <= MAX_EVENTS,
            "invalid privacy policy contents"
        );
        for event in &contents.events {
            event.validate()?;
        }
        let payload = serde_json::to_vec(envelope)?;
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing: Option<(i64, Vec<u8>)> = tx
            .query_row(
                "SELECT sequence,payload FROM policy_receipts WHERE id=?1",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if let Some((saved_sequence, saved_payload)) = existing {
            ensure!(
                saved_sequence == sequence && saved_payload == payload,
                "conflicting privacy policy package"
            );
            return Ok(());
        }
        let received: i64 = tx.query_row(
            "SELECT CAST(value AS INTEGER) FROM metadata WHERE key='policy_received'",
            [],
            |row| row.get(0),
        )?;
        ensure!(
            received == after && sequence > after,
            "privacy policy receipt is out of order"
        );
        let count: i64 =
            tx.query_row("SELECT COUNT(*) FROM policy_receipts", [], |row| row.get(0))?;
        ensure!(
            count < MAX_PACKAGES,
            "privacy policy exceeds its package budget"
        );
        tx.execute(
            "INSERT INTO policy_receipts VALUES (?1,?2,?3)",
            params![id, sequence, payload],
        )?;
        let mut pairs = BTreeSet::new();
        for event in &contents.events {
            record(&tx, event, false)?;
            tx.execute(
                "INSERT OR IGNORE INTO policy_members VALUES (?1,?2)",
                params![id, event.tag],
            )?;
            if dictionary.get(&event.token).is_some() {
                tx.execute(
                    "INSERT OR IGNORE INTO policy_values VALUES (?1)",
                    [&event.token],
                )?;
            }
            pairs.insert((&event.scope, &event.token));
        }
        for (scope, token) in pairs {
            settle_pair(&tx, scope, token)?;
        }
        budget(&tx)?;
        tx.execute(
            "UPDATE metadata SET value=?1 WHERE key='policy_received'",
            [sequence.to_string()],
        )?;
        progress(&tx)?;
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::{
        crypto::Owner,
        dictionary::{Label, Origin, Record},
    };
    use super::*;

    /// Missing dictionary values do not reserve a writer. Once a value is available, its block
    /// and applied progress must commit together before the rule can be reported as resolved.
    #[test]
    fn unresolved_rules_only_write_when_their_dictionary_values_become_available() {
        let temp = tempfile::tempdir().unwrap();
        let owner = Owner {
            hub: "fixture.test".into(),
            account: "fixture".into(),
        };
        let key = UserKey::new(owner.clone(), 1, vec![7; 32]).unwrap();
        let mut store = Store::open(&temp.path().join("privacy"), Some(owner)).unwrap();
        let record = Record::new(
            &store.dictionary_id,
            "amber meadow value",
            Origin::GlobalUser,
            Label::secret("explicit-block"),
        )
        .unwrap();
        let event = Event {
            tag: uuid::Uuid::new_v4().to_string(),
            op: Op::Add,
            scope: "global".into(),
            token: record.token.clone(),
            name: "fixture".into(),
            removes: vec![],
        };
        let id = uuid::Uuid::new_v4().to_string();
        let envelope = crypto::seal_as(
            Kind::Policy,
            &key,
            &store.dictionary_id,
            &id,
            &serde_json::to_vec(&Contents {
                format: 1,
                events: vec![event],
            })
            .unwrap(),
        )
        .unwrap();
        let mut dictionary = Dictionary::default();
        store
            .receive_policy(&id, 3, 0, &envelope, &key, &dictionary)
            .unwrap();
        let generation = store.policy_generation().unwrap();
        let lock = Connection::open(temp.path().join("privacy/journal.sqlite")).unwrap();
        lock.execute_batch("BEGIN IMMEDIATE").unwrap();
        store
            .resolve_policy(&dictionary)
            .expect("unavailable values need no write");
        dictionary.accept(record).unwrap();
        assert!(store.resolve_policy(&dictionary).is_err());
        assert_eq!(store.policy_generation().unwrap(), generation);
        assert_eq!(store.policy_progress().unwrap().applied, 0);
        assert_eq!(store.policy_progress().unwrap().unresolved_rules, 1);
        lock.execute_batch("ROLLBACK").unwrap();
        store.resolve_policy(&dictionary).unwrap();
        assert_eq!(store.policy_progress().unwrap().applied, 3);
        assert_eq!(store.policy_progress().unwrap().unresolved_rules, 0);
        assert_eq!(store.decisions().unwrap()[0].block, Some(true));
        lock.execute_batch("BEGIN IMMEDIATE").unwrap();
        store
            .resolve_policy(&dictionary)
            .expect("resolved rules need no write");
        lock.execute_batch("ROLLBACK").unwrap();
    }

    /// Receipt, events, effects and progress commit together. Replay at capacity cannot turn a
    /// durable acknowledgement into a permanent failure, while a reused tag cannot change meaning.
    #[test]
    fn package_acceptance_is_atomic_and_replay_at_capacity_is_idempotent() {
        let temp = tempfile::tempdir().unwrap();
        let owner = Owner {
            hub: "fixture.test".into(),
            account: "fixture".into(),
        };
        let key = UserKey::new(owner.clone(), 1, vec![7; 32]).unwrap();
        let mut store = Store::open(&temp.path().join("privacy"), Some(owner)).unwrap();
        let mut dictionary = Dictionary::default();
        let mut events = Vec::new();
        for original in ["amber meadow value", "violet harbor value"] {
            let record = Record::new(
                &store.dictionary_id,
                original,
                Origin::GlobalUser,
                Label::secret("explicit-block"),
            )
            .unwrap();
            events.push(Event {
                tag: uuid::Uuid::new_v4().to_string(),
                op: Op::Add,
                scope: "global".into(),
                token: record.token.clone(),
                name: "fixture".into(),
                removes: vec![],
            });
            dictionary.accept(record).unwrap();
        }
        let seal = |events: Vec<Event>| {
            let id = uuid::Uuid::new_v4().to_string();
            let plaintext = serde_json::to_vec(&Contents { format: 1, events }).unwrap();
            let envelope = crypto::seal_as(
                Kind::Policy,
                &key,
                &uuid::Uuid::new_v4().to_string(),
                &id,
                &plaintext,
            )
            .unwrap();
            (id, envelope)
        };
        store
            .db
            .execute_batch(
                "WITH RECURSIVE n(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM n WHERE i<16383)
            INSERT INTO policy(scope,token,name) SELECT 'global','filler-'||i,'' FROM n",
            )
            .unwrap();
        let (id, envelope) = seal(events.clone());
        assert!(
            store
                .receive_policy(&id, 3, 0, &envelope, &key, &dictionary)
                .is_err()
        );
        assert_eq!(store.policy_generation().unwrap(), 0);
        assert_eq!(store.policy_progress().unwrap().received, 0);
        assert_eq!(
            store
                .db
                .query_row("SELECT COUNT(*) FROM block_events", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
        store.db.execute("DELETE FROM policy", []).unwrap();
        store
            .receive_policy(&id, 3, 0, &envelope, &key, &dictionary)
            .unwrap();
        assert!(
            store
                .decisions()
                .unwrap()
                .iter()
                .all(|d| d.block == Some(true))
        );
        store
            .db
            .execute_batch(
                "WITH RECURSIVE n(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM n WHERE i<65534)
            INSERT INTO block_events(tag,scope,token,name,is_add,payload,local,resolved)
            SELECT 'filler-'||i,'local','filler','','0','{}',0,1 FROM n",
            )
            .unwrap();
        let generation = store.policy_generation().unwrap();
        store
            .receive_policy(&id, 3, 0, &envelope, &key, &dictionary)
            .unwrap();
        let (replay, envelope) = seal(events.clone());
        store
            .receive_policy(&replay, 9, 3, &envelope, &key, &dictionary)
            .unwrap();
        assert_eq!(store.policy_generation().unwrap(), generation);
        assert_eq!(store.policy_progress().unwrap().applied, 9);
        events[0].name = "different meaning".into();
        let (conflict, envelope) = seal(events);
        assert!(
            store
                .receive_policy(&conflict, 12, 9, &envelope, &key, &dictionary)
                .is_err()
        );
        assert_eq!(store.policy_progress().unwrap().received, 9);
        let other = UserKey::new(
            Owner {
                hub: key.owner.hub.clone(),
                account: "other-account".into(),
            },
            1,
            vec![7; 32],
        )
        .unwrap();
        assert!(
            store
                .receive_policy(&conflict, 12, 9, &envelope, &other, &dictionary)
                .is_err()
        );
        assert_eq!(
            store.policy_progress().unwrap().received,
            9,
            "another account cannot advance this journal"
        );
    }
}

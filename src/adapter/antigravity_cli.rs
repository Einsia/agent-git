use super::{
    Adapter, Capability, Event, EventKind, Installed, RecordClass, Session, SessionRef,
    native_snapshot,
};
use crate::Result;
use anyhow::{Context, bail, ensure};
use serde::Deserialize;
use std::path::{Path, PathBuf};

pub struct AntigravityCli;

const FORMAT: &str = "antigravity-cli-text-turn-v1";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TextTurn {
    format: String,
    conversation_id: String,
    turn: u64,
    prompt: String,
    reply: String,
}

fn canonical_id(id: &str) -> bool {
    uuid::Uuid::parse_str(id).is_ok_and(|parsed| parsed.to_string() == id)
}

impl Adapter for AntigravityCli {
    fn id(&self) -> &'static str {
        "antigravity-cli"
    }

    fn cli(&self) -> &'static str {
        "agy"
    }

    fn capability(&self) -> Capability {
        Capability::ImportOnly
    }

    fn format(&self) -> &'static str {
        FORMAT
    }

    fn sessions_for(&self, _repo: &Path) -> Result<Vec<SessionRef>> {
        Ok(Vec::new())
    }

    fn all_sessions(&self) -> Result<Vec<SessionRef>> {
        Ok(Vec::new())
    }

    fn resolve(&self, id: &str, _cwd: Option<&Path>) -> Option<PathBuf> {
        self.lookup_native_readonly(id, native_snapshot::Limits::default())
            .ok()
            .map(|source| source.path)
    }

    fn lookup_native_readonly(
        &self,
        id: &str,
        limits: native_snapshot::Limits,
    ) -> native_snapshot::Result<native_snapshot::Source> {
        native_snapshot::validate_id(id, limits)?;
        if !canonical_id(id) {
            return Err(native_snapshot::Unavailable::NotFound);
        }
        let home =
            crate::infra::config::agit_home().map_err(|_| native_snapshot::Unavailable::Read)?;
        let path = home
            .join("captures")
            .join(self.id())
            .join(format!("{id}.jsonl"));
        native_snapshot::source_at(self.id(), id, path)
    }

    fn snapshot_native_readonly(
        &self,
        source: &native_snapshot::Source,
        limits: native_snapshot::Limits,
    ) -> native_snapshot::Result<native_snapshot::Snapshot> {
        if *source != self.lookup_native_readonly(&source.session_id, limits)? {
            return Err(native_snapshot::Unavailable::Read);
        }
        let snapshot = native_snapshot::read_file(source, limits)?;
        let text = std::str::from_utf8(&snapshot.bytes)
            .map_err(|_| native_snapshot::Unavailable::Incomplete)?;
        let first = text
            .lines()
            .next()
            .and_then(|line| serde_json::from_str::<TextTurn>(line).ok());
        if !first.is_some_and(|turn| turn.turn == 1) {
            return Err(native_snapshot::Unavailable::Incomplete);
        }
        let session = self
            .parse(text)
            .map_err(|_| native_snapshot::Unavailable::Incomplete)?;
        if session.id != source.session_id {
            return Err(native_snapshot::Unavailable::Read);
        }
        Ok(snapshot)
    }

    fn read_native_bytes_at(&self, id: &str, path: &Path) -> Result<Vec<u8>> {
        let limits = native_snapshot::Limits::default();
        let source = self.lookup_native_readonly(id, limits)?;
        ensure!(source.path == path, "selected capture path does not match");
        Ok(self.snapshot_native_readonly(&source, limits)?.bytes)
    }

    fn record_class(&self, record: &serde_json::Value) -> RecordClass {
        if record["format"] == FORMAT {
            RecordClass::Content
        } else {
            RecordClass::Unknown
        }
    }

    fn parse(&self, text: &str) -> Result<Session> {
        let mut events = Vec::new();
        let mut identity = None;
        let mut last = None;
        for (line, raw) in text.lines().enumerate() {
            let turn: TextTurn = serde_json::from_str(raw)
                .map_err(|_| anyhow::anyhow!("invalid Antigravity CLI text turn"))?;
            ensure!(
                turn.format == FORMAT
                    && canonical_id(&turn.conversation_id)
                    && turn.turn > 0
                    && !turn.prompt.trim().is_empty()
                    && !turn.reply.trim().is_empty(),
                "incomplete Antigravity CLI text turn"
            );
            ensure!(
                identity.get_or_insert_with(|| turn.conversation_id.clone())
                    == &turn.conversation_id
                    && last.is_none_or(|ordinal: u64| ordinal.checked_add(1) == Some(turn.turn)),
                "Antigravity CLI text turns do not form one ordered conversation"
            );
            last = Some(turn.turn);
            events.push(Event::text(EventKind::UserPrompt, turn.prompt, None).at_line(line));
            events.push(Event::text(EventKind::AssistantReply, turn.reply, None).at_line(line));
            events.push(Event::text(EventKind::TurnEnd, "", None).at_line(line));
        }
        Ok(Session {
            id: identity.context("empty Antigravity CLI capture")?,
            runtime: self.id().into(),
            cwd: None,
            events,
        })
    }

    fn render(&self, _session: &Session, _id: &str, _cwd: &Path) -> Result<String> {
        bail!("Antigravity CLI text captures are import-only")
    }

    fn mint_id(&self) -> String {
        uuid::Uuid::new_v4().to_string()
    }

    fn install(&self, _content: &str, _id: &str, _cwd: &Path) -> Result<Installed> {
        bail!("Antigravity CLI text captures are import-only")
    }
}

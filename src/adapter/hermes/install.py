"""Restore a fresh transcript through the installed Hermes storage implementation."""
import json
import sys
from pathlib import Path
from hermes_state import SessionDB

payload = json.load(sys.stdin)
records = [json.loads(line) for line in payload["content"].splitlines() if line.strip()]
header = records[0]["data"]
messages = [record["data"] for record in records[1:] if record["type"] == "hermes_message"]
session_id = payload["id"]
db = SessionDB()


def install(conn):
    # Identity admission and all transcript writes share the native writer transaction.
    if conn.execute("SELECT 1 FROM sessions WHERE id = ?", (session_id,)).fetchone():
        raise ValueError("Hermes session already exists; refusing to overwrite it")
    prompt_hash = db._store_system_prompt(conn, header.get("system_prompt"))
    conn.execute(
        "INSERT INTO sessions (id, source, cwd, started_at, model, model_config, "
        "system_prompt_hash, profile_name) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        (session_id, "cli", payload["cwd"], header.get("started_at", 0),
         header.get("model"), header.get("model_config"), prompt_hash, db._own_profile_name()),
    )
    columns = {row[1] for row in conn.execute("PRAGMA table_info(messages)")}
    for original in messages:
        message = dict(original)
        message["content"] = db._decode_content(message.get("content"))
        for field in ("tool_calls", "reasoning_details", "codex_reasoning_items", "codex_message_items", "display_metadata"):
            if isinstance(message.get(field), str):
                message[field] = json.loads(message[field])
        inserted, tools = db._insert_message_rows(conn, session_id, [message])
        db._bump_session_counters(conn, session_id, inserted, tools, unit=False)
        preserved = {key: (bytes.fromhex(value["$sqlite_blob_hex"])
                           if isinstance(value, dict) and set(value) == {"$sqlite_blob_hex"}
                           else value) for key, value in original.items()
                     if key not in ("id", "session_id", "display_identity")}
        if not preserved.keys() <= columns:
            raise ValueError("Installed Hermes cannot preserve all archived message columns")
        if preserved:
            assignments = ", ".join('"' + key.replace('"', '""') + '" = ?' for key in preserved)
            conn.execute("UPDATE messages SET " + assignments + " WHERE id = ? AND session_id = ?",
                         (*preserved.values(), message["_row_id"], session_id))


try:
    db._execute_write(install)
finally:
    db.close()

"""Merge AgentGit integration through Hermes' native configuration writer."""
import json
import shlex
import sys
from hermes_cli.config import get_config_path, read_raw_config, save_config, require_readable_config_before_write

payload = json.load(sys.stdin)
require_readable_config_before_write(get_config_path())
config = read_raw_config() or {}
if not isinstance(config, dict):
    raise ValueError("Hermes configuration must be a mapping")
if payload["kind"] == "hooks":
    hooks = config.setdefault("hooks", {})
    if not isinstance(hooks, dict):
        raise ValueError("Hermes hooks must be a mapping")
    for event, action in (("on_session_start", "ingest"), ("on_session_end", "settle")):
        entries = hooks.setdefault(event, [])
        if not isinstance(entries, list):
            raise ValueError("Hermes hook entries must be a list")
        command = shlex.join([payload["exe"], "hooks", action, "--runtime", "hermes"])
        hooks[event] = [entry for entry in entries if not isinstance(entry, dict) or entry.get("name") != "agit"]
        hooks[event].append({"name": "agit", "command": command, "timeout": 60})
else:
    import importlib.util
    if importlib.util.find_spec("mcp") is None:
        raise RuntimeError("Hermes MCP support is not installed; install Hermes with its mcp optional dependencies")
    servers = config.setdefault("mcp_servers", {})
    if not isinstance(servers, dict):
        raise ValueError("Hermes MCP servers must be a mapping")
    servers["agit"] = {"command": payload["exe"], "args": ["mcp"]}
save_config(config, strip_defaults=False)
written = read_raw_config()
section = "hooks" if payload["kind"] == "hooks" else "mcp_servers"
if written.get(section) != config[section]:
    raise ValueError("Hermes did not persist the requested integration")

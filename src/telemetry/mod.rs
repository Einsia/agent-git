//! Product analytics accepts only properties constructed by the reviewed field registry.

pub mod acquisition;
mod campaign;
pub mod installation;
mod rc;
pub mod schema;
pub mod state;
pub mod transport;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::{
    ffi::OsString,
    io::IsTerminal,
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicI64, Ordering},
    },
    time::{Duration, Instant},
};
use transport::{Destination, Event, EventName};

static RUN: Mutex<Option<Run>> = Mutex::new(None);
static ONLINE: AtomicBool = AtomicBool::new(false);
static LAST_WORKER: AtomicI64 = AtomicI64::new(0);

pub const RESTART_ENV: &str = "AGIT_INTERNAL_TELEMETRY_RESTART";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Restart {
    invocation_id: uuid::Uuid,
    prompt_shown: bool,
    started_event: bool,
    elapsed_ms: u64,
}

#[derive(Clone)]
struct Seed {
    properties: Map<String, Value>,
    hub: String,
    background: bool,
    hook: bool,
    protocol: bool,
}
#[derive(Clone)]
struct Context {
    properties: Map<String, Value>,
    distinct_id: String,
    generation: u64,
    destination: Destination,
    debug: bool,
    protocol: bool,
    hub: String,
    account: Option<String>,
    consent_device: Option<uuid::Uuid>,
    identity_state: &'static str,
}
struct Run {
    seed: Seed,
    context: Option<Context>,
    identity_update: Option<(Option<String>, &'static str)>,
    started: Instant,
    started_event: bool,
}

#[derive(Serialize, Deserialize)]
struct Activity {
    principal: String,
    anonymous: uuid::Uuid,
    device: uuid::Uuid,
    session: uuid::Uuid,
    last_active: i64,
    day: i64,
}

fn present(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|value| !value.is_empty())
}

fn build_version() -> &'static str {
    let value = crate::infra::config::BUILD_VERSION;
    let (core, suffix) = value
        .split_once('-')
        .map_or((value, None), |(core, suffix)| (core, Some(suffix)));
    let numbers = core.split('.').collect::<Vec<_>>();
    let core_valid = numbers.len() == 3
        && numbers.iter().all(|part| {
            !part.is_empty() && part.len() <= 6 && part.bytes().all(|b| b.is_ascii_digit())
        });
    let suffix_valid = suffix.is_none_or(|suffix| {
        let (label, number) = suffix.split_once('.').unwrap_or((suffix, "0"));
        matches!(label, "rc" | "alpha" | "beta")
            && !number.is_empty()
            && number.len() <= 6
            && number.bytes().all(|b| b.is_ascii_digit())
    });
    if core_valid && suffix_valid {
        value
    } else {
        env!("CARGO_PKG_VERSION")
    }
}

fn seed(argv: &[OsString]) -> Seed {
    let mut properties = schema::properties(argv);
    let command = properties
        .get("command")
        .and_then(Value::as_str)
        .unwrap_or("unparsed")
        .to_owned();
    let matches = crate::commands::cli_def()
        .ignore_errors(true)
        .try_get_matches_from(argv)
        .ok();
    let hub = matches
        .as_ref()
        .and_then(|m| m.subcommand())
        .filter(|(name, _)| *name == "login")
        .and_then(|(_, m)| m.try_get_one::<String>("hub").ok().flatten())
        .cloned()
        .unwrap_or_else(crate::infra::config::hub_url);
    let runtimes = crate::infra::runtime_session::ENV_SESSIONS
        .iter()
        .filter(|(name, _)| present(name))
        .map(|(_, runtime)| *runtime)
        .collect::<std::collections::BTreeSet<_>>();
    let hook = command == "hooks"
        || properties.get("arg_from_hook") == Some(&json!(true))
        || properties.get("arg_from_supervisor") == Some(&json!(true));
    let protocol = command == "mcp" || state::positive("AGIT_PROTOCOL_CHILD") || hook;
    let source = if hook {
        "hook"
    } else if command == "mcp" || state::positive("AGIT_PROTOCOL_CHILD") {
        "mcp"
    } else if present("AGIT_RC") || command == "rc" {
        "rc"
    } else if command == "setup" {
        "installer"
    } else if !runtimes.is_empty() || present("AGIT_SESSION") {
        "agent"
    } else {
        "direct"
    };
    let background = hook
        || command == "mcp"
        || present("AGIT_RC")
        || properties.get("command_path") == Some(&json!("rc start"));
    let runtime = match runtimes.len() {
        0 => "unknown",
        1 => runtimes.iter().next().copied().unwrap_or("unknown"),
        _ => "multiple",
    };
    let stdin_tty = std::io::stdin().is_terminal();
    let stdout_tty = std::io::stdout().is_terminal();
    let ci = present("CI") || present("GITHUB_ACTIONS") || present("GITLAB_CI");
    let locale = std::env::var("LC_ALL")
        .or_else(|_| std::env::var("LANG"))
        .unwrap_or_default();
    let language = locale
        .split(['_', '-', '.'])
        .next()
        .filter(|v| {
            [
                "en", "zh", "ja", "ko", "de", "fr", "es", "pt", "ru", "it", "nl", "pl", "tr", "ar",
                "hi",
            ]
            .contains(v)
        })
        .unwrap_or("unknown");
    properties.extend(json!({
        "schema_version": 1, "client_type": "cli", "cli_version": build_version(), "pver": build_version(),
        "release_channel": if cfg!(debug_assertions) {"development"} else if build_version().contains('-') {"prerelease"} else {"stable"},
        "source": source, "source_confidence": if source == "agent" {"environment_hint"} else {"explicit"},
        "background": background, "runtime_env": runtime,
        "stdin_tty": stdin_tty, "stdout_tty": stdout_tty, "stderr_tty": std::io::stderr().is_terminal(),
        "prompt_capable": stdin_tty && stdout_tty, "prompt_shown": false,
        "ci": ci, "ci_provider": if present("GITHUB_ACTIONS") {"github_actions"} else if present("GITLAB_CI") {"gitlab_ci"} else if ci {"other"} else {"none"},
        "agit_session_env_present": present("AGIT_SESSION"), "agit_session_id_env_present": present("AGIT_SESSION_ID"),
        "agit_session_state": if !present("AGIT_SESSION") {"absent"} else if crate::infra::runtime_session::has_managed_env() {"syntactically_valid"} else {"invalid"},
        "managed_context": "unknown", "agit_merge_env_present": present("AGIT_MERGE_TX"),
        "rc_mode": present("AGIT_RC"), "protocol_child": state::positive("AGIT_PROTOCOL_CHILD"),
        "os": std::env::consts::OS, "arch": std::env::consts::ARCH, "language": language,
        "cpu_count_bucket": std::thread::available_parallelism().map(|v|schema::bucket(v.get() as u64)).unwrap_or("unknown"),
        "sample_rate": 1, "$geoip_disable": true,
        "output_mode": if protocol {"protocol"} else if properties.get("arg_json") == Some(&json!(true)) {"json"} else {"text"},
        "invocation_id": uuid::Uuid::new_v4(), "failure_stage": "dispatch"
    }).as_object().unwrap().clone());
    if let Some(parent) = std::env::var("AGIT_TELEMETRY_PARENT_ID")
        .ok()
        .and_then(|v| uuid::Uuid::parse_str(&v).ok())
    {
        properties.insert("parent_invocation_id".into(), json!(parent));
    }
    if let Some(tool) = std::env::var("AGIT_MCP_TOOL")
        .ok()
        .filter(|v| MCP_TOOLS.contains(&v.as_str()))
    {
        properties.insert("mcp_tool".into(), json!(tool));
    }
    Seed {
        properties,
        hub,
        background,
        hook,
        protocol,
    }
}

fn context(seed: &Seed) -> anyhow::Result<Option<(Context, bool)>> {
    let mut preferences = state::read()?;
    if !state::enabled(&preferences) {
        return Ok(None);
    }
    let Some(destination) = Destination::for_hub(&seed.hub) else {
        return Ok(None);
    };
    let debug = state::positive("AGIT_TELEMETRY_DEBUG");
    let (account, identity_state) = crate::infra::credentials::analytics_account(&seed.hub);
    let principal = format!(
        "{}:{}",
        destination.route,
        account.as_deref().unwrap_or("anonymous")
    );
    let now = chrono::Utc::now().timestamp_millis();
    let day = now / 86_400_000;
    let dir = state::directory()?;
    let _guard = if debug {
        None
    } else {
        Some(state::gate(&dir, false)?)
    };
    if !debug {
        let current = state::read_at(&dir)?;
        if !state::enabled(&current)
            || current.generation != preferences.generation
            || current.device_id != preferences.device_id
        {
            return Ok(None);
        }
        preferences = current;
    }
    let old = if debug {
        None
    } else {
        state::read_json::<Activity>(&dir.join("activity.json"), 4096)?
    };
    let first = old.is_none();
    let changed = old.as_ref().is_none_or(|old| old.principal != principal);
    let mut activity = old.unwrap_or_else(|| Activity {
        principal: principal.clone(),
        anonymous: uuid::Uuid::new_v4(),
        device: preferences.device_id.unwrap_or_else(uuid::Uuid::new_v4),
        session: uuid::Uuid::new_v4(),
        last_active: 0,
        day,
    });
    if changed && !first {
        activity.anonymous = uuid::Uuid::new_v4();
        activity.device = uuid::Uuid::new_v4();
    }
    let new_session = changed
        || (!seed.background
            && (now - activity.last_active > 1_800_000
                || activity.day != day
                || now < activity.last_active));
    if new_session {
        activity.session = uuid::Uuid::new_v4();
    }
    activity.principal = principal;
    if !seed.background {
        activity.last_active = now;
        activity.day = day;
    }
    if !debug {
        state::write_json(&dir.join("activity.json"), &activity)?;
    }
    let distinct_id = account
        .clone()
        .map(|account| {
            if destination.environment == "production" {
                account
            } else {
                format!("{}:{account}", destination.environment)
            }
        })
        .unwrap_or_else(|| format!("cli-anonymous:{}", activity.anonymous));
    let mut properties = seed.properties.clone();
    properties.extend(json!({ "identity_state": identity_state, "device_id": activity.device, "session_id": activity.session,
        "is_first_visit": first && !seed.background, "channel": match preferences.channel.as_str() {"create_agit"|"npm_global"|"source"|"archive" => preferences.channel.as_str(),_=>"unknown"}, "notice_version": preferences.notice_version,
        "telemetry_mode": if matches!(preferences.decision_source, Some(state::DecisionSource::ExplicitEnable)) {"explicit_enable"} else {"default_on"},
        "decision_source": preferences.decision_source, "app_env": if destination.environment == "production" {"production"} else {"development"},
        "deployment_env": destination.environment, "hub_class": if matches!(destination.environment,"production"|"staging") {"official"} else {"other"},
        "$process_person_profile": account.is_some()
    }).as_object().unwrap().clone());
    if let Some(account) = &account {
        properties.insert("user_id".into(), json!(account));
    }
    if seed.properties.get("command") == Some(&json!("login"))
        && preferences.first_acquisition_account.is_none()
        && if debug {
            preferences.acquisition_route.as_ref() == Some(&destination.route)
        } else {
            acquisition::bind_route(&mut preferences, &dir, &destination)?
        }
    {
        acquisition::extend_properties(&preferences, &mut properties);
    }
    Ok(Some((
        Context {
            properties,
            distinct_id,
            generation: preferences.generation,
            destination,
            debug,
            protocol: seed.protocol,
            hub: seed.hub.clone(),
            account,
            consent_device: preferences.device_id,
            identity_state,
        },
        new_session && !seed.background,
    )))
}

fn emit(context: &Context, event: EventName, extra: Map<String, Value>) {
    if state::override_reason().is_some() {
        return;
    }
    let Ok(preferences) = state::read() else {
        return;
    };
    if !state::enabled(&preferences)
        || preferences.generation != context.generation
        || preferences.device_id != context.consent_device
    {
        return;
    }
    if crate::infra::credentials::analytics_account(&context.hub)
        != (context.account.clone(), context.identity_state)
    {
        return;
    }
    let mut properties = context.properties.clone();
    properties.extend(extra);
    let uuid = uuid::Uuid::new_v4();
    let timestamp = chrono::Utc::now();
    properties.insert("event_id".into(), json!(uuid));
    properties.insert("event_ts".into(), json!(timestamp.timestamp_millis()));
    let event = Event {
        event,
        uuid,
        distinct_id: context.distinct_id.clone(),
        timestamp,
        properties,
    };
    if context.debug {
        if !context.protocol {
            eprintln!("{}", json!({"telemetry_preview": event}));
        }
    } else {
        let _ = transport::enqueue(event, context.generation, &context.destination);
    }
}

pub fn begin(argv: &[OsString], restart: Option<&str>) {
    let restart = restart
        .filter(|value| value.len() <= 512)
        .and_then(|value| serde_json::from_str::<Restart>(value).ok());
    let mut seed = seed(argv);
    if let Some(restart) = &restart {
        seed.properties
            .insert("invocation_id".into(), json!(restart.invocation_id));
        seed.properties
            .insert("prompt_shown".into(), json!(restart.prompt_shown));
    }
    if seed.properties.get("command") == Some(&json!("telemetry")) {
        return;
    }
    let parse_ok = crate::commands::cli_def()
        .try_get_matches_from(argv)
        .is_ok();
    let command = seed
        .properties
        .get("command")
        .and_then(Value::as_str)
        .unwrap_or("unparsed");
    if parse_ok
        && !matches!(command, "setup" | "bare" | "unparsed")
        && !seed.background
        && !seed.protocol
        && std::io::stderr().is_terminal()
        && !present("CI")
        && state::override_reason().is_none()
        && state::read().is_ok_and(|p| p.preference == state::Preference::Unset)
    {
        eprintln!("{}\n{}", state::DISCLOSURE, state::ENABLED_NOTICE);
        let _ = state::choose(
            state::Preference::Enabled,
            state::DecisionSource::FirstInvocation,
            true,
        );
    }
    if let Ok(mut run) = RUN.lock() {
        *run = Some(Run {
            seed,
            context: None,
            identity_update: None,
            started: restart
                .as_ref()
                .and_then(|restart| {
                    Instant::now().checked_sub(Duration::from_millis(restart.elapsed_ms))
                })
                .unwrap_or_else(Instant::now),
            started_event: restart.is_some_and(|restart| restart.started_event),
        });
    }
    activate(false);
}

pub fn activate(onboarding: bool) {
    if onboarding {
        allow_uploads();
    }
    let seed = RUN.lock().ok().and_then(|run| {
        run.as_ref()
            .filter(|run| run.context.is_none())
            .map(|run| (run.seed.clone(), run.started_event))
    });
    let Some((seed, started_event)) = seed else {
        return;
    };
    let _ = acquisition::resume_pending(&seed.hub);
    let Some((context, new_session)) = context(&seed).ok().flatten() else {
        return;
    };
    if new_session && !started_event {
        emit(&context, EventName::Session, Map::new());
    }
    if onboarding {
        emit(&context, EventName::Onboarding, Map::new());
        ONLINE.store(true, Ordering::Relaxed);
    }
    if !seed.hook && !started_event {
        emit(&context, EventName::Started, Map::new());
    }
    if let Ok(mut run) = RUN.lock()
        && let Some(run) = run.as_mut()
    {
        run.context = Some(context);
        run.started_event = !seed.hook;
    }
}

/// A replacement process continues the invocation; it must not emit another start or lose prompts.
pub fn configure_restart(command: &mut std::process::Command) {
    command.env_remove(RESTART_ENV);
    let restart = RUN.lock().ok().and_then(|run| {
        let run = run.as_ref()?;
        Some(Restart {
            invocation_id: uuid::Uuid::parse_str(
                run.seed.properties.get("invocation_id")?.as_str()?,
            )
            .ok()?,
            prompt_shown: run.seed.properties.get("prompt_shown") == Some(&json!(true)),
            started_event: run.started_event,
            elapsed_ms: run.started.elapsed().as_millis().min(u64::MAX as u128) as u64,
        })
    });
    if let Some(restart) = restart.and_then(|restart| serde_json::to_string(&restart).ok()) {
        command.env(RESTART_ENV, restart);
    }
}

pub fn finish(code: i32) {
    let run = RUN.lock().ok().and_then(|mut run| run.take());
    let Some(mut run) = run else {
        return;
    };
    if matches!(
        run.seed.properties.get("command").and_then(Value::as_str),
        Some("login" | "whoami")
    ) && code == 0
        && run.context.as_ref().is_some_and(|context| {
            state::read().is_ok_and(|preferences| {
                state::enabled(&preferences)
                    && preferences.generation == context.generation
                    && preferences.device_id == context.consent_device
            })
        })
        && let Some(expected) = &run.identity_update
        && let Some((mut fresh, _)) = context(&run.seed).ok().flatten()
        && (&fresh.account, fresh.identity_state) == (&expected.0, expected.1)
    {
        if let Some(old) = &run.context {
            for key in ["prompt_shown", "failure_stage", "authentication_outcome"] {
                if let Some(value) = old.properties.get(key) {
                    fresh.properties.insert(key.into(), value.clone());
                }
            }
        }
        run.context = Some(fresh);
    }
    if let Some(context) = run.context {
        let extras = json!({"exit_code": code, "exit_category": exit_category(code), "duration_bucket": duration_bucket(run.started.elapsed().as_millis()), "integration_count": 1, "failure_stage": if code == 0 {Value::Null} else {context.properties.get("failure_stage").cloned().unwrap_or(Value::Null)}});
        emit(
            &context,
            if run.seed.hook {
                EventName::Integration
            } else {
                EventName::Finished
            },
            extras.as_object().unwrap().clone(),
        );
        maybe_upload(&context.hub);
    }
}

/// Only identity saved by this invocation may change its attribution after authentication.
pub fn account_saved(hub: &str, account_id: Option<&str>) {
    if account_id.is_some_and(|id| {
        id.is_empty()
            || id.len() > 128
            || !id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    }) {
        return;
    }
    let Ok(authority) = crate::infra::hub_authority::HubAuthority::parse(hub) else {
        return;
    };
    if let Ok(mut run) = RUN.lock()
        && let Some(run) = run.as_mut()
        && crate::infra::hub_authority::HubAuthority::parse(&run.seed.hub)
            .is_ok_and(|selected| selected.storage_key() == authority.storage_key())
    {
        run.identity_update = Some((
            account_id.map(str::to_owned),
            if account_id.is_some() {
                "identified"
            } else {
                "signed_in_id_missing"
            },
        ));
    }
}

pub fn allow_uploads() {
    if RUN
        .lock()
        .ok()
        .and_then(|run| {
            run.as_ref().map(|run| {
                let command = run
                    .seed
                    .properties
                    .get("command")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                matches!(
                    command,
                    "status" | "log" | "view" | "diff" | "config" | "secrets"
                ) || (command == "search"
                    && run.seed.properties.get("arg_local") == Some(&json!(true)))
                    || (command == "doctor"
                        && run.seed.properties.get("arg_check_backend") != Some(&json!(true)))
                    || (command == "import"
                        && (run.seed.properties.get("arg_link_only") == Some(&json!(true))
                            || run.seed.properties.get("arg_propose_lineage")
                                == Some(&json!(true))))
            })
        })
        .unwrap_or(true)
    {
        return;
    }
    ONLINE.store(true, Ordering::Relaxed);
}
fn maybe_upload(hub: &str) {
    if !ONLINE.load(Ordering::Relaxed) {
        return;
    }
    let now = chrono::Utc::now().timestamp_millis();
    let previous = LAST_WORKER.load(Ordering::Relaxed);
    if now - previous >= 30_000
        && LAST_WORKER
            .compare_exchange(previous, now, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
    {
        transport::spawn_worker(hub);
    }
}

pub fn parent_invocation_id() -> Option<String> {
    RUN.lock().ok().and_then(|run| {
        run.as_ref()
            .and_then(|run| run.context.as_ref())
            .and_then(|context| context.properties.get("invocation_id"))
            .and_then(Value::as_str)
            .map(str::to_owned)
    })
}

pub enum Observation {
    Prompt,
    Tui,
    Managed(bool),
    Stage(&'static str),
    Authentication(bool),
    WorkspaceBound(bool),
    SettledTurns(u64),
    Parse(clap::error::ErrorKind),
}
pub fn observe(observation: Observation) {
    if let Ok(mut run) = RUN.lock()
        && let Some(run) = run.as_mut()
    {
        let (key, value) = match observation {
            Observation::Prompt => ("prompt_shown", json!(true)),
            Observation::Tui => ("output_mode", json!("tui")),
            Observation::Managed(managed) => (
                "managed_context",
                json!(if managed { "managed" } else { "unmanaged" }),
            ),
            Observation::Stage(stage) => (
                "failure_stage",
                json!(match stage {
                    "parse" | "startup" | "json_admission" | "dispatch" => stage,
                    _ => "unknown",
                }),
            ),
            Observation::Authentication(authenticated) => (
                "authentication_outcome",
                json!(if authenticated {
                    "authenticated"
                } else {
                    "authorization_pending"
                }),
            ),
            Observation::WorkspaceBound(bound) => ("workspace_bound", json!(bound)),
            Observation::SettledTurns(count) => {
                ("settled_turns_bucket", json!(schema::bucket(count)))
            }
            Observation::Parse(kind) => (
                "parse_kind",
                json!(match kind {
                    clap::error::ErrorKind::DisplayHelp
                    | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand => "help",
                    clap::error::ErrorKind::DisplayVersion => "version",
                    clap::error::ErrorKind::UnknownArgument => "unknown_argument",
                    clap::error::ErrorKind::InvalidSubcommand => "invalid_subcommand",
                    clap::error::ErrorKind::InvalidValue => "invalid_value",
                    clap::error::ErrorKind::MissingRequiredArgument => "missing_required_argument",
                    clap::error::ErrorKind::ArgumentConflict => "argument_conflict",
                    _ => "other",
                }),
            ),
        };
        run.seed.properties.insert(key.into(), value.clone());
        if let Some(context) = run.context.as_mut() {
            context.properties.insert(key.into(), value);
        }
    }
}

pub fn runtime(value: &str) {
    let runtime = match value {
        "claude-code" | "codex" | "cursor" | "opencode" | "claude-desktop" | "openclaw"
        | "hermes" | "workbuddy" => value,
        _ => "other",
    };
    if let Ok(mut run) = RUN.lock()
        && let Some(context) = run.as_mut().and_then(|run| run.context.as_mut())
    {
        context
            .properties
            .insert("effective_runtime".into(), json!(runtime));
    }
}

pub const MCP_TOOLS: &[&str] = &[
    "search",
    "show",
    "view",
    "status",
    "commit",
    "rc_status",
    "rc_list",
];

pub fn rc_request(method: &str) {
    rc::record(method);
}

pub fn mcp_finished(tool: &str, ok: bool, duration: std::time::Duration) {
    let context = RUN
        .lock()
        .ok()
        .and_then(|run| run.as_ref().and_then(|run| run.context.clone()));
    if let Some(context) = context {
        emit(&context, EventName::Operation,json!({"operation":"mcp_tool","mcp_tool":schema::mcp_tool(tool),"outcome":if ok {"ok"} else {"error"},"duration_bucket":duration_bucket(duration.as_millis())}).as_object().unwrap().clone());
    }
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    HubRequest,
    GitTransport,
    RuntimeLaunch,
    TuiSessions,
    TuiTimeline,
    TuiAdopt,
    TuiInitialize,
    TuiRepositories,
    TuiNaming,
    TuiHistory,
    TuiSharing,
    HookIngest,
    HookSettle,
    McpTool,
    RcConnect,
    Authentication,
    Settlement,
    SecretScan,
    ArtifactUpload,
    ArtifactDownload,
}

pub fn operation(
    operation: Operation,
    ok: bool,
    duration: std::time::Duration,
    count: Option<u64>,
) {
    let context = RUN
        .lock()
        .ok()
        .and_then(|run| run.as_ref().and_then(|run| run.context.clone()));
    if let Some(context) = context {
        emit(&context, EventName::Operation, json!({"operation": operation,"operation_id":uuid::Uuid::new_v4(),"outcome":if ok {"ok"} else {"error"},"duration_bucket":duration_bucket(duration.as_millis()),"item_count_bucket":count.map(schema::bucket)}).as_object().unwrap().clone());
        if context.properties.get("background") == Some(&json!(true)) {
            maybe_upload(&context.hub);
        }
    }
}

pub fn measure<T, E>(
    operation_kind: Operation,
    action: impl FnOnce() -> Result<T, E>,
) -> Result<T, E> {
    let start = Instant::now();
    let result = action();
    operation(operation_kind, result.is_ok(), start.elapsed(), None);
    result
}

pub fn measure_command(
    kind: Operation,
    action: impl FnOnce() -> crate::CmdResultAlias,
) -> crate::CmdResultAlias {
    let start = Instant::now();
    let result = action();
    operation(
        kind,
        result
            .as_ref()
            .is_ok_and(|code| *code == crate::ExitCode::Ok),
        start.elapsed(),
        None,
    );
    result
}

pub fn measure_pick<T, E>(
    kind: Operation,
    action: impl FnOnce() -> Result<Option<T>, E>,
) -> Result<Option<T>, E> {
    let start = Instant::now();
    let result = action();
    operation(
        kind,
        result.is_ok(),
        start.elapsed(),
        result.as_ref().ok().map(|value| u64::from(value.is_some())),
    );
    result
}

pub fn exit_category(code: i32) -> &'static str {
    match code {
        0 => "ok",
        1 => "failure",
        2 => "usage",
        3 => "ref",
        4 => "precondition",
        5 => "auth",
        6 => "network",
        7 => "policy",
        8 => "interactive",
        _ => "other",
    }
}

fn duration_bucket(ms: u128) -> &'static str {
    match ms {
        0..=9 => "under_10ms",
        10..=99 => "10-99ms",
        100..=999 => "100-999ms",
        1000..=9999 => "1-9s",
        10000..=59999 => "10-59s",
        60000..=299999 => "1-4m",
        _ => "5m+",
    }
}

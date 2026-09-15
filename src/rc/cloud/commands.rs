use super::store;
use agit_peer::{
    access::{Access, Principal, Resource, Rule},
    client::Client,
    cloud::Enrollment,
};
use clap::{Args as ClapArgs, Subcommand, ValueEnum};

#[derive(ClapArgs)]
pub struct Args {
    #[command(subcommand)]
    action: Action,
}

#[derive(Subcommand)]
enum Action {
    /// Register this daemon namespace and allow inbound cloud connections.
    Enroll {
        #[arg(long)]
        hub: String,
        #[arg(long)]
        name: Option<String>,
    },
    /// Discover devices this cloud account may connect to.
    Devices {
        #[arg(long)]
        hub: String,
        #[arg(long)]
        after: Option<String>,
    },
    /// Inspect enrollment without exposing credentials or private keys.
    Status {
        #[arg(long)]
        hub: String,
    },
    /// Enable or disable inbound access independently of outbound connections.
    Inbound {
        #[arg(long)]
        hub: String,
        #[arg(long, action = clap::ArgAction::Set)]
        enabled: bool,
    },
    /// Inspect the executor's resource policy.
    Policy,
    /// Assign access to "machine", "project:<id>", or "session:<id>".
    Grant {
        #[arg(long)]
        hub: String,
        /// Immutable account ID from the cloud identity service.
        #[arg(long)]
        account: String,
        #[arg(long)]
        resource: String,
        #[arg(long, value_enum)]
        access: Permission,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum Permission {
    Deny,
    Read,
    Control,
    Admin,
}

impl From<Permission> for Access {
    fn from(value: Permission) -> Self {
        match value {
            Permission::Deny => Self::Deny,
            Permission::Read => Self::Read,
            Permission::Control => Self::Control,
            Permission::Admin => Self::Admin,
        }
    }
}

pub fn run(args: Args) -> crate::commands::CmdResult {
    super::super::select_local_authority();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let value = runtime.block_on(async move {
        match args.action {
            Action::Enroll { hub, name } => enroll(&hub, name).await,
            Action::Devices { hub, after } => {
                let api = Client::new(&hub)?;
                let token = super::account_token(api.origin(), false).await?;
                let page = api.devices(&token, after.as_deref()).await?;
                Ok(serde_json::to_value(page)?)
            }
            Action::Status { hub } => {
                let mut status = store::status(&hub)?;
                status["state_directory"] = serde_json::to_value(store::directory()?)?;
                Ok(status)
            }
            Action::Inbound { hub, enabled } => inbound(&hub, enabled).await,
            Action::Policy => Ok(serde_json::to_value(store::policy()?)?),
            Action::Grant {
                hub,
                account,
                resource,
                access,
            } => {
                let api = Client::new(&hub)?;
                let resource = parse_resource(&resource)?;
                let policy = store::grant(Rule {
                    principal: Principal {
                        issuer: api.origin().into(),
                        account_id: account,
                    },
                    resource,
                    access: access.into(),
                })?;
                Ok(serde_json::to_value(policy)?)
            }
        }
    })?;
    println!("{}", serde_json::to_string(&value)?);
    Ok(crate::ExitCode::Ok)
}

pub(super) async fn enroll(hub: &str, name: Option<String>) -> crate::Result<serde_json::Value> {
    let api = Client::new(hub)?;
    let _lock = store::enrollment_lock(api.origin())?;
    let mut enrollment = register(&api, name).await?;
    store::grant_enrolling_owner(&enrollment)?;
    enrollment.inbound_enabled = true;
    store::save(&enrollment)?;
    store::status(api.origin())
}

pub(super) async fn controller(hub: &str) -> crate::Result<store::Enrollment> {
    let api = Client::new(hub)?;
    let _lock = store::enrollment_lock(api.origin())?;
    register(&api, None).await
}

pub(super) async fn inbound(hub: &str, enabled: bool) -> crate::Result<serde_json::Value> {
    if enabled {
        return enroll(hub, None).await;
    }
    let _lock = store::enrollment_lock(hub)?;
    if let Some(mut enrollment) = store::load(hub)? {
        enrollment.inbound_enabled = false;
        store::save(&enrollment)?;
    }
    store::status(hub)
}

async fn register(api: &Client, name: Option<String>) -> crate::Result<store::Enrollment> {
    if let Some(enrollment) = store::load(api.origin())? {
        return Ok(enrollment);
    }
    let machine = super::super::identity::identity()?;
    let identity = agit_peer::Identity::generate()?;
    let request = Enrollment {
        machine_id: machine.machine_fingerprint,
        display_name: name.unwrap_or(machine.display_name),
        certificate: identity.certificate().clone(),
    };
    let token = super::account_token(api.origin(), false).await?;
    let credential = api.enroll(&token, &request).await?;
    let enrollment = store::Enrollment {
        credential,
        identity,
        inbound_enabled: false,
    };
    store::save(&enrollment)?;
    Ok(enrollment)
}

fn parse_resource(resource: &str) -> crate::Result<Resource> {
    if resource == "machine" {
        return Ok(Resource::Machine);
    }
    match resource.split_once(':') {
        Some(("project", id)) if !id.is_empty() => Ok(Resource::Project(id.into())),
        Some(("session", id)) if !id.is_empty() => Ok(Resource::Session(id.into())),
        _ => anyhow::bail!("resource must be machine, project:<id>, or session:<id>"),
    }
}

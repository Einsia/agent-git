//! Read-only phase timings for a disposable Cloud controller and an existing executor.

use agit_peer::{
    Identity,
    client::{Client, join_data},
    cloud::{Device, DeviceCredential, Enrollment, Secret},
    transport::{Role, authenticate},
};
use agit_tunnel::{Connection, Packet, PacketSink, PacketSource};
use anyhow::{Context, ensure};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncBufReadExt, BufReader};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    hub: String,
    #[serde(default)]
    transport_origin: Option<String>,
    account_token: Secret,
    device_id: String,
    samples: usize,
    idle_seconds: u64,
}

async fn phase<T>(
    name: &str,
    future: impl Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
    let start = Instant::now();
    let result = future.await;
    println!(
        "{}",
        json!({"time_unix_ms":SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis(),"phase":name,"ms":start.elapsed().as_secs_f64()*1000.0,"ok":result.is_ok(),"error":result.as_ref().err().map(|e| format!("{e:#}"))})
    );
    result
}

async fn discover(api: &Client, input: &Input) -> anyhow::Result<Device> {
    let mut after = None;
    for _ in 0..64 {
        let page = api.devices(&input.account_token, after.as_deref()).await?;
        if let Some(device) = page
            .devices
            .into_iter()
            .find(|row| row.device.id == input.device_id)
        {
            return Ok(device.device);
        }
        after = page.next_cursor;
        if after.is_none() {
            break;
        }
    }
    anyhow::bail!("executor device is not visible to this account")
}

async fn rpc(
    sink: &mut PacketSink,
    source: &mut PacketSource,
    method: &str,
    params: Value,
) -> anyhow::Result<()> {
    let id = uuid::Uuid::new_v4().to_string();
    tokio::time::timeout(Duration::from_secs(30), async {
        sink.send(Packet::Text(
            json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}).to_string(),
        ))
        .await?;
        loop {
            let Packet::Text(text) = source.next().await.context("executor disconnected")?? else {
                continue;
            };
            let frame: Value = serde_json::from_str(&text)?;
            if frame["id"] == id {
                ensure!(
                    frame.get("error").is_none(),
                    "executor refused the probe method"
                );
                return Ok(());
            }
        }
    })
    .await
    .context("probe RPC timed out")?
}

async fn probe(
    input: &Input,
    api: &Client,
    identity: &Identity,
    source: &DeviceCredential,
) -> anyhow::Result<()> {
    let target = phase("device_discovery", discover(api, input)).await?;
    let dialed = phase(
        "grant_and_offer",
        api.connect(&input.account_token, &source.device, &target),
    )
    .await?;
    println!(
        "{}",
        json!({"phase":"correlation","source_id":source.device.id,"target_id":target.id,"link_id":dialed.link_id,"grant_id":dialed.connection.grant.id})
    );
    let raw = phase(
        "worker_tcp_tls_websocket",
        Connection::open(
            api.data_config(source)?,
            &std::env::current_exe()?,
            &["tunnel"],
        ),
    )
    .await?;
    let raw = phase("relay_pair", join_data(raw, &dialed.link_id, dialed.ticket)).await?;
    let encrypted = phase(
        "endpoint_tls",
        authenticate(raw, identity, &target.certificate, Role::Controller),
    )
    .await?;
    let (mut sink, mut source) = encrypted.split();
    phase(
        "executor_discovery",
        rpc(&mut sink, &mut source, "machine.describe", json!({})),
    )
    .await?;
    for _ in 0..input.samples {
        for (method, params) in [
            ("workspace.list", json!({})),
            ("session.list", json!({"include_local":true})),
            ("fs.readDirectory", json!({"path":""})),
        ] {
            phase(method, rpc(&mut sink, &mut source, method, params)).await?;
        }
    }
    phase("idle", async {
        let deadline = tokio::time::sleep(Duration::from_secs(input.idle_seconds));
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                _ = &mut deadline => return Ok(()),
                packet = source.next() => { packet.context("executor disconnected while idle")??; }
            }
        }
    })
    .await?;
    phase(
        "workspace.list_after_idle",
        rpc(&mut sink, &mut source, "workspace.list", json!({})),
    )
    .await
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    if std::env::args().nth(1).as_deref() == Some("tunnel") {
        return agit_tunnel::worker::run(BufReader::new(tokio::io::stdin()), tokio::io::stdout())
            .await;
    }
    let mut line = String::new();
    BufReader::new(tokio::io::stdin())
        .read_line(&mut line)
        .await?;
    let input: Input = serde_json::from_str(&line)
        .context("provide probe configuration as one JSON line on stdin")?;
    ensure!(
        (1..=20).contains(&input.samples) && input.idle_seconds <= 3600,
        "probe budget is out of range"
    );
    let mut api = Client::new(&input.hub)?;
    if let Some(origin) = &input.transport_origin {
        api = api.with_trusted_transport_origin(origin)?;
    }
    let identity = Identity::generate()?;
    let device = phase(
        "controller_registration",
        api.register_controller(
            &input.account_token,
            &Enrollment {
                machine_id: format!("rc-probe-{}", uuid::Uuid::new_v4()),
                display_name: "RC timing probe".into(),
                certificate: identity.certificate().clone(),
            },
        ),
    )
    .await?;
    let renewal = async {
        let mut ticks = tokio::time::interval(Duration::from_secs(20));
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticks.tick().await;
            phase("controller_renewal", api.renew_controller(&device)).await?;
        }
        #[allow(unreachable_code)]
        Ok::<(), anyhow::Error>(())
    };
    let result = tokio::select! {
        result = probe(&input, &api, &identity, &device) => result,
        result = renewal => result,
    };
    let cleanup = phase(
        "controller_cleanup",
        api.revoke(&input.account_token, &device.device),
    )
    .await;
    result.and(cleanup)
}

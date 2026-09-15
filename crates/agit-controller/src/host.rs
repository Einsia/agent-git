//! Private stdio host for authenticated Web adapters, without an executor.

use crate::{
    Controller, Event, Target, Worker,
    cloud::{Credentials, Route},
};
use agit_peer::{
    client::Client,
    cloud::{Enrollment, Secret},
};
use agit_tunnel::protocol::Reader;
use anyhow::{Context, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use tokio::io::{AsyncBufRead, AsyncWrite, AsyncWriteExt, BufReader};

const IO_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_REQUESTS: usize = 64;
const REQUEST_BYTES: usize = 16 * 1024 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Initialize {
    pub hub: String,
    pub account_token: Secret,
    pub machine_id: String,
}

pub async fn run() -> anyhow::Result<()> {
    if std::env::args().nth(1).as_deref() == Some("tunnel") {
        return agit_tunnel::worker::run(BufReader::new(tokio::io::stdin()), tokio::io::stdout())
            .await;
    }
    let mut input = Reader::new(BufReader::new(tokio::io::stdin()), 8 * 1024 * 1024);
    let init: Initialize = tokio::time::timeout(Duration::from_secs(10), input.read())
        .await??
        .context("controller initialization required")?;
    let api = Client::new(&init.hub)?;
    let identity = agit_peer::Identity::generate()?;
    let device = api
        .register_controller(
            &init.account_token,
            &Enrollment {
                machine_id: init.machine_id,
                display_name: "Web controller".into(),
                certificate: identity.certificate().clone(),
            },
        )
        .await?;
    let credentials = Arc::new(Credentials {
        identity: Arc::new(identity),
        device,
        account: init.account_token,
    });
    let controller = Arc::new(Controller::new(Worker {
        executable: std::env::current_exe()?,
        args: vec!["tunnel".into()],
    }));
    let renewal = renew(&api, &credentials);
    let result = serve(
        &mut input,
        &mut tokio::io::stdout(),
        controller.clone(),
        api.clone(),
        credentials.clone(),
        renewal,
    )
    .await;
    drop(controller);
    let _ = api
        .revoke(&credentials.account, &credentials.device.device)
        .await;
    result
}

async fn serve<R, W>(
    input: &mut Reader<R>,
    output: &mut W,
    controller: Arc<Controller>,
    api: Client,
    credentials: Arc<Credentials>,
    renewal: impl std::future::Future<Output = anyhow::Result<()>>,
) -> anyhow::Result<()>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut events = controller.subscribe();

    let mut requests = tokio::task::JoinSet::new();
    let bytes = Arc::new(tokio::sync::Semaphore::new(REQUEST_BYTES));
    tokio::pin!(renewal);
    let result = async {
        write(output, json!({"jsonrpc":"2.0", "method":"controller.ready", "params":{"pid":std::process::id(), "protocol":1, "device_id":credentials.device.device.id}})).await?;
        loop {
            tokio::select! {
                result = &mut renewal => { return result; }
                request = input.read::<Value>() => {
                    let Some(request) = request? else { break; };
                    let permit = bytes.clone().try_acquire_many_owned(
                        serde_json::to_vec(&request)?.len().max(1) as u32,
                    );
                    if requests.len() >= MAX_REQUESTS || permit.is_err() {
                        write(output, rejected(&request["id"], "controller request capacity exceeded")).await?;
                        continue;
                    }
                    let permit = permit?;
                    let (controller, api, credentials) = (controller.clone(), api.clone(), credentials.clone());
                    requests.spawn(async move {
                        let _permit = permit;
                        dispatch(controller, api, credentials, request).await
                    });
                }
                Some(response) = requests.join_next(), if !requests.is_empty() => { write(output, response?).await?; }
                event = events.recv() => {
                    let event = event.context("controller event cursor lost; reconnect and replay")?;
                    let frame = match event {
                        Event::State { status } => {
                            eprintln!("{}", json!({"event":"controller.route", "pid":std::process::id(), "peer_id":status.peer_id, "route_id":status.route_id, "generation":status.generation, "worker_pid":status.worker_pid, "state":status.state}));
                            json!({"jsonrpc":"2.0", "method":"peer.state", "params":{"peer_id":status.peer_id,"status":status}})
                        }
                        Event::Frame { peer_id, route_id, generation, frame, .. } => json!({"jsonrpc":"2.0", "method":"peer.frame", "params":{"peer_id":peer_id,"route_id":route_id,"generation":generation,"frame":frame}}),
                    };
                    write(output, frame).await?;
                }
            }
        }
        Ok::<_, anyhow::Error>(())
    }.await;
    requests.abort_all();
    while requests.join_next().await.is_some() {}
    result
}

async fn renew(api: &Client, credentials: &Credentials) -> anyhow::Result<()> {
    let mut interval = tokio::time::interval(Duration::from_secs(20));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        interval.tick().await;
        api.renew_controller(&credentials.device).await?;
    }
}

async fn write<W: AsyncWrite + Unpin>(output: &mut W, frame: Value) -> anyhow::Result<()> {
    let mut bytes = serde_json::to_vec(&frame)?;
    bytes.push(b'\n');
    tokio::time::timeout(IO_TIMEOUT, async {
        output.write_all(&bytes).await?;
        output.flush().await
    })
    .await??;
    Ok(())
}

/// Hosts authenticate callers and bound request admission before dispatching.
pub async fn dispatch(
    controller: Arc<Controller>,
    api: Client,
    credentials: Arc<Credentials>,
    request: Value,
) -> Value {
    let id = request["id"].clone();
    let result = async {
        ensure!(request["jsonrpc"] == "2.0" && (id.is_i64() || id.as_str().is_some_and(|id| !id.is_empty() && id.len() <= 256)), "invalid controller request");
        let params = &request["params"];
        let peer_id = || params["peer_id"].as_str().context("peer_id is required");
        match request["method"].as_str().unwrap_or("") {
            "peer.connect_cloud" => {
                let target = serde_json::from_value(params["target"].clone())?;
                controller.connect_with(peer_id()?.into(), Arc::new(Route::new(api, credentials, target)?), None)?;
                let status = controller.ready(peer_id()?, Duration::from_secs(60)).await?;
                Ok(json!({"description":status.description,"route_id":status.route_id,"generation":status.generation}))
            }
            "peer.list" => Ok(json!({"peers":controller.list()})),
            "peer.disconnect" => { controller.disconnect(peer_id()?); Ok(json!({"ok":true})) }
            "peer.request" => {
                let target: Target = serde_json::from_value(params.clone())?;
                let method = params["method"].as_str().context("executor method is required")?;
                let budget = Duration::from_millis(params["timeout_ms"].as_u64().unwrap_or(90000).clamp(1,120000));
                let response = controller.request_at(target, method.into(), params["params"].clone(), budget).await;
                match response {
                    Ok(mut frame) => { frame["id"] = id.clone(); Ok(frame) }
                    Err(error) => Ok(json!({"jsonrpc":"2.0","id":id,"error":{"code":300,"message":error.to_string(),"data":error}})),
                }
            }
            _ => anyhow::bail!("unknown cloud controller method"),
        }
    }.await;
    match result {
        Ok(frame) if request["method"] == "peer.request" => frame,
        Ok(result) => json!({"jsonrpc":"2.0","id":id,"result":result}),
        Err(error) => rejected(&id, &error.to_string()),
    }
}

fn rejected(id: &Value, message: &str) -> Value {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":300,"message":message,
        "data":{"outcome":"not_sent"}}})
}

#[cfg(test)]
mod tests {
    use super::*;
    use agit_peer::{
        access::Principal,
        cloud::{Device, DeviceCredential},
    };

    #[tokio::test]
    async fn pending_renewal_does_not_stall_requests_and_failure_closes_the_host() {
        let identity = Arc::new(agit_peer::Identity::generate().unwrap());
        let credentials = Arc::new(Credentials {
            identity: identity.clone(),
            device: DeviceCredential {
                device: Device {
                    id: "device".into(),
                    machine_id: "machine".into(),
                    display_name: "Controller".into(),
                    owner: Principal {
                        issuer: "http://127.0.0.1:1".into(),
                        account_id: "account".into(),
                    },
                    certificate: identity.certificate().clone(),
                    credential_epoch: 1,
                },
                token: Secret::new("device-token".into()),
            },
            account: Secret::new("account-token".into()),
        });
        let controller = Arc::new(Controller::new(Worker {
            executable: std::env::current_exe().unwrap(),
            args: vec![],
        }));
        let (entered, started) = tokio::sync::oneshot::channel();
        let (release, blocked) = tokio::sync::oneshot::channel();
        let renewal = async move {
            let _ = entered.send(());
            blocked.await?;
            anyhow::bail!("renewal rejected")
        };
        let (client, host) = tokio::io::duplex(8192);
        let (read, mut output) = tokio::io::split(host);
        let task = tokio::spawn(async move {
            let mut input = Reader::new(BufReader::new(read), 8192);
            serve(
                &mut input,
                &mut output,
                controller,
                Client::new("http://127.0.0.1:1").unwrap(),
                credentials,
                renewal,
            )
            .await
        });
        let (read, mut output) = tokio::io::split(client);
        let mut input = Reader::new(BufReader::new(read), 8192);
        tokio::time::timeout(IO_TIMEOUT, started)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            input.read::<Value>().await.unwrap().unwrap()["method"],
            "controller.ready"
        );
        write(
            &mut output,
            json!({"jsonrpc":"2.0","id":"list","method":"peer.list","params":{}}),
        )
        .await
        .unwrap();
        let reply = tokio::time::timeout(IO_TIMEOUT, input.read::<Value>())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(reply["id"], "list");
        assert_eq!(reply["result"]["peers"], json!([]));
        release.send(()).unwrap();
        let error = tokio::time::timeout(IO_TIMEOUT, task)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert_eq!(error.to_string(), "renewal rejected");
    }
}

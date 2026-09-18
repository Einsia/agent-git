//! Owner RPC adapter for the independent controller. Executor modules do not
//! import this adapter or construct outbound peer connections.

use crate::protocol::{ErrorCode, Frame, RequestId, RpcError};
use agit_controller::{Controller, Worker};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};

pub fn controller() -> crate::Result<Arc<Controller>> {
    Ok(Arc::new(Controller::new(worker()?)))
}

pub fn worker() -> crate::Result<Worker> {
    Ok(Worker {
        executable: super::tunnel::worker_executable()?,
        args: vec!["rc".into(), "tunnel".into()],
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Connect {
    peer_id: String,
    host: String,
    binary: String,
    #[serde(default)]
    fingerprint: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CloudConnect {
    peer_id: String,
    hub: String,
    target: agit_peer::cloud::Device,
    #[serde(default)]
    fingerprint: Option<String>,
}

pub async fn dispatch(
    controller: &Controller,
    cloud: &super::cloud::Clients,
    frame: Frame,
) -> Frame {
    let id = frame.id.clone().unwrap_or(RequestId::Num(0));
    let params = frame.params.clone().unwrap_or_default();
    let result: crate::Result<Frame> = async {
        let peer_id = || {
            params
                .get("peer_id")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("peer_id is required"))
        };
        match frame.method() {
            "peer.cloud" => Ok(Frame::response(id.clone(), super::cloud::manage(cloud, serde_json::from_value(params)?).await?)),
            "peer.connect_cloud" => {
                let config: CloudConnect = serde_json::from_value(params)?;
                let route = super::cloud::host::Route::new(cloud.get(&config.hub)?, config.target)?;
                controller.connect_with(config.peer_id.clone(), Arc::new(route), config.fingerprint)?;
                let status = controller.ready(&config.peer_id, Duration::from_secs(60)).await?;
                Ok(Frame::response(id.clone(), json!({"description":status.description,"generation":status.generation,"route_id":status.route_id})))
            }
            "peer.connect" => {
                let config: Connect = serde_json::from_value(params)?;
                let transport = agit_tunnel::Config::Ssh {
                    host: config.host,
                    command: vec![
                        config.binary,
                        "rc".into(),
                        "local".into(),
                        "bridge".into(),
                        "--ensure".into(),
                    ],
                };
                controller.connect(config.peer_id.clone(), transport, config.fingerprint)?;
                let status = controller
                    .ready(&config.peer_id, Duration::from_secs(40))
                    .await?;
                Ok(Frame::response(
                    id.clone(),
                    json!({"description":status.description,"generation":status.generation,"route_id":status.route_id}),
                ))
            }
            "peer.list" => Ok(Frame::response(
                id.clone(),
                json!({"peers":controller.list()}),
            )),
            "peer.disconnect" => {
                controller.disconnect(peer_id()?);
                Ok(Frame::response(id.clone(), json!({"ok":true})))
            }
            "peer.request" => {
                let method = params
                    .get("method")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("executor method is required"))?;
                let budget = params
                    .get("timeout_ms")
                    .and_then(Value::as_u64)
                    .unwrap_or(90000)
                    .clamp(1, 120000);
                match controller
                    .request_at(
                        serde_json::from_value::<agit_controller::Target>(params.clone())?,
                        method.into(),
                        params.get("params").cloned().unwrap_or_else(|| json!({})),
                        Duration::from_millis(budget),
                    )
                    .await
                {
                    Ok(mut response) => {
                        response["id"] = serde_json::to_value(&id)?;
                        Ok(serde_json::from_value(response)?)
                    }
                    Err(failure) => {
                        let mut error =
                            RpcError::new(ErrorCode::ConnectionOffline, failure.to_string());
                        error.data = Some(serde_json::to_value(failure)?);
                        Ok(Frame::error_response(id.clone(), error))
                    }
                }
            }
            _ => {
                let mut error = RpcError::new(ErrorCode::UnknownMethod, "local controller does not support this peer method");
                error.data = Some(json!({"origin":"local_controller", "hint":"inspect `agit rc local status` and use `agit rc local restart --if-idle` after user work finishes"}));
                Ok(Frame::error_response(id.clone(), error))
            },
        }
    }
    .await;
    result.unwrap_or_else(|error| {
        Frame::error_response(
            id,
            RpcError::new(ErrorCode::ConnectionOffline, error.to_string()),
        )
    })
}

pub fn notification(event: &agit_controller::Event) -> Frame {
    match event {
        agit_controller::Event::State { status } => Frame::notification(
            "peer.state",
            json!({"peer_id":status.peer_id,"status":status}),
        ),
        agit_controller::Event::Frame {
            peer_id,
            route_id,
            generation,
            frame,
            ..
        } => Frame::notification(
            "peer.frame",
            json!({"peer_id":peer_id,"route_id":route_id,"generation":generation,"frame":frame}),
        ),
    }
}

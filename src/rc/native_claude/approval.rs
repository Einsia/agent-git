//! Await native decisions without charging the runtime hook's execution budget.

use super::transport::{Descriptor, MAX_BODY};
use anyhow::{Context, ensure};
use http_body_util::{BodyExt, Full, Limited};
use hyper::{Request, body::Bytes};
use hyper_util::rt::TokioIo;
use serde_json::{Value, json};
use std::{
    io::Read,
    time::{Duration, Instant},
};

pub(crate) fn wait_approval(session: &str, generation: &str, id: &str) -> crate::Result<Value> {
    ensure!(
        crate::rc::native_inbox::valid_id(id),
        "native approval requires its exact identity"
    );
    let (_, parent) =
        super::process(std::process::id()).context("cannot verify the native approval caller")?;
    let registration =
        super::read_registration(&super::directory()?.join(format!("{parent}.json")))?;
    ensure!(
        registration.session == session
            && registration.generation == generation
            && registration.process.pid == parent
            && registration.is_registered(),
        "native approval caller is not the registered writer"
    );
    let body = json!({"session":session,"generation":generation,"cwd":registration.cwd,"id":id});
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(async {
            let deadline = Instant::now() + Duration::from_secs(30);
            while Instant::now() < deadline && registration.is_registered() {
                let reply = tokio::time::timeout(Duration::from_secs(2), async {
                    // Reopen the same descriptor so a daemon restart cannot strand a native dialog.
                    let descriptor: Descriptor = serde_json::from_reader(
                        crate::rc::native_inbox::open_regular(&registration.descriptor)?
                            .take(super::RECORD_LIMIT),
                    )?;
                    ensure!(
                        super::process(descriptor.owner.pid)
                            .is_some_and(|(process, _)| process == descriptor.owner),
                        "native approval listener changed"
                    );
                    let stream = tokio::net::UnixStream::connect(&descriptor.socket).await?;
                    ensure!(
                        stream.peer_cred()?.uid() == unsafe { libc::geteuid() },
                        "native approval listener has a different owner"
                    );
                    let (mut sender, connection) =
                        hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;
                    let connection = tokio::spawn(connection);
                    let request = Request::post("http://localhost/approval")
                        .header("Authorization", format!("Bearer {}", descriptor.token))
                        .header("Content-Type", "application/json")
                        .header("Connection", "close")
                        .body(Full::new(Bytes::from(serde_json::to_vec(&body)?)))?;
                    let response = sender.send_request(request).await;
                    let result = async {
                        let response = response?;
                        ensure!(
                            response.status().is_success(),
                            "native approval is reconnecting"
                        );
                        let bytes = Limited::new(response.into_body(), MAX_BODY)
                            .collect()
                            .await
                            .map_err(anyhow::Error::msg)?
                            .to_bytes();
                        Ok::<Value, anyhow::Error>(serde_json::from_slice(&bytes)?)
                    }
                    .await;
                    connection.abort();
                    result
                })
                .await;
                if let Ok(Ok(reply)) = reply
                    && reply["generation"] == generation
                    && reply["id"] == id
                    && !reply["decision"].is_null()
                {
                    return Ok(reply);
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            Ok(json!({"generation":generation,"id":id,"decision":null}))
        })
}

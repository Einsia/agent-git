use super::*;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

#[tokio::test]
async fn service_admission_pins_both_endpoints_and_session_authority() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let issuer = format!("http://{}", listener.local_addr().unwrap());
    let target = Device {
        id: "executor".into(),
        owner: crate::access::Principal {
            issuer: issuer.clone(),
            account_id: "owner".into(),
        },
        machine_id: "executor-machine".into(),
        display_name: "Executor".into(),
        certificate: crate::Identity::generate().unwrap().certificate().clone(),
        credential_epoch: 1,
    };
    let source = DeviceCredential {
        device: Device {
            id: "service".into(),
            machine_id: "service-incarnation".into(),
            certificate: crate::Identity::generate().unwrap().certificate().clone(),
            ..target.clone()
        },
        token: Secret::new("service-fixture".into()),
    };
    let scope = SessionController {
        session_id: "canonical-session".into(),
        runtime: "codex".into(),
        generation: 7,
        access: crate::access::Access::Admin,
    };
    let grant = ConnectionGrant {
        id: "connection".into(),
        caller: target.owner.clone(),
        source: source.device.clone(),
        target: target.clone(),
        expires_at_ms: i64::MAX,
        session_controller: Some(scope.clone()),
    };
    let mut missing = grant.clone();
    missing.session_controller = None;
    let mut session = grant.clone();
    session.session_controller.as_mut().unwrap().session_id = "another-session".into();
    let mut generation = grant.clone();
    generation.session_controller.as_mut().unwrap().generation += 1;
    let mut device = grant.clone();
    device.target.credential_epoch += 1;
    let mut controller = grant.clone();
    controller.source.id = "another-controller".into();
    let responses = [grant, missing, session, generation, device, controller];
    let requests = responses.len();
    let server = tokio::spawn(async move {
        for grant in responses {
            let (socket, _) = listener.accept().await.unwrap();
            let mut socket = BufReader::new(socket);
            let mut line = String::new();
            socket.read_line(&mut line).await.unwrap();
            assert_eq!(line, "POST /api/peer/session-connections HTTP/1.1\r\n");
            let mut service_authenticated = false;
            loop {
                line.clear();
                assert!(socket.read_line(&mut line).await.unwrap() > 0);
                if line == "\r\n" {
                    break;
                }
                service_authenticated |=
                    line.to_ascii_lowercase() == "authorization: bearer service-fixture\r\n";
            }
            assert!(service_authenticated);
            let body = serde_json::to_string(&DialedConnection {
                connection: GrantedConnection {
                    grant,
                    token: Secret::new("connection-fixture".into()),
                },
                link_id: "link".into(),
                ticket: Secret::new("ticket-fixture".into()),
            })
            .unwrap();
            socket
                .get_mut()
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        }
    });
    let client = Client::new(&issuer).unwrap();
    for index in 0..requests {
        let result = client.connect_session(&source, &target, &scope).await;
        if index == 0 {
            assert_eq!(
                result.unwrap().connection.grant.session_controller,
                Some(scope.clone())
            );
        } else {
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("authority changed")
            );
        }
    }
    server.await.unwrap();
}

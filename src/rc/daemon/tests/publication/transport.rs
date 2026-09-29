use super::*;
use agit_tunnel::Packet;
use futures_util::{SinkExt, StreamExt};
use std::time::Duration;

pub(super) struct Endpoint {
    endpoint: tokio::task::JoinHandle<crate::Result<()>>,
    dispatch: tokio::task::JoinHandle<()>,
    admission: mpsc::Sender<crate::rc::cloud::host::Authenticated>,
    pub outbound: crate::rc::outbound::OutboundTx,
    pub stop: tokio::sync::watch::Sender<bool>,
    _directory: tempfile::TempDir,
}

impl Drop for Endpoint {
    fn drop(&mut self) {
        self.endpoint.abort();
        self.dispatch.abort();
    }
}

impl Endpoint {
    pub async fn new(daemon: Arc<Mutex<Daemon>>, grant: &ConnectionGrant) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let listener =
            tokio::net::UnixListener::bind(directory.path().join("endpoint.sock")).unwrap();
        let registry = Registry::fixed(
            Policy::new(
                1,
                vec![Rule {
                    principal: grant.caller.clone(),
                    resource: Resource::Session("native".into()),
                    access: Access::Control,
                }],
            )
            .unwrap(),
        );
        let catalog = registry.client(grant.caller.clone(), grant.expires_at_ms);
        let request = catalog
            .authorize(Frame::request("session.list", json!({})))
            .unwrap();
        catalog.observe_response(&Frame::response(
            request.id.unwrap(),
            json!({"sessions":[{
                "session_id":"logical", "runtime_session_id":"native", "runtime":"codex",
                "workspace_id":"local-owner", "cwd":directory.path()
            }]}),
        ));
        let (admission, incoming) = mpsc::channel(2);
        let (outbound, output) = crate::rc::outbound::channel();
        let (events, mut requests) = mpsc::channel(16);
        let endpoint = tokio::spawn(crate::rc::endpoint::serve_test_cloud(
            listener, output, events, incoming, registry,
        ));
        let (stop, stopped) = tokio::sync::watch::channel(false);
        daemon.lock().await.settlement.send_modify(|state| {
            state.epoch = 1;
            state.local_owner = true;
        });
        let replies = outbound.clone();
        let dispatch = tokio::spawn(async move {
            let mut tasks = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    request = requests.recv() => {
                        let Some(crate::rc::link::LinkEvent::Frame { epoch, frame }) = request else {
                            break;
                        };
                        assert_eq!(frame.method(), method::SESSION_PUBLICATION_DELIVER);
                        super::super::super::publication::dispatch(
                            daemon.clone(), &frame, epoch, replies.clone(), &mut tasks,
                            stopped.clone(),
                        ).await;
                    }
                    Some(result) = tasks.join_next(), if !tasks.is_empty() => {
                        result.unwrap();
                    }
                }
            }
        });
        Self {
            endpoint,
            dispatch,
            admission,
            outbound,
            stop,
            _directory: directory,
        }
    }

    pub async fn connect(
        &self,
        grant: ConnectionGrant,
        source: &agit_peer::Identity,
    ) -> Connection {
        let target = store::load(&grant.target.owner.issuer)
            .unwrap()
            .unwrap()
            .identity;
        assert_eq!(&grant.source.certificate, source.certificate());
        let (outgoing, incoming) = tokio::io::duplex(65536);
        let (outgoing, incoming) = tokio::join!(
            source.connect(target.certificate(), outgoing),
            target.accept(source.certificate(), incoming),
        );
        let (sink, source) = agit_peer::transport::framed(0, outgoing.unwrap()).split();
        let (lifetime, stopped) = tokio::sync::watch::channel(());
        self.admission
            .send(crate::rc::cloud::host::Authenticated {
                connection: agit_peer::transport::framed(0, incoming.unwrap()),
                grant,
                stopped,
                renewal: None,
            })
            .await
            .unwrap();
        let mut connection = Connection {
            sink,
            source,
            _lifetime: lifetime,
        };
        let description = connection
            .call(Frame::request(
                "machine.describe",
                json!({"session_events":true}),
            ))
            .await
            .unwrap();
        assert_eq!(description["authority"], "cloud-session-controller");
        assert!(
            description["rpc_features"]
                .as_array()
                .unwrap()
                .iter()
                .any(|feature| feature == "publication-delivery-v1")
        );
        connection
    }
}

pub(super) struct Connection {
    sink: agit_tunnel::PacketSink,
    source: agit_tunnel::PacketSource,
    _lifetime: tokio::sync::watch::Sender<()>,
}

impl Connection {
    pub async fn send(&mut self, frame: &Frame) {
        self.sink.send(Packet::Text(frame.to_json())).await.unwrap();
    }

    pub async fn receive(&mut self) -> Frame {
        let packet = tokio::time::timeout(Duration::from_secs(15), self.source.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let Packet::Text(text) = packet else {
            panic!("expected an RPC frame")
        };
        serde_json::from_str(&text).unwrap()
    }

    pub async fn call(&mut self, frame: Frame) -> Result<serde_json::Value, RpcError> {
        self.send(&frame).await;
        let response = self.receive().await;
        assert_eq!(response.id, frame.id);
        match response.error {
            Some(error) => Err(error),
            None => Ok(response.result.unwrap()),
        }
    }

    pub async fn expect_revoked(&mut self) {
        self.send(&request()).await;
        let closed = tokio::time::timeout(Duration::from_secs(5), self.source.next())
            .await
            .unwrap();
        assert!(
            closed.is_none_or(|packet| packet.is_err()),
            "revoked controller received a response"
        );
    }
}

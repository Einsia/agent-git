//! Admission overlaps transport setup without consuming the relay join deadline.

use std::time::Duration;

/// A transport is returned only after admission succeeds; a stale speculative socket is replaced.
pub async fn verified_transport<G, T, V, O, F>(
    verification: V,
    mut open: O,
) -> anyhow::Result<(G, T, bool)>
where
    V: std::future::Future<Output = anyhow::Result<G>>,
    O: FnMut() -> F,
    F: std::future::Future<Output = anyhow::Result<T>>,
{
    // A speculative socket must retain headroom for the relay's first-frame deadline.
    let opening = async {
        let transport = open().await?;
        Ok::<_, anyhow::Error>((transport, tokio::time::Instant::now()))
    };
    let (grant, (transport, started)) =
        futures_util::future::try_join(verification, opening).await?;
    if started.elapsed() >= Duration::from_secs(5) {
        drop(transport);
        Ok((grant, open().await?, true))
    } else {
        Ok((grant, transport, false))
    }
}

#[cfg(test)]
mod tests {
    use super::super::join_data;
    use super::*;
    use crate::cloud::Secret;
    use anyhow::ensure;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };
    use tokio::sync::mpsc;

    struct SocketOwner(Arc<AtomicUsize>);
    impl Drop for SocketOwner {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn admission_preserves_join_deadline_and_drops_unaccepted_transports() {
        for (delay, opening_delay, reject, expected_opens) in [
            (1, 0, false, 1),
            (12, 0, false, 2),
            (1, 8, false, 1),
            (12, 0, true, 1),
        ] {
            let granted = Arc::new(AtomicBool::new(false));
            let opened = AtomicUsize::new(0);
            let dropped = Arc::new(AtomicUsize::new(0));
            let verification = async {
                tokio::time::sleep(Duration::from_secs(delay)).await;
                ensure!(!reject, "grant rejected");
                granted.store(true, Ordering::SeqCst);
                Ok(())
            };
            let open = || {
                opened.fetch_add(1, Ordering::SeqCst);
                let granted = granted.clone();
                let owner = SocketOwner(dropped.clone());
                async move {
                    tokio::time::sleep(Duration::from_secs(opening_delay)).await;
                    let connected = tokio::time::Instant::now();
                    let (tx, rx) = mpsc::channel(1);
                    let sink = futures_util::sink::unfold(
                        (connected, owner, tx, granted),
                        |(connected, owner, tx, granted), packet| async move {
                            ensure!(
                                granted.load(Ordering::SeqCst),
                                "join before grant verification"
                            );
                            ensure!(
                                connected.elapsed() < Duration::from_secs(10),
                                "relay first-frame deadline expired"
                            );
                            let agit_tunnel::Packet::Text(value) = packet else {
                                anyhow::bail!("expected relay join");
                            };
                            let _: crate::cloud::DataJoin = serde_json::from_str(&value)?;
                            tx.send(Ok(agit_tunnel::Packet::Text(serde_json::to_string(
                                &crate::cloud::DataReady {
                                    link_id: "test-link".into(),
                                },
                            )?)))
                            .await?;
                            Ok::<_, anyhow::Error>((connected, owner, tx, granted))
                        },
                    );
                    let source = futures_util::stream::unfold(rx, |mut rx| async {
                        rx.recv().await.map(|packet| (packet, rx))
                    });
                    Ok(agit_tunnel::Connection::from_parts(
                        0,
                        Box::pin(sink),
                        Box::pin(source),
                    ))
                }
            };
            let started = tokio::time::Instant::now();
            let admitted = verified_transport(verification, open).await;
            if opening_delay > 0 {
                assert!(started.elapsed() < Duration::from_secs(delay + opening_delay));
            }
            if reject {
                assert!(admitted.is_err());
            } else {
                let ((), connection, reopened) = admitted.unwrap();
                assert_eq!(reopened, delay == 12);
                let paired = join_data(connection, "test-link", Secret::new("ticket".into()))
                    .await
                    .unwrap();
                drop(paired);
            }
            assert_eq!(opened.load(Ordering::SeqCst), expected_opens);
            assert_eq!(dropped.load(Ordering::SeqCst), expected_opens);
        }
    }
}

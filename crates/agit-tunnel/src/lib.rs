//! Independently owned transport workers carry packets without session authority.

mod process;
pub mod protocol;
#[cfg(feature = "worker")]
mod tcp;
#[cfg(feature = "worker")]
mod websocket;
#[cfg(windows)]
pub mod windows_job;
#[cfg(feature = "worker")]
pub mod worker;

pub type Result<T> = anyhow::Result<T>;
use anyhow::{Context, ensure};
use futures_util::{Sink, Stream};
use protocol::{Command, Event, MAX_RECORD, QUEUE_BYTES, QUEUE_CAP, Reader, VERSION, write};
pub use protocol::{Config, ConnectTiming, Packet};
use std::{
    path::Path,
    pin::Pin,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncBufRead, AsyncWrite, BufReader},
    sync::{mpsc, watch},
};

pub type PacketSink = Pin<Box<dyn Sink<Packet, Error = anyhow::Error> + Send>>;
pub type PacketSource = Pin<Box<dyn Stream<Item = Result<Packet>> + Send>>;
const OPEN_TIMEOUT: Duration = Duration::from_secs(35);

struct Owner {
    stop: Option<watch::Sender<bool>>,
    tasks: Mutex<Vec<tokio::task::AbortHandle>>,
    failure: Arc<Mutex<Option<String>>>,
}

impl Drop for Owner {
    fn drop(&mut self) {
        if let Some(stop) = &self.stop {
            stop.send_replace(true);
        }
        for task in self
            .tasks
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
        {
            task.abort();
        }
    }
}

/// The WebSocket close code for a service restart: the far end is going away to be replaced, and
/// nothing failed.
pub const SERVICE_RESTART: u16 = 1012;

/// The far end closed the connection with [`SERVICE_RESTART`].
#[derive(Debug)]
pub struct ServiceRestart;

impl std::fmt::Display for ServiceRestart {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the far end closed the connection to restart")
    }
}

impl std::error::Error for ServiceRestart {}

/// Whether `error` comes from a [`ServiceRestart`] close, directly or through the I/O error an
/// endpoint wrapper reads it as.
pub fn is_service_restart(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause.is::<ServiceRestart>()
            || cause
                .downcast_ref::<std::io::Error>()
                .and_then(std::io::Error::get_ref)
                .is_some_and(|inner| inner.is::<ServiceRestart>())
    })
}

pub struct Connection {
    pub worker_pid: u32,
    pub connect_timing: Option<ConnectTiming>,
    sink: PacketSink,
    source: PacketSource,
    progress: Option<Arc<std::sync::atomic::AtomicU64>>,
}

impl Connection {
    /// Endpoint adapters preserve packet ownership while replacing wire framing.
    pub fn from_parts(worker_pid: u32, sink: PacketSink, source: PacketSource) -> Self {
        Self {
            worker_pid,
            connect_timing: None,
            sink,
            source,
            progress: None,
        }
    }

    /// Counts the bytes an endpoint wrapper has received beneath its packets. A wrapper that
    /// yields one packet per complete frame still shows a frame arriving slowly through it.
    pub fn with_progress(mut self, progress: Arc<std::sync::atomic::AtomicU64>) -> Self {
        self.progress = Some(progress);
        self
    }

    pub fn progress(&self) -> Option<Arc<std::sync::atomic::AtomicU64>> {
        self.progress.clone()
    }

    /// The controller chooses the executable; credentials travel only over IPC.
    pub async fn open(config: Config, executable: &Path, args: &[&str]) -> Result<Self> {
        config.validate()?;
        let mut command = tokio::process::Command::new(executable);
        command.args(args).env("AGIT_TUNNEL_CONNECT_TIMING", "1");
        let mut process = process::Process::spawn(&mut command)?;
        let output = process
            .child
            .stdin
            .take()
            .context("tunnel input is unavailable")?;
        let input = process
            .child
            .stdout
            .take()
            .context("tunnel output is unavailable")?;
        let (stop, mut stopping) = watch::channel(false);
        let owner = Arc::new(Owner {
            stop: Some(stop),
            tasks: Mutex::new(vec![]),
            failure: Default::default(),
        });
        tokio::spawn(async move {
            tokio::select! {
                _ = stopping.changed() => {},
                _ = process.child.wait() => {},
            }
            process.terminate();
            let _ = process.child.wait().await;
        });
        tokio::time::timeout(
            OPEN_TIMEOUT,
            Self::attach(config, BufReader::new(input), output, owner),
        )
        .await
        .context("tunnel worker connection timed out")?
    }

    pub fn split(self) -> (PacketSink, PacketSource) {
        (self.sink, self.source)
    }

    async fn attach<R, W>(
        config: Config,
        input: R,
        mut output: W,
        owner: Arc<Owner>,
    ) -> Result<Self>
    where
        R: AsyncBufRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        write(
            &mut output,
            &Command::Open {
                version: VERSION,
                config,
            },
        )
        .await?;
        let mut input = Reader::new(input, MAX_RECORD);
        let (worker_pid, connect_timing) = match input.read::<Event>().await? {
            Some(Event::Connected {
                version,
                worker_pid,
                timing,
            }) => {
                ensure!(version == VERSION, "unsupported tunnel worker version");
                (worker_pid, timing)
            }
            Some(Event::Failed { message }) => anyhow::bail!("tunnel connection failed: {message}"),
            _ => anyhow::bail!("tunnel worker closed before connecting"),
        };
        let (acks, acknowledgments) = mpsc::channel(1);
        let (packets, incoming) = mpsc::channel(QUEUE_CAP);
        let bytes = Arc::new(tokio::sync::Semaphore::new(QUEUE_BYTES));
        let failure = owner.failure.clone();
        let reader = tokio::spawn(async move {
            let result: Result<()> = async {
                while let Some(event) = input.read::<Event>().await? {
                    match event {
                        Event::Received { packet } => {
                            packet.validate()?;
                            let permit = bytes
                                .clone()
                                .try_acquire_many_owned(packet.len().max(1) as u32)
                                .map_err(|_| {
                                    anyhow::anyhow!("tunnel receive byte budget is exhausted")
                                })?;
                            packets.try_send((packet, permit)).map_err(|_| {
                                anyhow::anyhow!("tunnel receive queue is full or closed")
                            })?;
                        }
                        Event::Written { serial } => {
                            acks.try_send(serial).map_err(|_| {
                                anyhow::anyhow!("unexpected tunnel write acknowledgment")
                            })?;
                        }
                        Event::Failed { message } => {
                            anyhow::bail!("tunnel worker failed: {message}")
                        }
                        Event::Connected { .. } => anyhow::bail!("tunnel worker registered twice"),
                    }
                }
                anyhow::bail!("tunnel worker disconnected")
            }
            .await;
            *failure
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                Some(result.unwrap_err().to_string());
        });
        owner
            .tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(reader.abort_handle());
        let sink_owner = owner.clone();
        let sink = futures_util::sink::unfold(
            (output, acknowledgments, 0_u64, sink_owner),
            |(mut output, mut acks, serial, owner), packet: Packet| async move {
                packet.validate()?;
                let serial = serial
                    .checked_add(1)
                    .context("tunnel write identity exhausted")?;
                write(&mut output, &Command::Send { serial, packet }).await?;
                ensure!(
                    acks.recv().await == Some(serial),
                    "tunnel closed before confirming the write; outcome is unknown"
                );
                Ok::<_, anyhow::Error>((output, acks, serial, owner))
            },
        );
        let source =
            futures_util::stream::unfold((Some(incoming), owner), |(incoming, owner)| async move {
                let mut incoming = incoming?;
                match incoming.recv().await {
                    Some((packet, _permit)) => Some((Ok(packet), (Some(incoming), owner))),
                    None => {
                        let message = owner
                            .failure
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .clone()
                            .unwrap_or_else(|| "tunnel worker disconnected".into());
                        Some((Err(anyhow::anyhow!(message)), (None, owner)))
                    }
                }
            });
        Ok(Self {
            worker_pid,
            connect_timing,
            sink: Box::pin(sink),
            source: Box::pin(source),
            progress: None,
        })
    }

    #[cfg(all(feature = "worker", any(test, feature = "test-support")))]
    pub async fn in_process(config: Config) -> Result<Self> {
        let (client, server) = tokio::io::duplex(65536);
        let (input, output) = tokio::io::split(client);
        let (worker_input, worker_output) = tokio::io::split(server);
        let task = tokio::spawn(worker::run_with_timing(
            BufReader::new(worker_input),
            worker_output,
            true,
        ));
        let owner = Arc::new(Owner {
            stop: None,
            tasks: Mutex::new(vec![task.abort_handle()]),
            failure: Default::default(),
        });
        Self::attach(config, BufReader::new(input), output, owner).await
    }
}

#[cfg(all(test, feature = "worker"))]
mod tests;

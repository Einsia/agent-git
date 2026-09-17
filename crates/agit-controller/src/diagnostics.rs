use serde_json::Value;
use std::{
    io::{self, Write},
    sync::{OnceLock, mpsc},
};

const CAPACITY: usize = 64;

struct Diagnostics(mpsc::SyncSender<Value>);

impl Diagnostics {
    fn start(mut output: impl Write + Send + 'static) -> io::Result<Self> {
        let (sender, receiver) = mpsc::sync_channel::<Value>(CAPACITY);
        std::thread::Builder::new()
            .name("agit-controller-diagnostics".into())
            .spawn(move || {
                for record in receiver {
                    if writeln!(output, "{record}").is_err() {
                        break;
                    }
                }
            })?;
        Ok(Self(sender))
    }

    fn record(&self, record: Value) {
        // A stalled log consumer must not backpressure connection or request I/O.
        let _ = self.0.try_send(record);
    }
}

pub(crate) fn record(record: Value) {
    static WRITER: OnceLock<Option<Diagnostics>> = OnceLock::new();
    if let Some(writer) = WRITER.get_or_init(|| Diagnostics::start(io::stderr()).ok()) {
        writer.record(record);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    struct PausedWriter(Option<(mpsc::Sender<()>, mpsc::Receiver<()>)>);

    impl Write for PausedWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if let Some((entered, resume)) = self.0.take() {
                entered.send(()).unwrap();
                let _ = resume.recv();
            }
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn stalled_output_does_not_block_producers_when_queue_fills() {
        let (entered, writing) = mpsc::channel();
        let (resume, paused) = mpsc::channel();
        let diagnostics = Diagnostics::start(PausedWriter(Some((entered, paused)))).unwrap();
        diagnostics.record(Value::Null);
        writing.recv_timeout(Duration::from_secs(5)).unwrap();
        let (done, finished) = mpsc::channel();
        let producer = std::thread::spawn(move || {
            for _ in 0..CAPACITY + 1 {
                diagnostics.record(Value::Null);
            }
            let _ = done.send(());
        });
        let progressed = finished.recv_timeout(Duration::from_secs(5));
        let _ = resume.send(());
        producer.join().unwrap();
        assert!(progressed.is_ok(), "diagnostics blocked on stalled output");
    }
}

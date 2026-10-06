use super::*;
use sha2::{Digest, Sha256};

const MAX_INPUT_CLIENTS: usize = 64;

#[derive(Default)]
pub(super) struct Receipts {
    clients: HashMap<uuid::Uuid, Receipt>,
}

struct Receipt {
    sequence: u64,
    digest: [u8; 32],
}

impl Receipts {
    pub fn deliver(
        &mut self,
        input: &TerminalInput,
        instance: &str,
        write: impl FnOnce(&str) -> crate::Result<()>,
    ) -> Result<serde_json::Value, RpcError> {
        let Some(delivery) = &input.delivery else {
            write(&input.data).map_err(Self::write_error)?;
            return Ok(serde_json::json!({}));
        };
        if delivery.instance_id != instance {
            return Err(RpcError::new(
                ErrorCode::SessionNotFound,
                "the terminal input belongs to another daemon instance",
            ));
        }
        let client = uuid::Uuid::parse_str(&delivery.client_id)
            .map_err(|_| Self::invalid("terminal input requires a controller UUID"))?;
        let digest: [u8; 32] = Sha256::digest(input.data.as_bytes()).into();
        let sequence = delivery.sequence;
        let accepted = serde_json::json!({"sequence":sequence});
        let previous = self.clients.get(&client);
        if previous.is_some_and(|last| last.sequence == sequence && last.digest == digest) {
            return Ok(accepted);
        }
        if sequence == 0
            || previous.map_or(Some(1), |last| last.sequence.checked_add(1)) != Some(sequence)
        {
            return Err(Self::invalid(
                "terminal input must follow the acknowledged sequence",
            ));
        }
        // Receipts live as long as the shell; eviction would make old input executable again.
        if previous.is_none() && self.clients.len() >= MAX_INPUT_CLIENTS {
            return Err(RpcError::new(
                ErrorCode::QuotaExceeded,
                "the terminal cannot admit another input controller",
            ));
        }
        // Queue admission and its receipt share the daemon's dispatch lock.
        write(&input.data).map_err(Self::write_error)?;
        self.clients.insert(client, Receipt { sequence, digest });
        Ok(accepted)
    }

    fn invalid(message: &str) -> RpcError {
        RpcError::new(ErrorCode::MalformedFrame, message)
    }

    fn write_error(error: anyhow::Error) -> RpcError {
        RpcError::new(ErrorCode::Internal, error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejected_queue_admission_is_retryable_and_capacity_never_evicts_receipts() {
        let mut receipts = Receipts::default();
        let mut input: TerminalInput = serde_json::from_value(serde_json::json!({
            "terminal_id":"terminal", "data":"input", "delivery":{
                "instance_id":"instance", "client_id":uuid::Uuid::new_v4(), "sequence":1,
            },
        }))
        .unwrap();
        assert!(
            receipts
                .deliver(&input, "instance", |_| anyhow::bail!("queue full"))
                .is_err()
        );
        let mut writes = 0;
        receipts
            .deliver(&input, "instance", |_| {
                writes += 1;
                Ok(())
            })
            .unwrap();
        assert_eq!(writes, 1);
        for _ in 1..MAX_INPUT_CLIENTS {
            input.delivery.as_mut().unwrap().client_id = uuid::Uuid::new_v4().to_string();
            receipts.deliver(&input, "instance", |_| Ok(())).unwrap();
        }
        let replay = input.clone();
        input.delivery.as_mut().unwrap().client_id = uuid::Uuid::new_v4().to_string();
        assert!(
            receipts
                .deliver(&input, "instance", |_| panic!(
                    "an unadmitted client cannot write"
                ))
                .unwrap_err()
                .is(ErrorCode::QuotaExceeded)
        );
        receipts
            .deliver(&replay, "instance", |_| {
                panic!("replay cannot write even at capacity")
            })
            .unwrap();
        assert_eq!(receipts.clients.len(), MAX_INPUT_CLIENTS);
    }
}

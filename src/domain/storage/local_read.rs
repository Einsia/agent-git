//! Offline search budgets input structure before allocating values or expanding native events.

use anyhow::Result;
use serde::de::{DeserializeSeed, Error, MapAccess, SeqAccess, Visitor};
use std::fmt;

pub(crate) struct LocalReadBudget {
    remaining: usize,
    read_bytes: usize,
    deadline: crate::infra::local_git::Deadline,
}

impl LocalReadBudget {
    #[cfg(test)]
    pub(crate) fn new(remaining: usize) -> Self {
        Self::with_deadline(remaining, crate::infra::local_git::Deadline::new())
    }

    pub(crate) fn with_deadline(
        remaining: usize,
        deadline: crate::infra::local_git::Deadline,
    ) -> Self {
        Self {
            remaining,
            read_bytes: 0,
            deadline,
        }
    }

    pub(crate) fn deadline(&self) -> crate::infra::local_git::Deadline {
        self.deadline
    }

    pub(crate) fn remaining(&self) -> usize {
        self.remaining
    }

    pub(crate) fn read_bytes(&self) -> usize {
        self.read_bytes
    }

    pub(crate) fn record_read(&mut self, bytes: usize) -> Result<()> {
        self.read_bytes = self
            .read_bytes
            .checked_add(bytes)
            .ok_or_else(|| anyhow::anyhow!("local read accounting overflow"))?;
        Ok(())
    }

    pub(crate) fn spend(&mut self, amount: usize) -> Result<()> {
        anyhow::ensure!(!self.deadline.expired(), "local history deadline expired");
        if amount > self.remaining {
            self.remaining = 0;
            anyhow::bail!("local history exceeds the shared work budget");
        }
        self.remaining -= amount;
        Ok(())
    }

    /// Structural admission allocates no JSON values. Errors retain every consumed work unit.
    pub(crate) fn json(&mut self, text: &str) -> Result<()> {
        let mut decoder = serde_json::Deserializer::from_str(text);
        Node(self).deserialize(&mut decoder)?;
        decoder.end()?;
        Ok(())
    }

    pub(crate) fn lines(&mut self, text: &str) -> Result<()> {
        for line in text.split_inclusive('\n') {
            self.json(line)?;
        }
        Ok(())
    }
}

struct Node<'a>(&'a mut LocalReadBudget);

impl<'de> DeserializeSeed<'de> for Node<'_> {
    type Value = ();

    fn deserialize<D: serde::Deserializer<'de>>(self, decoder: D) -> Result<(), D::Error> {
        self.0.spend(1).map_err(D::Error::custom)?;
        decoder.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for Node<'_> {
    type Value = ();

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("bounded saved JSON")
    }

    fn visit_bool<E: Error>(self, _: bool) -> Result<(), E> {
        Ok(())
    }
    fn visit_i64<E: Error>(self, _: i64) -> Result<(), E> {
        Ok(())
    }
    fn visit_u64<E: Error>(self, _: u64) -> Result<(), E> {
        Ok(())
    }
    fn visit_f64<E: Error>(self, _: f64) -> Result<(), E> {
        Ok(())
    }
    fn visit_str<E: Error>(self, _: &str) -> Result<(), E> {
        Ok(())
    }
    fn visit_unit<E: Error>(self) -> Result<(), E> {
        Ok(())
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<(), A::Error> {
        while sequence.next_element_seed(Node(&mut *self.0))?.is_some() {}
        Ok(())
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        while map.next_key_seed(Node(&mut *self.0))?.is_some() {
            map.next_value_seed(Node(&mut *self.0))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dense_record_stops_before_the_unvisited_invalid_tail() {
        let dense = format!("[{}BROKEN]", "{},".repeat(60_000));
        let mut budget = LocalReadBudget::new(20_000);
        let error = budget.json(&dense).unwrap_err().to_string();
        assert!(
            error.contains("work budget"),
            "the decoder must stop before reaching BROKEN"
        );
        assert_eq!(budget.remaining(), 0);
        assert!(budget.json("{}").is_err());
    }

    #[test]
    fn failed_records_and_repeated_passes_keep_their_consumed_work() {
        let mut budget = LocalReadBudget::new(30);
        budget.json("{\"text\":\"saved\"}").unwrap();
        let first = budget.remaining();
        budget.json("{\"text\":\"saved\"}").unwrap();
        assert!(budget.remaining() < first);
        let before_failure = budget.remaining();
        assert!(budget.json("{\"text\":[0,0,BROKEN}").is_err());
        assert!(budget.remaining() < before_failure);
        budget.spend(budget.remaining()).unwrap();
        assert!(budget.json("{}").is_err());
    }
}

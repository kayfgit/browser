use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Batch {
    version: u8,
    document: String,
    dropped: u64,
    messages: Vec<(u64, String)>,
}

#[derive(Default)]
pub struct Receiver {
    pub epoch: u64,
    document: String,
    ack: u64,
    dropped: u64,
}
impl Receiver {
    pub fn reset(&mut self, epoch: u64) {
        *self = Self {
            epoch,
            ..Self::default()
        };
    }
    pub fn script(&self) -> String {
        format!(
            "typeof window.__servoShellRead === 'function' ? window.__servoShellRead({}, {}) : 'null'",
            serde_json::to_string(&self.document).unwrap(),
            self.ack
        )
    }
    pub fn accept(&mut self, epoch: u64, raw: &str) -> Result<Vec<String>, String> {
        if epoch != self.epoch {
            return Ok(Vec::new());
        }
        if raw.len() > 1_048_576 {
            return Err("bridge response exceeds 1 MiB".into());
        }
        if raw == "null" {
            return Ok(Vec::new());
        }
        let batch: Batch =
            serde_json::from_str(raw).map_err(|e| format!("bridge response: {e}"))?;
        if batch.version != 1
            || batch.document.is_empty()
            || batch.document.len() > 64
            || batch.messages.len() > 32
        {
            return Err("invalid bridge version, document token or batch size".into());
        }
        // Validate the complete batch before delivering or acknowledging anything.
        let mut previous = 0;
        for (seq, text) in &batch.messages {
            if *seq <= previous
                || *seq > 9_007_199_254_740_991
                || text.encode_utf16().count() > 8192
            {
                return Err("invalid bridge sequence or message size".into());
            }
            previous = *seq;
        }
        let mut ack = if batch.document == self.document {
            self.ack
        } else {
            0
        };
        let dropped = if batch.document == self.document {
            self.dropped
        } else {
            0
        };
        if batch.dropped != dropped {
            return Err(format!(
                "bridge queue overflow: {} messages dropped",
                batch.dropped
            ));
        }
        let mut messages = Vec::new();
        for (seq, text) in batch.messages {
            if seq <= ack {
                continue;
            }
            if seq != ack + 1 {
                return Err("bridge sequence gap".into());
            }
            ack = seq;
            messages.push(text);
        }
        self.document = batch.document;
        self.ack = ack;
        self.dropped = batch.dropped;
        Ok(messages)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn batch(doc: &str, messages: serde_json::Value) -> String {
        serde_json::json!({"version":1,"document":doc,"dropped":0,"messages":messages}).to_string()
    }
    #[test]
    fn retries_are_not_redelivered_and_documents_restart_sequences() {
        let mut receiver = Receiver::default();
        let data = batch(
            "first",
            serde_json::json!([[1, "page-ready"], [2, "insert-escape"]]),
        );
        assert_eq!(
            receiver.accept(0, &data).unwrap(),
            ["page-ready", "insert-escape"]
        );
        assert!(receiver.accept(0, &data).unwrap().is_empty());
        assert_eq!(
            receiver
                .accept(0, &batch("next", serde_json::json!([[1, "page-ready"]])))
                .unwrap(),
            ["page-ready"]
        );
    }
    #[test]
    fn old_navigation_cannot_deliver_or_acknowledge_new_document() {
        let mut receiver = Receiver::default();
        receiver.reset(2);
        assert!(receiver.accept(1, "not even JSON").unwrap().is_empty());
        assert_eq!(
            receiver
                .accept(2, &batch("new", serde_json::json!([[1, "page-ready"]])))
                .unwrap(),
            ["page-ready"]
        );
    }
    #[test]
    fn malformed_batch_is_rejected_before_delivery() {
        let mut receiver = Receiver::default();
        assert!(
            receiver
                .accept(
                    0,
                    &batch(
                        "doc",
                        serde_json::json!([[1, "page-ready"], [1, "insert-escape"]])
                    )
                )
                .is_err()
        );
        assert_eq!(receiver.ack, 0);
        assert!(
            receiver
                .accept(
                    0,
                    &batch(
                        "doc",
                        serde_json::json!([[1, "page-ready"], [3, "hint-exit"]])
                    )
                )
                .is_err()
        );
        assert_eq!(receiver.ack, 0);
    }
    #[test]
    fn overflow_and_unsupported_versions_stop_without_acknowledging() {
        let mut receiver = Receiver::default();
        for raw in [
            serde_json::json!({"version":2,"document":"doc","dropped":0,"messages":[[1,"page-ready"]]}),
            serde_json::json!({"version":1,"document":"doc","dropped":1,"messages":[[1,"page-ready"]]}),
        ] {
            assert!(receiver.accept(0, &raw.to_string()).is_err());
            assert_eq!(receiver.ack, 0);
        }
    }
}

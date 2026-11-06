//! Bounded live-event history for a device's sockets. Short bursts are retained
//! by both event count and encoded bytes; an overrun asks the caller to resync.
use super::wire::Outbound;
use axum::extract::ws::Message;
use serde_json::Value;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tokio::sync::{broadcast::error::RecvError, watch};

const MAX_EVENTS: usize = 4096;
const MAX_BYTES: usize = 4 * 1024 * 1024;

struct Entry {
    sequence: u64,
    value: Event,
    bytes: usize,
}
struct Buffer {
    entries: VecDeque<Entry>,
    next: u64,
    bytes: usize,
    max_events: usize,
    max_bytes: usize,
    subscribers: usize,
}
#[derive(Clone)]
pub struct Sender {
    buffer: Arc<Mutex<Buffer>>,
    changed: watch::Sender<()>,
}
pub struct Receiver {
    buffer: Arc<Mutex<Buffer>>,
    changed: watch::Receiver<()>,
    next: u64,
}
#[derive(Clone, Debug)]
pub struct Event {
    message: Message,
    generation: Option<u64>,
}
impl Event {
    pub fn outbound(self, fragmented: bool) -> Outbound {
        Outbound::new(self.message, self.generation, fragmented)
    }
    #[cfg(test)]
    fn value(self) -> Value {
        let Message::Text(text) = self.message else {
            panic!("text event required")
        };
        serde_json::from_str(&text).unwrap()
    }
}
impl Default for Sender {
    fn default() -> Self {
        Self::bounded(MAX_EVENTS, MAX_BYTES)
    }
}

impl Sender {
    pub fn new() -> Self {
        Self::default()
    }
    fn bounded(max_events: usize, max_bytes: usize) -> Self {
        let (changed, _) = watch::channel(());
        Self {
            buffer: Arc::new(Mutex::new(Buffer {
                entries: VecDeque::new(),
                next: 0,
                bytes: 0,
                max_events: max_events.max(1),
                max_bytes: max_bytes.max(1),
                subscribers: 0,
            })),
            changed,
        }
    }
    pub fn subscribe(&self) -> Receiver {
        let mut buffer = self.buffer.lock().unwrap();
        buffer.subscribers += 1;
        Receiver {
            buffer: self.buffer.clone(),
            changed: self.changed.subscribe(),
            next: buffer.next,
        }
    }
    pub fn send(&self, mut value: Value) {
        if self.changed.receiver_count() == 0 {
            let mut buffer = self.buffer.lock().unwrap();
            if buffer.subscribers == 0 {
                buffer.next += 1;
                return;
            }
        }
        let generation = value
            .as_object_mut()
            .and_then(|o| o.remove("_bridge_generation"))
            .and_then(|v| v.as_u64());
        // Encode once, using the same 16 MiB limit as RPC replies. Message's
        // UTF-8 bytes clone by sharing storage, without cloning a JSON tree.
        let message = crate::socket_message(&value);
        let Message::Text(text) = &message else {
            unreachable!("socket_message always returns text")
        };
        let bytes = text.len();
        {
            let mut buffer = self.buffer.lock().unwrap();
            let sequence = buffer.next;
            buffer.next += 1;
            if buffer.subscribers == 0 {
                return;
            }
            // A valid event larger than the normal history budget is retained
            // alone. The existing socket-message limit still caps it at 16 MiB.
            while !buffer.entries.is_empty()
                && (buffer.entries.len() >= buffer.max_events
                    || bytes > buffer.max_bytes
                    || buffer.bytes > buffer.max_bytes.saturating_sub(bytes))
            {
                let entry = buffer.entries.pop_front().unwrap();
                buffer.bytes -= entry.bytes;
            }
            buffer.bytes += bytes;
            buffer.entries.push_back(Entry {
                sequence,
                value: Event {
                    message,
                    generation,
                },
                bytes,
            });
        }
        self.changed.send_replace(());
    }
}
impl Drop for Receiver {
    fn drop(&mut self) {
        let mut buffer = self.buffer.lock().unwrap();
        buffer.subscribers -= 1;
        if buffer.subscribers == 0 {
            buffer.entries.clear();
            buffer.bytes = 0;
        }
    }
}
impl Receiver {
    pub async fn recv(&mut self) -> Result<Event, RecvError> {
        loop {
            self.changed.borrow_and_update();
            let value = {
                let buffer = self.buffer.lock().unwrap();
                let first = buffer.entries.front().map_or(buffer.next, |e| e.sequence);
                if self.next < first {
                    let missed = first - self.next;
                    self.next = first;
                    return Err(RecvError::Lagged(missed));
                }
                buffer.entries.get((self.next - first) as usize).map(|e| {
                    self.next += 1;
                    e.value.clone()
                })
            };
            if let Some(value) = value {
                return Ok(value);
            }
            self.changed
                .changed()
                .await
                .map_err(|_| RecvError::Closed)?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn a_tool_burst_survives_a_temporarily_stalled_socket() {
        let sender = Sender::new();
        let mut receiver = sender.subscribe();
        for sequence in 0..2002 {
            sender.send(json!({"sequence":sequence,"text":"tool output"}));
        }
        for sequence in 0..2002 {
            assert_eq!(receiver.recv().await.unwrap().value()["sequence"], sequence);
        }
    }

    #[tokio::test]
    async fn count_overruns_report_the_gap_before_the_retained_tail() {
        let sender = Sender::bounded(2, 1024);
        let mut slow = sender.subscribe();
        let mut fast = sender.subscribe();
        for sequence in 0..4 {
            sender.send(json!(sequence));
            assert_eq!(fast.recv().await.unwrap().value(), sequence);
        }
        assert_eq!(
            slow.recv().await.map(Event::value),
            Err(RecvError::Lagged(2))
        );
        assert_eq!(slow.recv().await.unwrap().value(), 2);
        assert_eq!(slow.recv().await.unwrap().value(), 3);
    }

    #[tokio::test]
    async fn encoded_bytes_bound_retention_even_below_the_count_limit() {
        let sender = Sender::bounded(10, 7);
        let mut receiver = sender.subscribe();
        sender.send(json!("aaa"));
        sender.send(json!("bbb"));
        assert_eq!(sender.buffer.lock().unwrap().bytes, 5);
        assert_eq!(
            receiver.recv().await.map(Event::value),
            Err(RecvError::Lagged(1))
        );
        assert_eq!(receiver.recv().await.unwrap().value(), "bbb");
        sender.send(json!("oversized"));
        assert_eq!(sender.buffer.lock().unwrap().bytes, 11);
        assert_eq!(receiver.recv().await.unwrap().value(), "oversized");
        sender.send(json!("ok"));
        assert_eq!(receiver.recv().await.unwrap().value(), "ok");
        assert_eq!(sender.buffer.lock().unwrap().bytes, 4);
    }

    #[tokio::test]
    async fn history_is_released_when_the_last_socket_disconnects() {
        let sender = Sender::new();
        let first = sender.subscribe();
        let second = sender.subscribe();
        sender.send(json!("retained"));
        drop(first);
        assert!(!sender.buffer.lock().unwrap().entries.is_empty());
        drop(second);
        assert_eq!(sender.buffer.lock().unwrap().bytes, 0);
        sender.send(json!("nobody is listening"));
        assert!(sender.buffer.lock().unwrap().entries.is_empty());
        let mut receiver = sender.subscribe();
        sender.send(json!("new socket"));
        assert_eq!(receiver.recv().await.unwrap().value(), "new socket");
    }

    #[tokio::test]
    async fn encoded_events_preserve_generation_without_exposing_it_on_the_wire() {
        let sender = Sender::new();
        let mut receiver = sender.subscribe();
        sender.send(json!({"_bridge_generation":7,"type":"thread","text":"hello"}));
        let event = receiver.recv().await.unwrap();
        let outbound = event.outbound(false);
        assert_eq!(outbound.generation, Some(7));
        let Message::Text(text) = outbound.frames.front().unwrap() else {
            panic!("text required")
        };
        assert_eq!(
            serde_json::from_str::<Value>(text).unwrap(),
            json!({"type":"thread","text":"hello"})
        );
    }

    #[tokio::test]
    async fn events_above_the_socket_limit_are_bounded_errors() {
        let sender = Sender::new();
        let mut receiver = sender.subscribe();
        sender.send(json!("x".repeat(crate::MAX_SOCKET_REPLY + 1)));
        let event = receiver.recv().await.unwrap().value();
        assert_eq!(event["type"], "error");
        assert_eq!(event["error"], crate::OVERSIZED_REPLY_ERROR);
        assert!(sender.buffer.lock().unwrap().bytes < 1024);
    }

    #[tokio::test]
    async fn subscribers_start_at_the_tail_and_drain_before_close() {
        let sender = Sender::new();
        sender.send(json!("before subscription"));
        let mut receiver = sender.subscribe();
        sender.send(json!("after subscription"));
        drop(sender);
        assert_eq!(receiver.recv().await.unwrap().value(), "after subscription");
        assert_eq!(
            receiver.recv().await.map(Event::value),
            Err(RecvError::Closed)
        );
    }

    #[tokio::test]
    async fn a_waiting_subscriber_wakes_when_an_event_arrives() {
        let sender = Sender::new();
        let mut receiver = sender.subscribe();
        let mut waiting = Box::pin(receiver.recv());
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), &mut waiting)
                .await
                .is_err()
        );
        sender.send(json!("ready"));
        assert_eq!(waiting.await.unwrap().value(), "ready");
    }
}

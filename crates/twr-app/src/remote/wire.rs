use axum::extract::ws::Message;
use serde_json::json;
use std::collections::VecDeque;

pub const CHUNK_BYTES: usize = 32 * 1024;

/// One logical message. The writer multiplexes replies, control events and PTY
/// output between these bounded frames, preserving order within each event lane.
pub struct Outbound {
    pub generation: Option<u64>,
    pub transfer: Option<String>,
    pub frames: VecDeque<Message>,
}
impl Outbound {
    pub fn new(message: Message, generation: Option<u64>, fragmented: bool) -> Self {
        let mut result = Self {
            generation,
            transfer: None,
            frames: VecDeque::new(),
        };
        let Message::Text(text) = &message else {
            result.frames.push_back(message);
            return result;
        };
        if !fragmented || text.len() <= CHUNK_BYTES {
            result.frames.push_back(message);
            return result;
        }
        let transfer = uuid::Uuid::new_v4().to_string();
        let mut pieces = Vec::new();
        let mut start = 0;
        while start < text.len() {
            let mut end = (start + CHUNK_BYTES).min(text.len());
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            pieces.push(&text[start..end]);
            start = end;
        }
        for (index, data) in pieces.iter().enumerate() {
            result.frames.push_back(Message::Text(
                json!({
                    "type":"frame-part", "transfer":transfer,
                    "index":index, "count":pieces.len(), "data":data,
                })
                .to_string()
                .into(),
            ));
        }
        result.transfer = Some(transfer);
        result
    }
    pub fn cancel(&self) -> Option<Message> {
        self.transfer.as_ref().map(|id| {
            Message::Text(
                json!({"type":"frame-cancel","transfer":id})
                    .to_string()
                    .into(),
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn large_unicode_messages_round_trip_through_bounded_frames() {
        let original = json!({"id":"reply","result":"界\\\"".repeat(100000)}).to_string();
        let outgoing = Outbound::new(Message::Text(original.clone().into()), Some(7), true);
        let mut restored = String::new();
        for (index, message) in outgoing.frames.iter().enumerate() {
            let Message::Text(text) = message else {
                panic!("text required")
            };
            let part: serde_json::Value = serde_json::from_str(text).unwrap();
            assert_eq!(part["index"], index);
            assert_eq!(part["count"], outgoing.frames.len());
            let data = part["data"].as_str().unwrap();
            assert!(data.len() <= CHUNK_BYTES);
            assert!(text.len() <= CHUNK_BYTES * 2 + 256);
            restored.push_str(data);
        }
        assert_eq!(restored, original);
        assert!(outgoing.cancel().is_some());
    }
}

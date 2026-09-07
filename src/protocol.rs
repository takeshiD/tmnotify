//! Bounded, versioned messages exchanged over tmnotify's private Unix sockets.
//!
//! This module deliberately stops at the wire boundary. It validates framing
//! and envelope semantics, but leaves domain conversion to the daemon so wire
//! representations cannot leak into scheduler state.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use uuid::Uuid;

pub const PROTOCOL_VERSION: u16 = 1;
pub const MAX_REQUEST_BYTES: usize = 64 * 1024;
pub const REQUEST_CACHE_CAPACITY: usize = 10_000;
pub const REQUEST_CACHE_TTL: Duration = Duration::from_secs(10 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestKind {
    Send,
    Update,
    Dismiss,
    Jump,
    History,
    HistoryClear,
    RendererRedeem,
}

impl RequestKind {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "send" => Some(Self::Send),
            "update" => Some(Self::Update),
            "dismiss" => Some(Self::Dismiss),
            "jump" => Some(Self::Jump),
            "history" => Some(Self::History),
            "history-clear" => Some(Self::HistoryClear),
            "renderer-redeem" => Some(Self::RendererRedeem),
            _ => None,
        }
    }
}

/// A validated request plus the complete JSON value used for replay detection.
#[derive(Debug, Clone, PartialEq)]
pub struct RequestEnvelope {
    pub request_id: Uuid,
    pub kind: RequestKind,
    pub value: Value,
}

impl RequestEnvelope {
    pub fn decode(frame: &[u8]) -> Result<Self, ProtocolError> {
        if frame.len() > MAX_REQUEST_BYTES {
            return Err(ProtocolError::FrameTooLarge {
                limit: MAX_REQUEST_BYTES,
            });
        }

        let value: Value = serde_json::from_slice(frame).map_err(ProtocolError::MalformedJson)?;
        let object = value
            .as_object()
            .ok_or(ProtocolError::RequestMustBeObject)?;

        let version = object
            .get("version")
            .and_then(Value::as_u64)
            .ok_or(ProtocolError::MissingOrInvalidField("version"))?;
        if version != u64::from(PROTOCOL_VERSION) {
            return Err(ProtocolError::UnsupportedVersion(version));
        }

        let request_id = object
            .get("request_id")
            .and_then(Value::as_str)
            .ok_or(ProtocolError::MissingOrInvalidField("request_id"))?
            .parse()
            .map_err(|_| ProtocolError::MissingOrInvalidField("request_id"))?;

        let request_type = object
            .get("type")
            .and_then(Value::as_str)
            .ok_or(ProtocolError::MissingOrInvalidField("type"))?;
        let kind = RequestKind::parse(request_type)
            .ok_or_else(|| ProtocolError::UnknownRequestType(request_type.to_owned()))?;

        Ok(Self {
            request_id,
            kind,
            value,
        })
    }

    pub fn payload_matches(&self, other: &Self) -> bool {
        self.request_id == other.request_id && self.value == other.value
    }
}

/// Incrementally extracts newline-delimited frames while enforcing the limit
/// before allocation can grow beyond one complete request.
#[derive(Debug)]
pub struct FrameDecoder {
    buffer: Vec<u8>,
    max_frame_bytes: usize,
}

impl Default for FrameDecoder {
    fn default() -> Self {
        Self::new(MAX_REQUEST_BYTES)
    }
}

impl FrameDecoder {
    pub fn new(max_frame_bytes: usize) -> Self {
        assert!(max_frame_bytes > 0, "frame limit must be positive");
        Self {
            buffer: Vec::new(),
            max_frame_bytes,
        }
    }

    /// Adds bytes and returns every complete frame in arrival order.
    ///
    /// A frame excludes its newline. Callers should close the connection after
    /// `FrameTooLarge`; the decoder clears its partial buffer before returning.
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<Vec<u8>>, ProtocolError> {
        let mut frames = Vec::new();
        let mut start = 0;

        for (index, byte) in bytes.iter().enumerate() {
            if *byte != b'\n' {
                continue;
            }

            if let Err(error) = self.append_bounded(&bytes[start..index]) {
                self.buffer.clear();
                return Err(error);
            }
            frames.push(std::mem::take(&mut self.buffer));
            start = index + 1;
        }

        if let Err(error) = self.append_bounded(&bytes[start..]) {
            self.buffer.clear();
            return Err(error);
        }

        Ok(frames)
    }

    pub fn pending_bytes(&self) -> usize {
        self.buffer.len()
    }

    fn append_bounded(&mut self, bytes: &[u8]) -> Result<(), ProtocolError> {
        if self.buffer.len().saturating_add(bytes.len()) > self.max_frame_bytes {
            return Err(ProtocolError::FrameTooLarge {
                limit: self.max_frame_bytes,
            });
        }
        self.buffer.extend_from_slice(bytes);
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResponseEnvelope<T> {
    pub version: u16,
    pub request_id: Uuid,
    #[serde(flatten)]
    pub result: T,
}

impl<T> ResponseEnvelope<T> {
    pub fn new(request_id: Uuid, result: T) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            request_id,
            result,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Disposition {
    Queued,
    Visible,
    Updated,
    Duplicate,
    Suppressed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Acknowledgement {
    pub accepted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notification_id: Option<Uuid>,
    pub history_persisted: bool,
    pub disposition: Disposition,
}

#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error("request exceeds the {limit}-byte limit")]
    FrameTooLarge { limit: usize },
    #[error("malformed JSON: {0}")]
    MalformedJson(serde_json::Error),
    #[error("request must be a JSON object")]
    RequestMustBeObject,
    #[error("missing or invalid field: {0}")]
    MissingOrInvalidField(&'static str),
    #[error("unsupported protocol version: {0}")]
    UnsupportedVersion(u64),
    #[error("unknown request type: {0}")]
    UnknownRequestType(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheLookup<T> {
    Miss,
    Replay(T),
    PayloadMismatch,
}

#[derive(Debug)]
struct CacheEntry<T> {
    payload: Value,
    response: T,
    touched_at: Instant,
}

/// A small explicit LRU used to make client retries idempotent.
#[derive(Debug)]
pub struct RequestResultCache<T> {
    entries: HashMap<Uuid, CacheEntry<T>>,
    order: VecDeque<Uuid>,
    capacity: usize,
    ttl: Duration,
}

impl<T: Clone> Default for RequestResultCache<T> {
    fn default() -> Self {
        Self::new(REQUEST_CACHE_CAPACITY, REQUEST_CACHE_TTL)
    }
}

impl<T: Clone> RequestResultCache<T> {
    pub fn new(capacity: usize, ttl: Duration) -> Self {
        assert!(capacity > 0, "cache capacity must be positive");
        Self {
            entries: HashMap::with_capacity(capacity),
            order: VecDeque::with_capacity(capacity),
            capacity,
            ttl,
        }
    }

    pub fn lookup(&mut self, request: &RequestEnvelope, now: Instant) -> CacheLookup<T> {
        self.expire(now);
        let Some(entry) = self.entries.get_mut(&request.request_id) else {
            return CacheLookup::Miss;
        };

        if entry.payload != request.value {
            return CacheLookup::PayloadMismatch;
        }

        entry.touched_at = now;
        let response = entry.response.clone();
        self.touch(request.request_id);
        CacheLookup::Replay(response)
    }

    pub fn insert(&mut self, request: &RequestEnvelope, response: T, now: Instant) {
        self.expire(now);
        self.entries.insert(
            request.request_id,
            CacheEntry {
                payload: request.value.clone(),
                response,
                touched_at: now,
            },
        );
        self.touch(request.request_id);

        while self.entries.len() > self.capacity {
            if let Some(oldest) = self.order.pop_front() {
                self.entries.remove(&oldest);
            }
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn touch(&mut self, request_id: Uuid) {
        if let Some(index) = self.order.iter().position(|id| *id == request_id) {
            self.order.remove(index);
        }
        self.order.push_back(request_id);
    }

    fn expire(&mut self, now: Instant) {
        while let Some(request_id) = self.order.front().copied() {
            let Some(entry) = self.entries.get(&request_id) else {
                self.order.pop_front();
                continue;
            };
            if now.saturating_duration_since(entry.touched_at) < self.ttl {
                break;
            }
            self.order.pop_front();
            self.entries.remove(&request_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(id: Uuid, body: &str) -> RequestEnvelope {
        RequestEnvelope::decode(
            format!(r#"{{"version":1,"request_id":"{id}","type":"send","body":"{body}"}}"#)
                .as_bytes(),
        )
        .unwrap()
    }

    #[test]
    fn decoder_handles_partial_and_multiplexed_frames() {
        let mut decoder = FrameDecoder::new(16);
        assert!(decoder.push(b"one").unwrap().is_empty());
        assert_eq!(decoder.pending_bytes(), 3);
        assert_eq!(
            decoder.push(b"\ntwo\nthree").unwrap(),
            vec![b"one".to_vec(), b"two".to_vec()]
        );
        assert_eq!(decoder.pending_bytes(), 5);
        assert_eq!(decoder.push(b"\n").unwrap(), vec![b"three".to_vec()]);
    }

    #[test]
    fn decoder_rejects_a_partial_frame_as_soon_as_it_is_too_large() {
        let mut decoder = FrameDecoder::new(4);
        assert!(decoder.push(b"1234").unwrap().is_empty());
        assert!(matches!(
            decoder.push(b"5"),
            Err(ProtocolError::FrameTooLarge { limit: 4 })
        ));
        assert_eq!(decoder.pending_bytes(), 0);
    }

    #[test]
    fn envelope_ignores_unknown_fields_but_keeps_them_for_replay_matching() {
        let id = Uuid::now_v7();
        let decoded = RequestEnvelope::decode(
            format!(r#"{{"version":1,"request_id":"{id}","type":"jump","future":true}}"#)
                .as_bytes(),
        )
        .unwrap();
        assert_eq!(decoded.kind, RequestKind::Jump);
        assert_eq!(decoded.value["future"], true);
    }

    #[test]
    fn envelope_rejects_bad_envelope_semantics() {
        assert!(matches!(
            RequestEnvelope::decode(br#"{"version":2,"request_id":"bad","type":"send"}"#),
            Err(ProtocolError::UnsupportedVersion(2))
        ));
        assert!(matches!(
            RequestEnvelope::decode(br#"{"version":1,"request_id":"bad","type":"send"}"#),
            Err(ProtocolError::MissingOrInvalidField("request_id"))
        ));
        let id = Uuid::now_v7();
        assert!(matches!(
            RequestEnvelope::decode(
                format!(r#"{{"version":1,"request_id":"{id}","type":"future"}}"#).as_bytes()
            ),
            Err(ProtocolError::UnknownRequestType(kind)) if kind == "future"
        ));
    }

    #[test]
    fn cache_replays_only_an_identical_payload() {
        let id = Uuid::now_v7();
        let now = Instant::now();
        let mut cache = RequestResultCache::new(2, Duration::from_secs(60));
        let original = request(id, "one");
        cache.insert(&original, "accepted", now);

        assert_eq!(
            cache.lookup(&original, now),
            CacheLookup::Replay("accepted")
        );
        assert_eq!(
            cache.lookup(&request(id, "two"), now),
            CacheLookup::PayloadMismatch
        );
    }

    #[test]
    fn cache_is_lru_and_expires_entries() {
        let now = Instant::now();
        let first = request(Uuid::now_v7(), "first");
        let second = request(Uuid::now_v7(), "second");
        let third = request(Uuid::now_v7(), "third");
        let mut cache = RequestResultCache::new(2, Duration::from_secs(5));

        cache.insert(&first, 1, now);
        cache.insert(&second, 2, now);
        assert_eq!(cache.lookup(&first, now), CacheLookup::Replay(1));
        cache.insert(&third, 3, now);
        assert_eq!(cache.lookup(&second, now), CacheLookup::Miss);
        assert_eq!(cache.lookup(&first, now), CacheLookup::Replay(1));
        assert_eq!(
            cache.lookup(&first, now + Duration::from_secs(5)),
            CacheLookup::Miss
        );
    }
}

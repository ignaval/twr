//! Content-addressed blob store and the 4 KB payload cap.
//!
//! Event `data` is capped at [`PAYLOAD_CAP`] serialized bytes. [`cap_payload`] moves the largest
//! string fields (then, if still needed, array/object fields) into a [`BlobStore`], leaves a
//! short preview in place, adds a sibling `<field>_ref: "sha256:<hex>"` and records the field
//! name in the object's reserved `_capped` list. [`hydrate`] reverses it, following only fields
//! listed in `_capped`.
//!
//! Ref key naming: `<field>_ref`, except `<stem>_summary` fields, which fill the schema's
//! `<stem>_ref` field (`input_summary` -> `input_ref`, `output_summary` -> `output_ref`) when it
//! is `null`; if the adapter already set that pointer, `<stem>_summary_ref` is used instead.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Mutex;

use anyhow::{anyhow, Context};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::event::EventBody;

/// Maximum serialized size of an event's `data`, in bytes.
pub const PAYLOAD_CAP: usize = 4096;

/// Maximum size of a preview left in place of a moved string, in bytes.
pub const PREVIEW_MAX: usize = 512;

/// Containers smaller than this (serialized) are never moved.
const MIN_CONTAINER: usize = 128;

/// Reserved key listing the fields [`cap_payload`] moved out of an object
/// (`"_capped": ["text", "output_summary"]`). [`hydrate`] follows only these.
pub const CAPPED_KEY: &str = "_capped";

/// Prefix of every blob pointer.
pub const REF_PREFIX: &str = "sha256:";

/// Content-addressed storage for large payload fields and raw runtime messages.
pub trait BlobStore: Send + Sync {
    /// Stores `bytes` and returns its pointer, `sha256:<hex>`. Idempotent.
    fn put(&self, bytes: &[u8]) -> anyhow::Result<String>;
    /// Fetches the bytes for a pointer returned by [`BlobStore::put`].
    fn get(&self, blob_ref: &str) -> anyhow::Result<Option<Vec<u8>>>;
}

/// Computes the pointer for `bytes` without storing them.
pub fn blob_ref(bytes: &[u8]) -> String {
    format!("{REF_PREFIX}{}", hex::encode(Sha256::digest(bytes)))
}

/// Whether `s` looks like a blob pointer (`sha256:` + 64 lowercase hex digits).
pub fn is_blob_ref(s: &str) -> bool {
    s.strip_prefix(REF_PREFIX)
        .is_some_and(|h| h.len() == 64 && h.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
}

/// In-memory [`BlobStore`], for tests and clients.
#[derive(Debug, Default)]
pub struct MemoryBlobStore {
    blobs: Mutex<HashMap<String, Vec<u8>>>,
}

impl MemoryBlobStore {
    /// Creates an empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of stored blobs.
    pub fn len(&self) -> usize {
        self.blobs.lock().expect("blob store poisoned").len()
    }

    /// Whether the store is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl BlobStore for MemoryBlobStore {
    fn put(&self, bytes: &[u8]) -> anyhow::Result<String> {
        let r = blob_ref(bytes);
        self.blobs
            .lock()
            .map_err(|_| anyhow!("blob store poisoned"))?
            .entry(r.clone())
            .or_insert_with(|| bytes.to_vec());
        Ok(r)
    }

    fn get(&self, blob_ref: &str) -> anyhow::Result<Option<Vec<u8>>> {
        Ok(self
            .blobs
            .lock()
            .map_err(|_| anyhow!("blob store poisoned"))?
            .get(blob_ref)
            .cloned())
    }
}

/// Truncates `s` to at most `max` bytes on a UTF-8 character boundary.
pub fn truncate_utf8(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Keys that are never moved to the blob store (identifiers, existing pointers, the marker).
fn is_protected(key: &str) -> bool {
    key == "id"
        || key == "thread"
        || key == "ext"
        || key == CAPPED_KEY
        || key.ends_with("_id")
        || key.ends_with("_ids")
        || key.ends_with("_ref")
}

/// Top-level payload fields that hold typed structs (`attention.resolved.answer`,
/// `turn.ended.usage`). They are never moved wholesale: an empty `{}` in their place would lose
/// required fields (`answer.decision`). Their string fields may still move.
const TYPED_STRUCT_FIELDS: &[&str] = &["answer", "usage"];

/// The sibling key that will hold the pointer for `key` in `obj`, or `None` if no free key
/// exists (the field is then not moved).
///
/// `<stem>_summary` uses the schema's `<stem>_ref` when that key is present and `null`;
/// everything else uses `<key>_ref`, which must be absent.
fn ref_key_for(obj: &Map<String, Value>, key: &str) -> Option<String> {
    if let Some(stem) = key.strip_suffix("_summary") {
        let schema_key = format!("{stem}_ref");
        if obj.get(&schema_key) == Some(&Value::Null) {
            return Some(schema_key);
        }
    }
    let generic = format!("{key}_ref");
    (!obj.contains_key(&generic)).then_some(generic)
}

#[derive(Clone, Debug)]
enum Seg {
    Key(String),
    Idx(usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CandKind {
    Str,
    Container,
}

#[derive(Debug)]
struct Candidate {
    parent: Vec<Seg>,
    key: String,
    /// Exact number of serialized bytes the move removes (always > 0).
    saved: usize,
}

fn json_len(v: &Value) -> usize {
    serde_json::to_vec(v).map(|b| b.len()).unwrap_or(usize::MAX)
}

fn str_json_len(s: &str) -> usize {
    serde_json::to_vec(s).map(|b| b.len()).unwrap_or(usize::MAX)
}

/// Serialized length of a blob pointer string (`"sha256:<64 hex>"`, quotes included).
const REF_JSON_LEN: usize = 2 + REF_PREFIX.len() + 64;

/// Bytes a move of `obj[key]` adds besides the value itself shrinking: the pointer (filling a
/// `null` schema key or adding a `"<ref_key>":"sha256:..."` entry) and the `_capped` marker entry.
fn move_overhead(obj: &Map<String, Value>, key: &str, ref_key: &str) -> usize {
    let pointer = if obj.get(ref_key) == Some(&Value::Null) {
        REF_JSON_LEN - "null".len()
    } else {
        // `,"<ref_key>":"sha256:..."`
        1 + str_json_len(ref_key) + 1 + REF_JSON_LEN
    };
    let marker = match obj.get(CAPPED_KEY) {
        // `,"<key>"`
        Some(Value::Array(a)) if !a.is_empty() => 1 + str_json_len(key),
        // `"<key>"` into an existing empty list
        Some(Value::Array(_)) => str_json_len(key),
        // `,"_capped":["<key>"]`
        _ => 1 + str_json_len(CAPPED_KEY) + 1 + 2 + str_json_len(key),
    };
    pointer + marker
}

/// Bytes saved by moving `obj[key]` (`child`), if the move is possible and saves anything.
fn savings(obj: &Map<String, Value>, key: &str, child: &Value, kind: CandKind) -> Option<usize> {
    let (before, after) = match (kind, child) {
        (CandKind::Str, Value::String(s)) if s.len() > PREVIEW_MAX => {
            (str_json_len(s), str_json_len(truncate_utf8(s, PREVIEW_MAX)))
        }
        (CandKind::Container, Value::Array(_) | Value::Object(_)) => {
            let n = json_len(child);
            if n < MIN_CONTAINER {
                return None;
            }
            (n, 2)
        }
        _ => return None,
    };
    let ref_key = ref_key_for(obj, key)?;
    let saved = before.checked_sub(after + move_overhead(obj, key, &ref_key))?;
    (saved > 0).then_some(saved)
}

fn collect(v: &Value, path: &mut Vec<Seg>, kind: CandKind, out: &mut Vec<Candidate>) {
    match v {
        Value::Object(map) => {
            for (k, child) in map {
                let typed_struct = path.is_empty() && TYPED_STRUCT_FIELDS.contains(&k.as_str());
                let movable = !is_protected(k) && !(kind == CandKind::Container && typed_struct);
                if movable {
                    if let Some(saved) = savings(map, k, child, kind) {
                        out.push(Candidate {
                            parent: path.clone(),
                            key: k.clone(),
                            saved,
                        });
                    }
                }
                // Protected keys are not moved themselves, but `ext` content may be.
                if !k.ends_with("_ref") && k != CAPPED_KEY {
                    path.push(Seg::Key(k.clone()));
                    collect(child, path, kind, out);
                    path.pop();
                }
            }
        }
        Value::Array(items) => {
            for (i, child) in items.iter().enumerate() {
                path.push(Seg::Idx(i));
                collect(child, path, kind, out);
                path.pop();
            }
        }
        _ => {}
    }
}

fn navigate<'a>(root: &'a mut Value, path: &[Seg]) -> Option<&'a mut Map<String, Value>> {
    let mut cur = root;
    for seg in path {
        cur = match (seg, cur) {
            (Seg::Key(k), Value::Object(m)) => m.get_mut(k)?,
            (Seg::Idx(i), Value::Array(a)) => a.get_mut(*i)?,
            _ => return None,
        };
    }
    cur.as_object_mut()
}

/// Picks the next field to move. Strings: the one saving the most. Containers: the one saving
/// the least that still brings the payload under the cap (so the deepest sufficient container
/// goes, not its parent), else the one saving the most.
fn pick(data: &Value, kind: CandKind, excess: usize) -> Option<Candidate> {
    let mut out = Vec::new();
    collect(data, &mut Vec::new(), kind, &mut out);
    // Ties keep the first-found candidate: `min_by_key` returns the first minimum, and
    // `rev` + `max_by_key` the first maximum.
    if kind == CandKind::Container {
        let sufficient = out
            .iter()
            .enumerate()
            .filter(|(_, c)| c.saved >= excess)
            .min_by_key(|(_, c)| c.saved)
            .map(|(i, _)| i);
        if let Some(i) = sufficient {
            return Some(out.swap_remove(i));
        }
    }
    out.into_iter().rev().max_by_key(|c| c.saved)
}

/// Moves `obj[key]` to the store, leaving a preview, a pointer sibling and a `_capped` entry.
fn move_field(
    obj: &mut Map<String, Value>,
    key: &str,
    store: &dyn BlobStore,
) -> anyhow::Result<()> {
    let ref_key = ref_key_for(obj, key).ok_or_else(|| anyhow!("no free ref key for {key}"))?;
    let value = obj
        .get_mut(key)
        .ok_or_else(|| anyhow!("field {key} vanished"))?;
    let (bytes, preview) = match value {
        Value::String(s) => (
            s.as_bytes().to_vec(),
            Value::String(truncate_utf8(s, PREVIEW_MAX).to_string()),
        ),
        Value::Array(_) => (serde_json::to_vec(value)?, Value::Array(Vec::new())),
        Value::Object(_) => (serde_json::to_vec(value)?, Value::Object(Map::new())),
        _ => return Err(anyhow!("field {key} is not movable")),
    };
    let r = store.put(&bytes).context("storing payload blob")?;
    *value = preview;
    obj.insert(ref_key, Value::String(r));
    match obj
        .entry(CAPPED_KEY)
        .or_insert_with(|| Value::Array(Vec::new()))
    {
        Value::Array(list) => list.push(Value::String(key.to_string())),
        other => *other = Value::Array(vec![Value::String(key.to_string())]),
    }
    Ok(())
}

/// Caps serialized `data` at [`PAYLOAD_CAP`] bytes by moving fields to `store`.
///
/// Strings longer than [`PREVIEW_MAX`] are moved (the one saving the most bytes first) and
/// replaced by a UTF-8-safe preview of at most [`PREVIEW_MAX`] bytes. If the payload is still
/// too large, whole arrays/objects are moved and replaced by an empty array/object. A field is
/// only moved when the move shrinks the payload once the pointer and the `_capped` entry are
/// counted, so capping never makes a payload larger.
///
/// Never moved: identifier fields (`id`, `thread`, `*_id`, `*_ids`), existing `*_ref`
/// pointers, the `_capped` marker, and (wholesale) the typed top-level structs `answer` and
/// `usage`.
///
/// Returns the number of fields moved. Idempotent: an already-capped payload is left as is.
/// A payload made only of protected or tiny fields may remain over the cap; [`cap_body`]
/// refuses those.
pub fn cap_payload(data: &mut Value, store: &dyn BlobStore) -> anyhow::Result<usize> {
    let mut moved = 0;
    for kind in [CandKind::Str, CandKind::Container] {
        loop {
            let len = json_len(data);
            if len <= PAYLOAD_CAP {
                break;
            }
            let Some(c) = pick(data, kind, len - PAYLOAD_CAP) else {
                break;
            };
            let obj = navigate(data, &c.parent).ok_or_else(|| anyhow!("bad candidate path"))?;
            move_field(obj, &c.key, store)?;
            moved += 1;
        }
    }
    Ok(moved)
}

/// Why [`cap_body`] refused a body.
#[derive(Debug, thiserror::Error)]
pub enum CapError {
    /// Nothing movable is left and the payload is still over [`PAYLOAD_CAP`] (many small fields,
    /// or oversized identifiers, which are never moved).
    #[error("{kind} payload is {size} bytes after capping, over the {PAYLOAD_CAP}-byte cap")]
    TooLarge {
        /// Event kind.
        kind: String,
        /// Serialized size after every possible move.
        size: usize,
    },
    /// Serialization or blob store failure.
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// Applies [`cap_payload`] to a typed body, returning the capped body.
///
/// Unlike [`cap_payload`], the result is guaranteed to be within [`PAYLOAD_CAP`]: a body that
/// cannot be brought under it is refused with [`CapError::TooLarge`] (nothing is stored for it
/// beyond blobs `store` already received).
pub fn cap_body(body: &EventBody, store: &dyn BlobStore) -> Result<EventBody, CapError> {
    let mut data = body.data().map_err(anyhow::Error::from)?;
    let moved = cap_payload(&mut data, store)?;
    let size = json_len(&data);
    if size > PAYLOAD_CAP {
        return Err(CapError::TooLarge {
            kind: body.kind().to_string(),
            size,
        });
    }
    if moved == 0 {
        return Ok(body.clone());
    }
    EventBody::from_kind_data(body.kind(), data).map_err(|e| CapError::Other(e.into()))
}

/// Schema-owned blob pointers a producer may set itself (the full input / output of a tool
/// call). They are never stripped: [`cap_payload`] fills them only when they are `null`, and
/// [`hydrate`] prefers the generic `<stem>_summary_ref` over them, so a producer's pointer here
/// is never taken for a moved field.
const PRODUCER_REFS: &[&str] = &["input_ref", "output_ref"];

/// Removes, at every depth, the capping metadata only [`cap_payload`] may write: the reserved
/// `_capped` marker, and every `<field>_ref` key whose `<field>` sibling is present (except the
/// schema-owned [`PRODUCER_REFS`]). Applied to what adapters and runners send before the hub
/// caps it, so only hub-generated metadata ever drives [`hydrate`] (otherwise a producer could
/// make replay restore a value the live state never saw). Returns how many keys went.
pub fn strip_capping_metadata(v: &mut Value) -> usize {
    let mut removed = 0;
    match v {
        Value::Object(map) => {
            if map.remove(CAPPED_KEY).is_some() {
                removed += 1;
            }
            let refs: Vec<String> = map
                .keys()
                .filter(|k| {
                    !PRODUCER_REFS.contains(&k.as_str())
                        && k.strip_suffix("_ref")
                            .is_some_and(|f| !f.is_empty() && map.contains_key(f))
                })
                .cloned()
                .collect();
            for k in refs {
                map.remove(&k);
                removed += 1;
            }
            for c in map.values_mut() {
                removed += strip_capping_metadata(c);
            }
        }
        Value::Array(items) => {
            for c in items {
                removed += strip_capping_metadata(c);
            }
        }
        _ => {}
    }
    removed
}

/// [`strip_capping_metadata`] on a typed body: the cleaned body and how many keys went
/// (`None` when there was nothing to remove).
pub fn strip_body_capping_metadata(
    body: &EventBody,
) -> Result<Option<(EventBody, usize)>, serde_json::Error> {
    let mut data = body.data()?;
    let removed = strip_capping_metadata(&mut data);
    if removed == 0 {
        return Ok(None);
    }
    Ok(Some((
        EventBody::from_kind_data(body.kind(), data)?,
        removed,
    )))
}

/// Whether `v` contains a `_capped` marker anywhere (fields moved by [`cap_payload`] that
/// hydration would restore).
pub fn has_capped(v: &Value) -> bool {
    match v {
        Value::Object(m) => {
            m.contains_key(CAPPED_KEY)
                || m.iter().any(|(k, c)| !k.ends_with("_ref") && has_capped(c))
        }
        Value::Array(a) => a.iter().any(has_capped),
        _ => false,
    }
}

/// Restores fields moved by [`cap_payload`], in place. Returns the number of fields restored.
///
/// Only fields listed in an object's `_capped` marker are restored, so pointers the runtime set
/// on its own (e.g. an `output_ref` to the full tool output whose `output_summary` happens to be
/// a prefix of it) are never followed. For a listed field `<x>`, the pointer is read from
/// `<x>_ref` first and, for `<stem>_summary` fields, from the schema's `<stem>_ref` second. A
/// listed field is restored only when it still looks like the preview `cap_payload` left (a
/// prefix of the blob for strings, an empty array/object for containers). Restored `<x>_ref`
/// keys are removed, schema pointers are reset to `null`, and the marker is removed once empty.
/// Restored content is hydrated too (a moved container may hold fields moved before it).
pub fn hydrate(data: &mut Value, store: &dyn BlobStore) -> anyhow::Result<usize> {
    hydrate_with(data, &mut |r: &str| store.get(r), false)
}

/// [`hydrate`] with blobs from `fetch` instead of a [`BlobStore`]. With `missing_ok`, a field
/// whose blob `fetch` does not have is left capped (still listed in `_capped`) instead of
/// failing.
pub fn hydrate_with(
    data: &mut Value,
    fetch: &mut dyn FnMut(&str) -> anyhow::Result<Option<Vec<u8>>>,
    missing_ok: bool,
) -> anyhow::Result<usize> {
    let mut restored = 0;
    match data {
        Value::Object(map) => {
            if let Some(Value::Array(fields)) = map.get(CAPPED_KEY) {
                let fields: Vec<Value> = fields.clone();
                let mut left = Vec::new();
                for f in fields {
                    match f.as_str() {
                        Some(field) if restore_field(map, field, fetch, missing_ok)? => {
                            restored += 1
                        }
                        _ => left.push(f),
                    }
                }
                if left.is_empty() {
                    map.remove(CAPPED_KEY);
                } else {
                    map.insert(CAPPED_KEY.to_string(), Value::Array(left));
                }
            }
            for (k, v) in map.iter_mut() {
                if !k.ends_with("_ref") && k != CAPPED_KEY {
                    restored += hydrate_with(v, fetch, missing_ok)?;
                }
            }
        }
        Value::Array(items) => {
            for v in items {
                restored += hydrate_with(v, fetch, missing_ok)?;
            }
        }
        _ => {}
    }
    Ok(restored)
}

/// The pointer [`hydrate`] would follow for `_capped` field `field` of `map`: `(ref key, ref)`.
fn capped_ref(map: &Map<String, Value>, field: &str) -> Option<(String, String)> {
    let generic = format!("{field}_ref");
    let mut ref_keys = vec![generic];
    if let Some(stem) = field.strip_suffix("_summary") {
        ref_keys.push(format!("{stem}_ref"));
    }
    ref_keys.into_iter().find_map(|k| {
        let r = map.get(&k)?.as_str()?;
        is_blob_ref(r).then(|| (k.clone(), r.to_string()))
    })
}

/// Restores one `_capped` field of `map`. Returns whether it was restored.
fn restore_field(
    map: &mut Map<String, Value>,
    field: &str,
    fetch: &mut dyn FnMut(&str) -> anyhow::Result<Option<Vec<u8>>>,
    missing_ok: bool,
) -> anyhow::Result<bool> {
    let Some((ref_key, r)) = capped_ref(map, field) else {
        return Ok(false);
    };
    let Some(current) = map.get(field) else {
        return Ok(false);
    };
    let bytes = match fetch(&r)? {
        Some(b) => b,
        None if missing_ok => return Ok(false),
        None => return Err(anyhow!("missing blob {r} for {field}")),
    };
    let value = match current {
        Value::String(preview) => {
            let full = String::from_utf8_lossy(&bytes).into_owned();
            if !full.starts_with(preview.as_str()) {
                return Ok(false);
            }
            Value::String(full)
        }
        Value::Array(a) if a.is_empty() => serde_json::from_slice(&bytes)?,
        Value::Object(o) if o.is_empty() => serde_json::from_slice(&bytes)?,
        _ => return Ok(false),
    };
    map.insert(field.to_string(), value);
    if ref_key == format!("{field}_ref") {
        map.remove(&ref_key);
    } else {
        map.insert(ref_key, Value::Null);
    }
    Ok(true)
}

/// Pointers of every `_capped` field in `v` (not looking inside moved content, which is not
/// there yet).
fn pending_refs(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::Object(map) => {
            if let Some(Value::Array(fields)) = map.get(CAPPED_KEY) {
                for f in fields.iter().filter_map(Value::as_str) {
                    if let Some((_, r)) = capped_ref(map, f) {
                        if !out.contains(&r) {
                            out.push(r);
                        }
                    }
                }
            }
            for (k, c) in map {
                if !k.ends_with("_ref") && k != CAPPED_KEY {
                    pending_refs(c, out);
                }
            }
        }
        Value::Array(a) => a.iter().for_each(|c| pending_refs(c, out)),
        _ => {}
    }
}

/// How many rounds [`hydrate_deep`] fetches before giving up (each round restores one more
/// level of nesting: a field moved, then its container moved, is two rounds).
pub const HYDRATE_MAX_ROUNDS: usize = 16;

/// Asynchronous blob source for [`hydrate_deep`] (the hub client implements it over
/// `blob.get`; closures `FnMut(String) -> impl Future` do too).
pub trait BlobFetch {
    /// Fetches a blob; `Ok(None)` when it does not exist.
    fn fetch(
        &mut self,
        blob_ref: &str,
    ) -> impl Future<Output = anyhow::Result<Option<Vec<u8>>>> + Send;
}

impl<F, Fut> BlobFetch for F
where
    F: FnMut(String) -> Fut,
    Fut: Future<Output = anyhow::Result<Option<Vec<u8>>>> + Send,
{
    fn fetch(
        &mut self,
        blob_ref: &str,
    ) -> impl Future<Output = anyhow::Result<Option<Vec<u8>>>> + Send {
        self(blob_ref.to_string())
    }
}

/// [`hydrate`] with blobs fetched on demand: fetches the pointers of every `_capped` field,
/// restores them, and repeats on the restored content (a container moved after one of its
/// fields was moved holds that field's pointer only in its own blob) until no restorable
/// marker is left, for at most [`HYDRATE_MAX_ROUNDS`] rounds. Each blob is fetched once.
/// A pointer whose blob does not exist is an error, as in [`hydrate`].
pub async fn hydrate_deep<F: BlobFetch>(data: &mut Value, fetch: &mut F) -> anyhow::Result<usize> {
    let mut cache: HashMap<String, Vec<u8>> = HashMap::new();
    let mut restored = 0;
    for _ in 0..HYDRATE_MAX_ROUNDS {
        let mut refs = Vec::new();
        pending_refs(data, &mut refs);
        refs.retain(|r| !cache.contains_key(r));
        if refs.is_empty() {
            return Ok(restored);
        }
        for r in refs {
            let bytes = fetch
                .fetch(&r)
                .await?
                .ok_or_else(|| anyhow!("missing blob {r}"))?;
            cache.insert(r, bytes);
        }
        restored += hydrate_with(data, &mut |r: &str| Ok(cache.get(r).cloned()), true)?;
    }
    let mut refs = Vec::new();
    pending_refs(data, &mut refs);
    if refs.iter().any(|r| !cache.contains_key(r)) {
        return Err(anyhow!(
            "payload is nested deeper than {HYDRATE_MAX_ROUNDS} capped levels"
        ));
    }
    Ok(restored)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{
        AgentText, AgentToolCall, Answer, AttentionResolved, Event, JsonMap, ResolvedBy, Source,
    };
    use serde_json::json;

    fn size(v: &Value) -> usize {
        serde_json::to_vec(v).unwrap().len()
    }

    /// Builds `{"text": <string>}` with serialized size exactly `n` bytes.
    fn text_payload(n: usize) -> Value {
        let overhead = size(&json!({"text": ""}));
        json!({"text": "x".repeat(n - overhead)})
    }

    #[test]
    fn blob_ref_format() {
        let r = blob_ref(b"abc");
        assert_eq!(
            r,
            "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert!(is_blob_ref(&r));
        assert!(!is_blob_ref("sha256:xyz"));
        assert!(!is_blob_ref("md5:ba7816bf8f01cfea414140de5dae2223"));
    }

    #[test]
    fn memory_store_put_get_idempotent() {
        let s = MemoryBlobStore::new();
        assert!(s.is_empty());
        let a = s.put(b"hello").unwrap();
        let b = s.put(b"hello").unwrap();
        assert_eq!(a, b);
        assert_eq!(s.len(), 1);
        assert_eq!(s.get(&a).unwrap().unwrap(), b"hello");
        assert!(s.get(&blob_ref(b"other")).unwrap().is_none());
    }

    #[test]
    fn exact_boundary_is_not_capped() {
        let store = MemoryBlobStore::new();
        let mut at_cap = text_payload(PAYLOAD_CAP);
        assert_eq!(size(&at_cap), PAYLOAD_CAP);
        let before = at_cap.clone();
        assert_eq!(cap_payload(&mut at_cap, &store).unwrap(), 0);
        assert_eq!(at_cap, before);
        assert!(store.is_empty());

        let mut over = text_payload(PAYLOAD_CAP + 1);
        let original = over["text"].as_str().unwrap().to_string();
        assert_eq!(cap_payload(&mut over, &store).unwrap(), 1);
        assert!(size(&over) <= PAYLOAD_CAP);
        assert_eq!(over["text"].as_str().unwrap().len(), PREVIEW_MAX);
        let r = over["text_ref"].as_str().unwrap();
        assert_eq!(store.get(r).unwrap().unwrap(), original.as_bytes());
    }

    #[test]
    fn largest_string_moves_first_and_small_ones_stay() {
        let store = MemoryBlobStore::new();
        let mut v = json!({
            "a": "a".repeat(3000),
            "b": "b".repeat(2000),
            "c": "c".repeat(100),
        });
        assert_eq!(cap_payload(&mut v, &store).unwrap(), 1);
        assert!(v.get("a_ref").is_some());
        assert!(v.get("b_ref").is_none());
        assert_eq!(v["b"].as_str().unwrap().len(), 2000);
        assert_eq!(v["c"].as_str().unwrap().len(), 100);
        assert!(size(&v) <= PAYLOAD_CAP);
    }

    #[test]
    fn preview_truncates_on_char_boundary() {
        let store = MemoryBlobStore::new();
        // 'é' is 2 bytes; an odd prefix byte forces the boundary back by one.
        let s = format!("x{}", "é".repeat(3000));
        let mut v = json!({ "text": s });
        cap_payload(&mut v, &store).unwrap();
        let preview = v["text"].as_str().unwrap();
        assert!(preview.len() <= PREVIEW_MAX);
        assert_eq!(preview.len(), PREVIEW_MAX - 1);
        assert!(s.starts_with(preview));
        assert_eq!(truncate_utf8("héllo", 2), "h");
        assert_eq!(truncate_utf8("héllo", 3), "hé");
        assert_eq!(truncate_utf8("hi", 10), "hi");
    }

    #[test]
    fn nested_ext_strings_are_capped() {
        let store = MemoryBlobStore::new();
        let mut v = json!({
            "text": "short",
            "ext": {"claude-code": {"message": "m".repeat(5000), "msg_id": "keep"}},
        });
        assert_eq!(cap_payload(&mut v, &store).unwrap(), 1);
        let inner = &v["ext"]["claude-code"];
        assert_eq!(inner["message"].as_str().unwrap().len(), PREVIEW_MAX);
        assert!(is_blob_ref(inner["message_ref"].as_str().unwrap()));
        assert_eq!(inner["msg_id"], "keep");
        assert_eq!(v["text"], "short");
    }

    #[test]
    fn summary_fields_fill_schema_refs() {
        let store = MemoryBlobStore::new();
        let mut v = json!({"call_id":"c1","tool":"Bash","input_summary":"i".repeat(5000),
            "input_ref":null,"risky":true});
        cap_payload(&mut v, &store).unwrap();
        assert!(is_blob_ref(v["input_ref"].as_str().unwrap()));
        assert!(v.get("input_summary_ref").is_none());

        // An adapter-provided input_ref is kept; the summary gets its own pointer.
        let own = store.put(b"full input").unwrap();
        let mut v = json!({"call_id":"c1","input_summary":"i".repeat(5000),"input_ref":own});
        cap_payload(&mut v, &store).unwrap();
        assert_eq!(v["input_ref"], json!(own));
        assert!(is_blob_ref(v["input_summary_ref"].as_str().unwrap()));
    }

    #[test]
    fn idempotent() {
        let store = MemoryBlobStore::new();
        let mut v = json!({"text": "t".repeat(10_000), "other": "o".repeat(10_000)});
        let n = cap_payload(&mut v, &store).unwrap();
        assert!(n >= 1);
        let once = v.clone();
        assert_eq!(cap_payload(&mut v, &store).unwrap(), 0);
        assert_eq!(v, once);
    }

    #[test]
    fn identifiers_are_never_moved() {
        let store = MemoryBlobStore::new();
        let mut v = json!({
            "attn_id": "a".repeat(5000),
            "call_id": "c".repeat(5000),
            "session_id": "s".repeat(5000),
            "thread": "t".repeat(5000),
            "id": "i".repeat(5000),
        });
        let before = v.clone();
        assert_eq!(cap_payload(&mut v, &store).unwrap(), 0);
        assert_eq!(v, before);
    }

    #[test]
    fn oversized_containers_move_wholesale() {
        let store = MemoryBlobStore::new();
        let options: Vec<String> = (0..500).map(|i| format!("option-{i}")).collect();
        let mut v = json!({"attn_id":"a1","summary":"pick one","options":options,
            "ext":{"x":{"list":(0..10).collect::<Vec<_>>()}}});
        let original = v.clone();
        assert_eq!(cap_payload(&mut v, &store).unwrap(), 1);
        assert_eq!(v["options"], json!([]));
        assert!(is_blob_ref(v["options_ref"].as_str().unwrap()));
        assert_eq!(v["attn_id"], "a1");
        assert!(size(&v) <= PAYLOAD_CAP);
        // Small containers stay.
        assert_eq!(v["ext"], original["ext"]);

        let mut objs = json!({"session_id":"s","ext":{"rt":{"blocks":{"k": "v".repeat(400),
            "k2": "v".repeat(400), "k3": "v".repeat(400), "k4": "v".repeat(400),
            "k5": "v".repeat(400), "k6": "v".repeat(400), "k7": "v".repeat(400),
            "k8": "v".repeat(400), "k9": "v".repeat(400), "k10": "v".repeat(400),
            "k11": "v".repeat(400)}}}});
        assert!(size(&objs) > PAYLOAD_CAP);
        cap_payload(&mut objs, &store).unwrap();
        assert!(size(&objs) <= PAYLOAD_CAP);
        assert_eq!(objs["ext"]["rt"]["blocks"], json!({}));
        assert!(is_blob_ref(
            objs["ext"]["rt"]["blocks_ref"].as_str().unwrap()
        ));
        assert_eq!(objs["session_id"], "s");
    }

    #[test]
    fn hydrate_restores_capped_payload() {
        let store = MemoryBlobStore::new();
        let options: Vec<String> = (0..500).map(|i| format!("option-{i}")).collect();
        let original = json!({
            "text": "t".repeat(9000),
            "input_summary": "é".repeat(3000),
            "input_ref": null,
            "options": options,
            "ext": {"rt": {"message": "m".repeat(6000)}},
        });
        let mut v = original.clone();
        assert!(cap_payload(&mut v, &store).unwrap() >= 3);
        assert!(size(&v) <= PAYLOAD_CAP);
        let n = hydrate(&mut v, &store).unwrap();
        assert!(n >= 3);
        assert_eq!(v, original);
    }

    #[test]
    fn hydrate_leaves_runtime_pointers_alone() {
        let store = MemoryBlobStore::new();
        let own = store.put(b"{\"cmd\":\"ls -la\"}").unwrap();
        let mut v = json!({"input_summary":"ls","input_ref":own});
        let before = v.clone();
        assert_eq!(hydrate(&mut v, &store).unwrap(), 0);
        assert_eq!(v, before);
    }

    #[test]
    fn hydrate_missing_blob_errors() {
        let store = MemoryBlobStore::new();
        let mut v = json!({"text":"abc","text_ref":blob_ref(b"abcdef"),"_capped":["text"]});
        assert!(hydrate(&mut v, &store).is_err());
    }

    #[test]
    fn hydrate_ignores_adapter_pointer_whose_summary_is_a_prefix() {
        let store = MemoryBlobStore::new();
        // The adapter stored the full output and kept a prefix of it as the summary.
        let own = store.put(b"ok: 3 files changed\n...").unwrap();
        let mut v = json!({"call_id":"c1","ok":true,"output_summary":"ok","output_ref":own});
        let before = v.clone();
        assert_eq!(hydrate(&mut v, &store).unwrap(), 0);
        assert_eq!(v, before);
    }

    #[test]
    fn hydrate_restores_capped_summary_next_to_adapter_pointer() {
        let store = MemoryBlobStore::new();
        let long = "o".repeat(6000);
        let own = store.put(long.as_bytes()).unwrap();
        // Summary capped while the adapter owns output_ref: the cap uses output_summary_ref,
        // and hydrate must restore from it without touching output_ref.
        let original = json!({"call_id":"c1","output_summary":long,"output_ref":own});
        let mut v = original.clone();
        assert_eq!(cap_payload(&mut v, &store).unwrap(), 1);
        assert_eq!(v["output_ref"], json!(own));
        assert!(is_blob_ref(v["output_summary_ref"].as_str().unwrap()));
        assert_eq!(v[CAPPED_KEY], json!(["output_summary"]));
        assert_eq!(hydrate(&mut v, &store).unwrap(), 1);
        assert_eq!(v, original);

        // Cap-generated schema pointer: restored and reset to null.
        let original = json!({"call_id":"c1","output_summary":long,"output_ref":null});
        let mut v = original.clone();
        cap_payload(&mut v, &store).unwrap();
        assert!(is_blob_ref(v["output_ref"].as_str().unwrap()));
        assert!(v.get("output_summary_ref").is_none());
        assert_eq!(hydrate(&mut v, &store).unwrap(), 1);
        assert_eq!(v, original);
    }

    #[test]
    fn capping_never_grows_a_payload() {
        let store = MemoryBlobStore::new();
        let strings8 = |n: usize| -> Value {
            (0..8)
                .map(|i| (format!("k{i}"), json!("v".repeat(n))))
                .collect::<Map<String, Value>>()
                .into()
        };
        let options: Vec<String> = (0..500).map(|i| format!("option-{i}")).collect();
        let payloads = vec![
            text_payload(PAYLOAD_CAP + 1),
            text_payload(PAYLOAD_CAP + 600),
            strings8(520),
            strings8(530),
            strings8(700),
            json!({"ext": {"rt": strings8(520)}}),
            json!({"a": "é".repeat(400), "b": "\"".repeat(2000), "c": "x".repeat(600)}),
            json!({"options": options, "summary": "s".repeat(600)}),
            json!({"list": (0..3000).collect::<Vec<_>>()}),
        ];
        for p in payloads {
            let mut v = p.clone();
            let before = size(&v);
            cap_payload(&mut v, &store).unwrap();
            let after = size(&v);
            assert!(after <= before, "grew {before} -> {after}: {p}");
            let mut h = v.clone();
            hydrate(&mut h, &store).unwrap();
            assert_eq!(h, p);
        }
    }

    #[test]
    fn strings_just_over_preview_are_not_moved() {
        let store = MemoryBlobStore::new();
        // 8 strings of 520 bytes: each move would save 8 bytes of text but cost a pointer and a
        // marker entry, so none moves. At top level there is no container to move either.
        let top: Value = (0..8)
            .map(|i| (format!("k{i}"), json!("v".repeat(520))))
            .collect::<Map<String, Value>>()
            .into();
        assert!(size(&top) > PAYLOAD_CAP);
        let mut v = top.clone();
        assert_eq!(cap_payload(&mut v, &store).unwrap(), 0);
        assert_eq!(v, top);
        assert!(store.is_empty());

        // Nested, the container pass moves the enclosing object instead.
        let mut v = json!({"ext": {"rt": top.clone()}});
        assert_eq!(cap_payload(&mut v, &store).unwrap(), 1);
        assert!(size(&v) <= PAYLOAD_CAP);
        assert_eq!(v["ext"]["rt"], json!({}));
        hydrate(&mut v, &store).unwrap();
        assert_eq!(v["ext"]["rt"], top);
    }

    fn resolved_with_message(message: String, ext: Option<JsonMap>) -> EventBody {
        EventBody::AttentionResolved(AttentionResolved {
            attn_id: "a1".into(),
            by: ResolvedBy::User,
            answer: Some(Answer {
                decision: "allow".into(),
                message: Some(message),
                ..Default::default()
            }),
            ext,
            ..Default::default()
        })
    }

    /// Caps `body`, round-trips it through the event JSON, hydrates it and checks it matches.
    fn assert_cap_round_trip(body: &EventBody, store: &MemoryBlobStore) -> Value {
        let capped = cap_body(body, store).unwrap();
        let ev = Event::new("t1", 1, Some(1), Source::User("tui".into()), capped);
        let json = serde_json::to_string(&ev).unwrap();
        let back: Event = serde_json::from_str(&json).expect("capped event deserializes");
        let mut data = back.body.data().unwrap();
        assert!(size(&data) <= PAYLOAD_CAP, "{data}");
        let capped_data = data.clone();
        hydrate(&mut data, store).unwrap();
        assert_eq!(data, body.data().unwrap());
        let EventBody::AttentionResolved(r) = back.body else {
            panic!("variant")
        };
        assert_eq!(r.answer.unwrap().decision, "allow");
        capped_data
    }

    #[test]
    fn attention_resolved_long_answer_round_trips() {
        let store = MemoryBlobStore::new();
        let body = resolved_with_message("m".repeat(6000), None);
        let capped = assert_cap_round_trip(&body, &store);
        let answer = &capped["answer"];
        assert_eq!(answer["message"].as_str().unwrap().len(), PREVIEW_MAX);
        assert!(is_blob_ref(answer["message_ref"].as_str().unwrap()));
        assert_eq!(answer[CAPPED_KEY], json!(["message"]));
    }

    #[test]
    fn attention_resolved_container_pass_never_moves_answer() {
        let store = MemoryBlobStore::new();
        // After the string pass the payload is still slightly over the cap because of ext; the
        // answer object would be the smallest sufficient container, but it must stay in place.
        let rt: Map<String, Value> = (0..9)
            .map(|i| (format!("k{i}"), json!("v".repeat(400))))
            .collect();
        let mut ext = JsonMap::new();
        ext.insert("rt".into(), Value::Object(rt));
        let body = resolved_with_message("m".repeat(6000), Some(ext));
        let capped = assert_cap_round_trip(&body, &store);
        assert!(capped["answer"]["decision"] == "allow");
        assert!(is_blob_ref(
            capped["answer"]["message_ref"].as_str().unwrap()
        ));
        assert_eq!(capped["ext"]["rt"], json!({}), "ext.rt moved instead");

        // Without the protection the answer would have been picked: check the premise.
        let mut after_strings = body.data().unwrap();
        let answer = after_strings["answer"].as_object_mut().unwrap();
        move_field(answer, "message", &MemoryBlobStore::new()).unwrap();
        let excess = size(&after_strings) - PAYLOAD_CAP;
        let top = after_strings.as_object().unwrap();
        let answer_saving = savings(top, "answer", &top["answer"], CandKind::Container).unwrap();
        let rt_saving = {
            let ext = top["ext"].as_object().unwrap();
            savings(ext, "rt", &ext["rt"], CandKind::Container).unwrap()
        };
        assert!(answer_saving >= excess, "answer alone would have sufficed");
        assert!(
            answer_saving < rt_saving,
            "and it is the smaller sufficient container"
        );
    }

    #[test]
    fn capped_event_round_trips_with_refs() {
        let store = MemoryBlobStore::new();
        let body = EventBody::AgentText(AgentText {
            text: "z".repeat(20_000),
            is_final: true,
            ..Default::default()
        });
        let capped = cap_body(&body, &store).unwrap();
        let ev = Event::new("t1", 1, Some(1), Source::Adapter("fake".into()), capped);
        let json = serde_json::to_string(&ev).unwrap();
        let back: Event = serde_json::from_str(&json).unwrap();
        let again = serde_json::to_value(&back).unwrap();
        let r = again["data"]["text_ref"].as_str().unwrap();
        assert!(is_blob_ref(r));
        assert!(serde_json::to_vec(&again["data"]).unwrap().len() <= PAYLOAD_CAP);
        let mut data = again["data"].clone();
        hydrate(&mut data, &store).unwrap();
        assert_eq!(data, body.data().unwrap());

        // Tool calls fill the schema input_ref field on the typed struct.
        let call = EventBody::AgentToolCall(AgentToolCall {
            call_id: "c".into(),
            tool: "Write".into(),
            input_summary: "w".repeat(8000),
            ..Default::default()
        });
        let EventBody::AgentToolCall(c) = cap_body(&call, &store).unwrap() else {
            panic!("variant")
        };
        assert!(is_blob_ref(c.input_ref.as_deref().unwrap()));
        assert_eq!(c.input_summary.len(), PREVIEW_MAX);
        assert_eq!(c.call_id, "c");
    }

    #[test]
    fn small_bodies_are_untouched() {
        let store = MemoryBlobStore::new();
        let body = EventBody::AgentText(AgentText {
            text: "hi".into(),
            ..Default::default()
        });
        assert_eq!(cap_body(&body, &store).unwrap(), body);
        assert!(store.is_empty());
    }

    #[test]
    fn cap_body_refuses_irreducible_payloads() {
        let store = MemoryBlobStore::new();
        // A protected identifier is never moved.
        let body = EventBody::AttentionResolved(AttentionResolved {
            attn_id: "a".repeat(5000),
            by: ResolvedBy::User,
            ..Default::default()
        });
        match cap_body(&body, &store) {
            Err(CapError::TooLarge { kind, size }) => {
                assert_eq!(kind, "attention.resolved");
                assert!(size > PAYLOAD_CAP);
            }
            other => panic!("expected TooLarge, got {other:?}"),
        }
        // Many short top-level fields: nothing is worth moving.
        let mut extra = JsonMap::new();
        for i in 0..400 {
            extra.insert(format!("k{i}"), json!("v"));
        }
        let body = EventBody::AttentionResolved(AttentionResolved {
            attn_id: "a1".into(),
            by: ResolvedBy::User,
            answer: Some(Answer {
                decision: "allow".into(),
                extra,
                ..Default::default()
            }),
            ..Default::default()
        });
        assert!(matches!(
            cap_body(&body, &store),
            Err(CapError::TooLarge { .. })
        ));
        // Right at the cap is fine.
        let body = EventBody::AgentText(AgentText {
            text: "z".repeat(20_000),
            ..Default::default()
        });
        let capped = cap_body(&body, &store).unwrap();
        assert!(size(&capped.data().unwrap()) <= PAYLOAD_CAP);
    }

    #[test]
    fn producer_capping_metadata_is_stripped() {
        let mut v = json!({
            "session_id": "session-",
            "session_id_ref": blob_ref(b"session-other"),
            "_capped": ["session_id"],
            "input_summary": "x",
            "input_ref": "sha256:00",
            "output_summary_ref": "sha256:11",
            "output_summary": "y",
            "base_ref": "main",
            "ext": {"a": {"text": "t", "text_ref": "sha256:22", "_capped": ["text"]}},
            "list": [{"_capped": []}]
        });
        assert_eq!(strip_capping_metadata(&mut v), 6);
        assert_eq!(
            v,
            json!({
                "session_id": "session-",
                "input_summary": "x",
                "input_ref": "sha256:00",
                "output_summary": "y",
                "base_ref": "main",
                "ext": {"a": {"text": "t"}},
                "list": [{}]
            })
        );
        assert!(!has_capped(&v));
        // Nothing to strip: nothing changes.
        let mut clean = v.clone();
        assert_eq!(strip_capping_metadata(&mut clean), 0);
        assert_eq!(clean, v);
    }

    #[test]
    fn has_capped_finds_nested_markers() {
        assert!(!has_capped(&json!({"a": [1, {"b": "c"}]})));
        assert!(has_capped(&json!({"a": [1, {"_capped": ["b"]}]})));
        let store = MemoryBlobStore::new();
        let mut v = json!({"options": (0..500).map(|i| format!("option-{i}")).collect::<Vec<_>>()});
        cap_payload(&mut v, &store).unwrap();
        assert!(has_capped(&v));
        hydrate(&mut v, &store).unwrap();
        assert!(!has_capped(&v));
    }

    /// `{"ext": {"rt": {message (6000 bytes), k0..k8 (400 bytes each)}}}`: the string pass moves
    /// `message`, the payload is still over the cap, so the container pass moves `rt`, whose
    /// blob now holds `message`'s pointer.
    fn moved_then_container_moved() -> Value {
        let mut rt: Map<String, Value> = (0..9)
            .map(|i| (format!("k{i}"), json!("v".repeat(400))))
            .collect();
        rt.insert("message".into(), json!("m".repeat(6000)));
        json!({"text": "hi", "ext": {"rt": rt}})
    }

    #[tokio::test]
    async fn hydrate_deep_follows_refs_inside_moved_containers() {
        let store = std::sync::Arc::new(MemoryBlobStore::new());
        let original = moved_then_container_moved();
        let mut v = original.clone();
        assert_eq!(cap_payload(&mut v, store.as_ref()).unwrap(), 2);
        assert!(size(&v) <= PAYLOAD_CAP);
        assert_eq!(v["ext"]["rt"], json!({}));
        // Only the container's pointer is visible in the capped payload.
        let mut visible = Vec::new();
        pending_refs(&v, &mut visible);
        assert_eq!(visible.len(), 1);
        assert_eq!(store.len(), 2);

        let fetched = std::sync::Arc::new(Mutex::new(Vec::<String>::new()));
        let mut fetch = {
            let (store, fetched) = (store.clone(), fetched.clone());
            move |r: String| {
                let (store, fetched) = (store.clone(), fetched.clone());
                async move {
                    fetched.lock().unwrap().push(r.clone());
                    store.get(&r)
                }
            }
        };
        let mut h = v.clone();
        assert_eq!(hydrate_deep(&mut h, &mut fetch).await.unwrap(), 2);
        assert_eq!(h, original);
        assert_eq!(fetched.lock().unwrap().len(), 2, "each blob fetched once");
        assert!(!has_capped(&h));

        // A missing inner blob is an error, not a silent preview.
        let partial = std::sync::Arc::new(MemoryBlobStore::new());
        partial
            .put(&store.get(&visible[0]).unwrap().unwrap())
            .unwrap();
        let mut fetch = move |r: String| {
            let partial = partial.clone();
            async move { partial.get(&r) }
        };
        let mut h = v.clone();
        let err = hydrate_deep(&mut h, &mut fetch).await.unwrap_err();
        assert!(err.to_string().contains("missing blob"), "{err}");
        // The synchronous hydrate agrees when it has every blob.
        let mut h = v.clone();
        hydrate(&mut h, store.as_ref()).unwrap();
        assert_eq!(h, original);
    }
}

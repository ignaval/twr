//! Reading aloud from the TUI (`docs/read-aloud.md`): `/read` and Ctrl-R as the hub's
//! `speech.*` calls, and what the TUI shows of the speakers connected to its hub.
//!
//! - **Calls.** `/read` ([`crate::slash::ReadCmd`]) and Ctrl-R are [`Action::Speech`]s
//!   ([`request`], [`control`]), sent on the view's ordered calls like any other action. Their
//!   answers come back with what was asked ([`answer`]), so [`SpeechView::answered`] can say
//!   what was done: no speaker connected (and how to connect one), how many messages are
//!   read, live mode on or off. Nothing of it is ever sent as a message.
//! - **Status.** The IO loop asks `speech.status` every [`crate::SPEECH_EVERY`], of the current
//!   workspace's hub only, one call at a time ([`SpeechView::pending`]); the answer is
//!   [`SpeechView::status`]. A hub from before reading aloud answers `method_not_found`: the
//!   poll stops then ([`SpeechView::unsupported`]), silently, and `/read` says so
//!   ([`PREDATES`]). A `/read` the hub answers turns the poll on again (an upgraded hub).
//! - **What shows.** The status bar's note while a speaker reads, is paused, or has live mode
//!   on ([`SpeechView::note`]); `▶` before the message being read ([`SpeechView::playing_in`],
//!   drawn by [`crate::ui`]); `/read status`'s line ([`SpeechView::listing`]). A speaker's
//!   error is said once, when the poll first reports it ([`SpeechView::answered`]).
//! - **Speakers** are what the hub lists: `twr speak` on another machine, or the TUI's own
//!   in-process speaker (`[speech] local`). Either attaches to the hub on a connection of its
//!   own, so nothing here depends on which one runs, or where.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use serde_json::{json, Value};
use twr_core::speech::{CtlOp, SpeechControlParams, SpeechRequestParams, SpeechStatus};

use crate::app::Action;

/// What `/read` says when the hub has no `speech.*` methods.
pub const PREDATES: &str = "this hub's twr predates reading aloud";

/// What Ctrl-R says when nothing is being read.
pub const NOTHING_READ: &str = "nothing is being read · /read on · /read N";

/// This machine's name, as `twr speak --ssh` would name it (`gethostname`), else `<host>`.
pub fn host() -> String {
    static HOST: OnceLock<String> = OnceLock::new();
    HOST.get_or_init(|| {
        let mut buf = [0u8; 256];
        // The buffer is writable for its whole length; the name is cut at its first NUL (or
        // at the buffer's end, should a long one come without).
        let ok = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } == 0;
        let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        let name = String::from_utf8_lossy(&buf[..end]).trim().to_string();
        let plain = name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_'));
        if ok && !name.is_empty() && plain {
            name
        } else {
            "<host>".into()
        }
    })
    .clone()
}

/// What a call answered with no speaker connected says: how to connect one.
pub fn no_speaker() -> String {
    let host = host();
    format!(
        "no speaker connected to this hub · on the machine you listen on: twr speak --ssh \
         {host} (or twr remote {host})"
    )
}

/// `n` and the noun for it, `one` or `many`.
fn count(n: u64, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// A `speech.request` reading the last `last` messages of `thread` (`None`: the
/// orchestrator's), or the one whose final text has seq `seq`, on `speaker` (`None`: every
/// speaker).
pub fn request(
    thread: Option<String>,
    last: Option<u32>,
    seq: Option<u64>,
    speaker: Option<String>,
) -> Action {
    let params = SpeechRequestParams {
        thread,
        last,
        seq,
        speaker,
    };
    Action::Speech {
        method: "speech.request",
        params: serde_json::to_value(params).unwrap_or_default(),
    }
}

/// A `speech.control` of `op` (with its `value`) on `speaker` (`None`: every speaker). The
/// error, for the status bar, is why the hub would refuse it.
pub fn control(op: CtlOp, value: Option<Value>, speaker: Option<String>) -> Result<Action, String> {
    let params = SpeechControlParams { op, value, speaker };
    if let Some(e) = params.error() {
        return Err(format!("read aloud: {e}"));
    }
    Ok(Action::Speech {
        method: "speech.control",
        params: serde_json::to_value(params).unwrap_or_default(),
    })
}

/// A `speech.*` call's outcome as [`SpeechView::answered`] takes it: what was `asked` and the
/// hub's `result`, or `unsupported` (the hub has no such method), or the `error`.
pub fn answer(asked: Value, answer: &anyhow::Result<Value>) -> Value {
    match answer {
        Ok(r) => json!({ "asked": asked, "result": r }),
        Err(e)
            if e.downcast_ref::<twr_core::proto::RpcError>()
                .is_some_and(|r| r.code == twr_core::proto::codes::METHOD_NOT_FOUND) =>
        {
            json!({ "asked": asked, "unsupported": true })
        }
        Err(e) => json!({ "asked": asked, "error": format!("{e:#}") }),
    }
}

/// What the TUI knows of its hub's speakers.
#[derive(Debug, Clone, Default)]
pub struct SpeechView {
    /// The last `speech.status` answer (`None` before the first, or from a hub without it).
    pub status: Option<SpeechStatus>,
    /// A `speech.status` call is on its way: the next poll waits for it.
    pub pending: bool,
    /// The hub has no `speech.*` methods (it answered `method_not_found`): no more polls
    /// until a `/read` is answered.
    pub unsupported: bool,
    /// The error each speaker reported that was said, by device: said once.
    said: BTreeMap<String, String>,
}

impl SpeechView {
    /// Whether a speaker plays something or has messages waiting: Ctrl-R stops it then.
    pub fn busy(&self) -> bool {
        self.status.as_ref().is_some_and(|s| {
            s.speakers
                .iter()
                .any(|p| p.state.playing.is_some() || p.state.queued > 0)
        })
    }

    /// The seqs (of their final `agent.text`) of the messages of `thread` a speaker reads now.
    pub fn playing_in(&self, thread: &str) -> Vec<u64> {
        self.status
            .iter()
            .flat_map(|s| &s.speakers)
            .filter_map(|p| p.state.playing.as_ref())
            .filter(|p| p.thread == thread)
            .map(|p| p.seq)
            .collect()
    }

    /// The status bar's note: `🔊 reading 2/5 · Ctrl-R stop` while a speaker reads (its place in
    /// the batch `/read N` asked for, when it reads one), `🔊 paused · /read resume` while one
    /// is paused, `read aloud on (<names>)` while the speakers with live mode on wait for the
    /// next message. `plain` (`[tui] colors = "none"`): `[read]` in place of `🔊`.
    pub fn note(&self, plain: bool) -> Option<String> {
        let speakers = &self.status.as_ref()?.speakers;
        let glyph = if plain { "[read]" } else { "🔊" };
        let reads =
            |p: &&twr_core::speech::SpeakerStatus| p.state.playing.is_some() || p.state.queued > 0;
        if let Some(p) = speakers.iter().filter(reads).find(|p| !p.state.paused) {
            return Some(match p.state.batch {
                Some((n, of)) => format!("{glyph} reading {n}/{of} · Ctrl-R stop"),
                None => format!("{glyph} reading · Ctrl-R stop"),
            });
        }
        if speakers.iter().any(|p| p.state.paused) {
            return Some(format!("{glyph} paused · /read resume"));
        }
        let live: Vec<&str> = speakers
            .iter()
            .filter(|p| p.state.live)
            .map(|p| p.name.as_str())
            .collect();
        (!live.is_empty()).then(|| format!("read aloud on ({})", live.join(", ")))
    }

    /// `/read status`: each speaker as `name (engine, voice) · live on · reading 2/5`, as the
    /// last poll had them.
    pub fn listing(&self) -> String {
        if self.unsupported {
            return PREDATES.into();
        }
        let Some(status) = &self.status else {
            return "read aloud: no word from the hub yet; /read status again in a moment".into();
        };
        if status.speakers.is_empty() {
            return no_speaker();
        }
        let rows: Vec<String> = status
            .speakers
            .iter()
            .map(|p| {
                let s = &p.state;
                let engine = match &s.voice {
                    Some(v) => format!("{}, {v}", p.engine),
                    None => p.engine.clone(),
                };
                let live = if s.live { "live on" } else { "live off" };
                let doing = match (s.paused, s.playing.is_some() || s.queued > 0, s.batch) {
                    (true, _, _) => "paused".to_string(),
                    (false, true, Some((n, of))) => format!("reading {n}/{of}"),
                    (false, true, None) => "reading".to_string(),
                    (false, false, _) => "idle".to_string(),
                };
                let mut row = format!("{} ({engine}) · {live} · {doing}", p.name);
                if let Some(e) = &s.error {
                    row.push_str(&format!(" · {e}"));
                }
                row
            })
            .collect();
        let n = u64::try_from(rows.len()).unwrap_or(u64::MAX);
        format!("{}: {}", count(n, "speaker", "speakers"), rows.join("; "))
    }

    /// Takes the outcome of a `speech.*` call ([`answer`]); what the status bar says of it, if
    /// anything. A poll (`speech.status`) says only a speaker's new error; a poll that failed
    /// (the hub went away, restarted) shows nothing as read until one succeeds: what the last
    /// one said may be gone, and `Ctrl-R` then isn't taken from the views under it.
    pub fn answered(&mut self, method: &str, v: &Value) -> Option<String> {
        let unsupported = v["unsupported"].as_bool() == Some(true);
        if method == "speech.status" {
            self.pending = false;
            if unsupported {
                self.unsupported = true;
                self.status = None;
                return None;
            }
            let Ok(status) = serde_json::from_value::<SpeechStatus>(v["result"].clone()) else {
                self.status = None;
                return None;
            };
            self.unsupported = false;
            self.status = Some(status);
            return self.new_errors();
        }
        if unsupported {
            self.unsupported = true;
            self.status = None;
            return Some(PREDATES.into());
        }
        if let Some(e) = v["error"].as_str() {
            return Some(format!("read aloud: {e}"));
        }
        self.unsupported = false;
        let (asked, r) = (&v["asked"], &v["result"]);
        let speakers = r["speakers"].as_u64().unwrap_or(0);
        if speakers == 0 {
            return Some(no_speaker());
        }
        let on = format!("({})", count(speakers, "speaker", "speakers"));
        if method == "speech.request" {
            return Some(match r["count"].as_u64().unwrap_or(0) {
                0 => "nothing to read: no message of that thread yet".into(),
                n => format!("reading {}", count(n, "message", "messages")),
            });
        }
        Some(match asked["op"].as_str() {
            Some("live") if asked["value"].as_bool() == Some(true) => {
                format!("read aloud on {on}: new messages are read as they come")
            }
            Some("live") => format!("read aloud off {on}"),
            Some("stop") => {
                self.stopped(asked["speaker"].as_str());
                format!("stopped reading {on}")
            }
            Some("pause") => format!("paused {on} · /read resume"),
            Some("resume") => format!("reading again {on}"),
            Some("skip") => format!("skipped to the next message {on}"),
            Some("voice") => format!("voice {} {on}", asked["value"].as_str().unwrap_or("?")),
            Some("rate") => format!(
                "rate {} words per minute {on}",
                asked["value"].as_u64().unwrap_or(0)
            ),
            _ => format!("done {on}"),
        })
    }

    /// A stop the hub sent (to `speaker`, or every one): nothing reads any more, until the
    /// next poll says otherwise.
    fn stopped(&mut self, speaker: Option<&str>) {
        let Some(status) = self.status.as_mut() else {
            return;
        };
        for p in &mut status.speakers {
            if speaker.is_none_or(|s| s == p.name || s == p.device) {
                p.state.playing = None;
                p.state.queued = 0;
                p.state.batch = None;
            }
        }
    }

    /// The speakers' errors not said yet, said once: `read aloud: laptop: no say`. One that
    /// cleared may be said again when it comes back.
    fn new_errors(&mut self) -> Option<String> {
        let speakers = self.status.as_ref().map_or(&[][..], |s| &s.speakers[..]);
        let now: BTreeMap<String, String> = speakers
            .iter()
            .filter_map(|p| Some((p.device.clone(), p.state.error.clone()?)))
            .collect();
        let new: Vec<String> = speakers
            .iter()
            .filter_map(|p| {
                let e = p.state.error.as_ref()?;
                (self.said.get(&p.device) != Some(e)).then(|| format!("{}: {e}", p.name))
            })
            .collect();
        self.said = now;
        (!new.is_empty()).then(|| format!("read aloud: {}", new.join("; ")))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use twr_core::speech::{Playing, SpeakerStatus, SpeechStateParams};

    /// A speaker named `name` in `state`.
    pub(crate) fn speaker(name: &str, state: SpeechStateParams) -> SpeakerStatus {
        SpeakerStatus {
            device: format!("dev-{name}"),
            name: name.into(),
            engine: "spd".into(),
            since: "2026-10-27T10:00:00Z".into(),
            state,
        }
    }

    /// A poll's answer listing `speakers`.
    pub(crate) fn polled(speakers: Vec<SpeakerStatus>) -> Value {
        answer(
            json!({}),
            &Ok(serde_json::to_value(SpeechStatus { speakers }).unwrap()),
        )
    }

    /// What it plays: chunk 1 of 3 of `seq` in `thread`.
    pub(crate) fn playing(thread: &str, seq: u64) -> Option<Playing> {
        Some(Playing {
            thread: thread.into(),
            seq,
            chunk: 1,
            of: 3,
        })
    }

    #[test]
    fn the_note_says_reading_paused_or_live() {
        let mut v = SpeechView::default();
        assert_eq!(v.note(false), None, "nothing polled yet");
        assert_eq!(v.answered("speech.status", &polled(Vec::new())), None);
        assert_eq!(v.note(false), None, "no speaker");
        // Live mode on, idle: its name.
        let live = SpeechStateParams {
            live: true,
            ..Default::default()
        };
        v.answered(
            "speech.status",
            &polled(vec![speaker("laptop", live.clone())]),
        );
        assert_eq!(v.note(false).as_deref(), Some("read aloud on (laptop)"));
        assert_eq!(v.note(true).as_deref(), Some("read aloud on (laptop)"));
        assert!(!v.busy());
        // Reading a batch of /read N: its place in it.
        let batch = SpeechStateParams {
            playing: playing("t-orch", 7),
            queued: 3,
            batch: Some((2, 5)),
            ..live.clone()
        };
        v.answered("speech.status", &polled(vec![speaker("laptop", batch)]));
        assert_eq!(
            v.note(false).as_deref(),
            Some("🔊 reading 2/5 · Ctrl-R stop")
        );
        assert_eq!(
            v.note(true).as_deref(),
            Some("[read] reading 2/5 · Ctrl-R stop")
        );
        assert!(v.busy());
        assert_eq!(v.playing_in("t-orch"), [7]);
        assert!(v.playing_in("t-api").is_empty());
        // A live message: no counter.
        let one = SpeechStateParams {
            playing: playing("t-orch", 9),
            ..live.clone()
        };
        v.answered("speech.status", &polled(vec![speaker("laptop", one)]));
        assert_eq!(v.note(false).as_deref(), Some("🔊 reading · Ctrl-R stop"));
        // Queued, between two messages: still reading.
        let queued = SpeechStateParams {
            queued: 2,
            ..Default::default()
        };
        v.answered("speech.status", &polled(vec![speaker("laptop", queued)]));
        assert_eq!(v.note(false).as_deref(), Some("🔊 reading · Ctrl-R stop"));
        // Paused.
        let paused = SpeechStateParams {
            playing: playing("t-orch", 9),
            paused: true,
            ..Default::default()
        };
        v.answered(
            "speech.status",
            &polled(vec![speaker("laptop", paused.clone())]),
        );
        assert_eq!(v.note(false).as_deref(), Some("🔊 paused · /read resume"));
        assert_eq!(
            v.note(true).as_deref(),
            Some("[read] paused · /read resume")
        );
        assert!(v.busy(), "Ctrl-R stops a paused message too");
        // One speaker paused, another reading: reading.
        let reading = SpeechStateParams {
            playing: playing("t-orch", 9),
            ..Default::default()
        };
        v.answered(
            "speech.status",
            &polled(vec![speaker("laptop", paused), speaker("phone", reading)]),
        );
        assert_eq!(v.note(false).as_deref(), Some("🔊 reading · Ctrl-R stop"));
        // Two live speakers.
        v.answered(
            "speech.status",
            &polled(vec![
                speaker("laptop", live.clone()),
                speaker("phone", live),
            ]),
        );
        assert_eq!(
            v.note(false).as_deref(),
            Some("read aloud on (laptop, phone)")
        );
    }

    #[test]
    fn answers_say_what_was_done() {
        let mut v = SpeechView::default();
        let req = |count: u32, speakers: u32| {
            answer(
                json!({"last": 3}),
                &Ok(json!({"count": count, "speakers": speakers, "thread": "t-orch"})),
            )
        };
        let host = host();
        let none = format!(
            "no speaker connected to this hub · on the machine you listen on: twr speak --ssh \
             {host} (or twr remote {host})"
        );
        assert_eq!(v.answered("speech.request", &req(0, 0)), Some(none.clone()));
        assert_eq!(
            v.answered("speech.request", &req(3, 1)).as_deref(),
            Some("reading 3 messages")
        );
        assert_eq!(
            v.answered("speech.request", &req(1, 2)).as_deref(),
            Some("reading 1 message")
        );
        assert!(v
            .answered("speech.request", &req(0, 1))
            .unwrap()
            .starts_with("nothing to read"));
        let ctl = |asked: Value, speakers: u32| answer(asked, &Ok(json!({"speakers": speakers})));
        assert_eq!(
            v.answered(
                "speech.control",
                &ctl(json!({"op": "live", "value": true}), 0)
            ),
            Some(none)
        );
        assert_eq!(
            v.answered(
                "speech.control",
                &ctl(json!({"op": "live", "value": true}), 1)
            )
            .as_deref(),
            Some("read aloud on (1 speaker): new messages are read as they come")
        );
        assert_eq!(
            v.answered(
                "speech.control",
                &ctl(json!({"op": "live", "value": false}), 2)
            )
            .as_deref(),
            Some("read aloud off (2 speakers)")
        );
        assert_eq!(
            v.answered(
                "speech.control",
                &ctl(json!({"op": "rate", "value": 220}), 1)
            )
            .as_deref(),
            Some("rate 220 words per minute (1 speaker)")
        );
        // An error: said.
        let failed = answer(json!({}), &Err(anyhow::anyhow!("not_found: no thread x")));
        assert_eq!(
            v.answered("speech.request", &failed).as_deref(),
            Some("read aloud: not_found: no thread x")
        );
    }

    /// A stop answered clears what the last poll said is read, so Ctrl-R twice does not stop
    /// twice; only on the speaker it went to.
    #[test]
    fn a_stop_clears_what_reads() {
        let mut v = SpeechView::default();
        let reading = SpeechStateParams {
            playing: playing("t-orch", 9),
            queued: 2,
            batch: Some((1, 3)),
            ..Default::default()
        };
        v.answered(
            "speech.status",
            &polled(vec![
                speaker("laptop", reading.clone()),
                speaker("phone", reading),
            ]),
        );
        let stop = |on: Option<&str>| {
            answer(
                json!({"op": "stop", "speaker": on}),
                &Ok(json!({"speakers": 1})),
            )
        };
        v.answered("speech.control", &stop(Some("phone")));
        assert!(v.busy(), "the laptop still reads");
        assert_eq!(v.playing_in("t-orch"), [9]);
        v.answered("speech.control", &stop(None));
        assert!(!v.busy());
        assert_eq!(v.note(false), None);
    }

    /// A hub from before reading aloud: its poll says nothing and stops; `/read` says so; a
    /// `/read` it answers later turns the poll on again.
    #[test]
    fn an_older_hub_is_said_once_on_read() {
        let mut v = SpeechView {
            pending: true,
            ..Default::default()
        };
        let old = |asked: Value| {
            let e = anyhow::Error::new(twr_core::proto::RpcError::new(
                twr_core::proto::codes::METHOD_NOT_FOUND,
                "speech.status",
            ));
            answer(asked, &Err(e))
        };
        assert_eq!(v.answered("speech.status", &old(json!({}))), None);
        assert!(v.unsupported && !v.pending);
        assert_eq!(v.listing(), PREDATES);
        assert_eq!(
            v.answered("speech.request", &old(json!({"last": 1})))
                .as_deref(),
            Some(PREDATES)
        );
        assert!(v.unsupported);
        v.answered(
            "speech.control",
            &answer(json!({"op": "stop"}), &Ok(json!({"speakers": 1}))),
        );
        assert!(!v.unsupported, "an upgraded hub");
    }

    /// `/read status` lists each speaker; an error is said once by the poll, and again only
    /// once it cleared and came back.
    #[test]
    fn status_lists_speakers_and_errors_are_said_once() {
        let mut v = SpeechView::default();
        assert!(v.listing().contains("no word from the hub yet"));
        v.answered("speech.status", &polled(Vec::new()));
        assert_eq!(v.listing(), no_speaker());
        let laptop = SpeechStateParams {
            live: true,
            voice: Some("Samantha".into()),
            playing: playing("t-orch", 3),
            batch: Some((2, 5)),
            ..Default::default()
        };
        let broken = SpeechStateParams {
            error: Some("spd-say not found".into()),
            ..Default::default()
        };
        let both = polled(vec![speaker("laptop", laptop), speaker("mini", broken)]);
        assert_eq!(
            v.answered("speech.status", &both).as_deref(),
            Some("read aloud: mini: spd-say not found")
        );
        assert_eq!(
            v.listing(),
            "2 speakers: laptop (spd, Samantha) · live on · reading 2/5; mini (spd) · live off \
             · idle · spd-say not found"
        );
        assert_eq!(v.answered("speech.status", &both), None, "said once");
        let fine = polled(vec![speaker("mini", SpeechStateParams::default())]);
        assert_eq!(v.answered("speech.status", &fine), None);
        assert!(v.answered("speech.status", &both).is_some(), "it came back");
        // A failed poll shows nothing as read: what the last one said may be gone.
        let failed = answer(json!({}), &Err(anyhow::anyhow!("broken pipe")));
        assert_eq!(v.answered("speech.status", &failed), None);
        assert!(v.status.is_none());
        assert!(!v.busy());
    }

    #[test]
    fn controls_are_checked_before_they_go() {
        assert_eq!(
            control(CtlOp::Live, Some(Value::Bool(true)), Some("laptop".into())),
            Ok(Action::Speech {
                method: "speech.control",
                params: json!({"op": "live", "value": true, "speaker": "laptop"}),
            })
        );
        assert_eq!(
            control(CtlOp::Stop, None, None),
            Ok(Action::Speech {
                method: "speech.control",
                params: json!({"op": "stop"}),
            })
        );
        assert!(control(CtlOp::Voice, Some("bad\u{7}".into()), None).is_err());
        assert_eq!(
            request(Some("t-api".into()), Some(3), None, None),
            Action::Speech {
                method: "speech.request",
                params: json!({"thread": "t-api", "last": 3}),
            }
        );
        assert!(!host().is_empty());
    }
}

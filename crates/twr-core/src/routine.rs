//! Classification shared by conversation renderers. Attention requests are never routine.
use crate::event::{
    routine_of, JsonMap, ROUTINE_AUTO_WAKE, ROUTINE_BACKGROUND_DONE, ROUTINE_INTERIM, ROUTINE_KEY,
    ROUTINE_LOOP,
};

/// What kind of routine update a hub update is in the conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Routine {
    /// An interim report: the turn yielded with background tasks still running.
    Interim,
    /// `api background work finished.`
    BackgroundDone,
    /// The hub woke the worker after its background work.
    AutoWake,
    /// A stall that resolved by itself (only when the conversation is drawn: the caller).
    Stall,
    /// A loop's round, phase change or control, or a loop turn's report ([`ROUTINE_LOOP`]).
    Loop,
    /// A kind a newer hub marks routine that this TUI doesn't know.
    Other,
}

impl Routine {
    /// The kind of routine update `text` with `ext` is, if it is one: as the hub marks it
    /// (`ext.twr.routine`), else, for an update of an older hub (no mark), by its text. The
    /// caller leaves out the updates of an attention item and the acknowledgments of a
    /// message of the user's: never routine.
    pub fn of(ext: &Option<JsonMap>, text: &str) -> Option<Routine> {
        if let Some(kind) = routine_of(ext) {
            return Some(match kind {
                ROUTINE_INTERIM => Routine::Interim,
                ROUTINE_BACKGROUND_DONE => Routine::BackgroundDone,
                ROUTINE_AUTO_WAKE => Routine::AutoWake,
                ROUTINE_LOOP => Routine::Loop,
                _ => Routine::Other,
            });
        }
        let marked = ext
            .as_ref()
            .and_then(|e| e.get("twr"))
            .is_some_and(|t| t.get(ROUTINE_KEY).is_some());
        if marked {
            return None;
        }
        // An older hub's words (`hub::updates`): `api yielded with 2 background task(s)
        // (success): …`, `api background work finished.`, `api's background work finished
        // and no turn started after it: twr woke it …`. An older hub said a failed turn's
        // report so too, graded `gave_up` (as every failed turn is), and its choice not to
        // wake a worker (`…; twr did not wake it (…). Send it a message to continue.`), which
        // asks the user to act: never routine.
        let word = |n: &str| !n.is_empty() && !n.contains(' ');
        if let Some((name, rest)) = text.split_once(" yielded with ") {
            let grade = rest
                .split_once("): ")
                .and_then(|(head, _)| head.rsplit_once(" ("))
                .map(|(_, grade)| grade);
            if word(name)
                && rest.starts_with(|c: char| c.is_ascii_digit())
                && rest.contains(" background task")
                && grade != Some("gave_up")
            {
                return Some(Routine::Interim);
            }
        }
        if text
            .strip_suffix(" background work finished.")
            .is_some_and(word)
        {
            return Some(Routine::BackgroundDone);
        }
        match text.split_once("'s background work finished and no turn started after it") {
            Some((name, rest)) if word(name) && !rest.contains("did not wake") => {
                Some(Routine::AutoWake)
            }
            _ => None,
        }
    }
}

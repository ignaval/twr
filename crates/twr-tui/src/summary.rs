//! The orchestrator conversation's routine updates, folded, and their summaries.
//!
//! - **Routine updates** ([`Routine`]): the hub's updates that tell of a worker's work going
//!   on, not of something to decide: an interim report (its turn yielded with background tasks
//!   still running), `api background work finished.`, and the hub waking the worker after its
//!   background work. The hub marks them (`ext.twr.routine`, [`twr_core::event::ROUTINE_KEY`]);
//!   one an older hub wrote is told by its text. When the conversation is drawn, a stall that
//!   resolved by itself is one too: the control stream said its item closed because its thread
//!   went on working (`ext.twr.went_on`, [`twr_core::event::WENT_ON_KEY`];
//!   [`Summaries::went_on`]), not merely that it is missing from the stack (the conversation
//!   and control streams come apart: the update may draw before its raise). One closed
//!   otherwise (its thread stopped, crashed or was interrupted, its turn failed, a hung-turn
//!   card took its place, the hub's startup closed it, or an older hub that doesn't say) stays
//!   as it is. Never routine: a permission or a question (a request's box), a review, an
//!   error, a hung turn, a confirm or delete card, a final report, a failure (a failed turn's
//!   report too, though its tasks ran on: the hub marks it `routine: null`, and an older hub's
//!   words of one are graded `gave_up`), the hub's choice not to wake a worker (it asks the
//!   user to send it a message: marked `null` too, and an older hub's words of it are told),
//!   the acknowledgment of a message of the user's.
//! - **Runs** ([`runs`]): a thread's routine updates since the last update about it that is not
//!   routine. In the orchestrator conversation a run draws as one row at the place of its newest
//!   update (its other updates draw nothing): `◦ api · <text> (ctrl+o to expand)`, the text
//!   being the run's summary when there is one ([`AppState::summary_text`]), else the newest
//!   update's short form and how many there are ([`Run::plain`]). Ctrl-O, or a click on the row
//!   (its key, `routine:<thread>:<first seq>`, goes into the expanded groups), shows each of its
//!   updates as it is, in its own place. A thread's newest run is its live one; a run an update
//!   that is not routine closed keeps the text it had.
//! - **Summaries** (`[tui] summaries`, `/summaries`): for a live run whose folded row the last
//!   frame drew ([`DrawnRun`], none under an overlay), the TUI asks the hub for a line on where
//!   the thread stands (`summary.thread`, `twr_hub::hub::summary`), off the IO loop on a
//!   connection of its own ([`AppState::take_summaries_due`]): when the run has an update the
//!   last ask didn't, or while the thread works (running, or its background work) and its
//!   activity line changed since; never sooner than `[tui] summary_every` after the last ask
//!   of the thread, nor than the hub's `next_after_secs`; one ask of a thread at a time. A
//!   summary is kept with the newest update it covered (the hub's `covers`; from an older hub,
//!   which names no conversation either, the one asked about; one that covered none, the run
//!   closed before the hub read, is of no run), so a run shows the latest summary of one of
//!   its updates (closed, it keeps it), and never one of another run's; a `NONE` answer is
//!   kept the same way, the run then read as its updates rather than as an older summary.
//!   An update is told by its conversation (the orchestrator's thread, the hub's
//!   `covers_in`: a reset or a switch is a new thread whose seqs start again) and its seq,
//!   in what is kept, in each run's row and in the pace; only the current
//!   orchestrator's runs are asked about (the hub reads its conversation), and an answer of
//!   another conversation than the ask's is no summary of the run (it is due again); one that
//!   covered no update of the conversation it names is of no run, and that run is not asked
//!   about again until it has a newer update ([`Pace::barren`]): the hub reads none of it.
//!   `/summaries off` and quitting drop the asks on their way (their connections close, and
//!   the hub drops them); each ask has its id ([`Ask::id`]), and only the answer of a
//!   thread's ask on its way is taken, never one of an ask dropped before. The first ask ever waits
//!   for the notice of what summaries send ([`crate::firstrun::summary_notice`]) to have been
//!   drawn whole. An ask that brings no summary (the call failed, the hub held off) leaves its
//!   run due again, once the pace allows. Where the hub can't run them (no CLI, or none signed
//!   in) they stop for the run, the rows read as their updates, and one notice ever says so;
//!   `/summaries on` asks again at once about the runs whose ask came to nothing, and its first
//!   asks say so (`retry`): the hub probes the agents' CLIs afresh and tries again what its
//!   calls found unable to run (after the user signed in, say), as for `/suggest on`. A
//!   summary is the text of a routine row only: an item's row is never one.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use ratatui::layout::Rect;
use twr_core::event::{
    routine_of, went_on, AttentionReason, Event, EventBody, JsonMap, ROUTINE_AUTO_WAKE,
    ROUTINE_BACKGROUND_DONE, ROUTINE_INTERIM, ROUTINE_KEY,
};
use twr_core::proto::{SummaryStats, SummaryThreadParams, SummaryThreadResult};
use twr_core::reduce::ThreadStatus;

use crate::app::{AppState, Entry};
use crate::firstrun::Once;

/// What kind of routine update a hub update is ([`crate::app::Entry::HubUpdate`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Routine {
    /// An interim report: the turn yielded with background tasks still running.
    Interim,
    /// `api background work finished.`
    BackgroundDone,
    /// The hub woke the worker after its background work.
    AutoWake,
    /// A stall that resolved by itself (only when the conversation is drawn: [`kind`]).
    Stall,
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

/// The kind of routine update entry `e` is as the conversation is drawn now: the kind it came
/// with ([`Routine::of`]), or, with `state`, [`Routine::Stall`] for a stall (`api is
/// stalled: …`) whose item the control stream said closed because its thread went on working
/// ([`Summaries::went_on`]): not one merely missing from the stack, which the update may reach
/// before its raise does.
pub fn kind(e: &Entry, state: Option<&AppState>) -> Option<Routine> {
    let Entry::HubUpdate {
        text,
        attn_id,
        outcome,
        user_message,
        resolution,
        routine,
        ..
    } = e
    else {
        return None;
    };
    if routine.is_some() {
        return *routine;
    }
    let (state, id) = (state?, attn_id.as_deref()?);
    let stall = text
        .split_once(" is stalled: ")
        .is_some_and(|(name, _)| !name.is_empty() && !name.contains(' '));
    let went_on = state.summary.went_on.contains(id);
    (stall && went_on && outcome.is_none() && user_message.is_none() && !resolution)
        .then_some(Routine::Stall)
}

/// The thread hub update `e` is about: the one the hub names (`about_thread`, an id), else,
/// for an update of an older hub, the first word of its text (`api` of `api's …` too), by
/// its id when `state` knows it ([`AppState::thread_named`], a deleted thread too).
fn thread_of(e: &Entry, state: Option<&AppState>) -> Option<String> {
    let Entry::HubUpdate { about, text, .. } = e else {
        return None;
    };
    if let Some(a) = about {
        return Some(a.clone());
    }
    let word = text.split(' ').next().unwrap_or_default();
    let name = word.strip_suffix("'s").unwrap_or(word);
    if name.is_empty() {
        return None;
    }
    Some(
        state
            .and_then(|s| s.thread_named(name))
            .map_or_else(|| name.to_string(), |t| t.id.clone()),
    )
}

/// A thread's run of routine updates ([`runs`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Run {
    /// The thread (its id, or the name an older update says when no thread has it).
    pub thread: String,
    /// The orchestrator's conversation it is in (its thread's id): its seqs are that thread's.
    pub conv: String,
    /// What a click on its row puts in the expanded groups: `routine:<thread>:<first seq>`.
    pub key: String,
    /// Its updates' places in the entries, oldest first (one at least).
    pub entries: Vec<usize>,
    /// Its oldest and newest update's event seq.
    pub first_seq: u64,
    pub newest_seq: u64,
    /// No update about its thread that is not routine came after it: the thread's live run.
    pub live: bool,
}

impl Run {
    /// The place of its newest update: where its row is drawn.
    pub fn newest(&self) -> usize {
        self.entries.last().copied().unwrap_or_default()
    }

    /// Its row's text without a summary: its newest update's short form ([`short`]), and how
    /// many updates it has when more than one (`background work finished · 3 updates`).
    pub fn plain(&self, entries: &[Entry], state: Option<&AppState>) -> String {
        let e = &entries[self.newest()];
        let said = kind(e, state).map_or_else(String::new, |k| short(e, k));
        match self.entries.len() {
            1 => said,
            n => format!("{said} · {n} updates"),
        }
    }
}

/// The routine runs of a conversation's entries ([`runs`]), and the run each entry is in.
#[derive(Debug, Clone, Default)]
pub struct Runs {
    /// Oldest first (by their first update).
    pub runs: Vec<Run>,
    /// By entry: its run's index in `runs`.
    of: Vec<Option<usize>>,
}

impl Runs {
    /// The run entry `i` is in, if it is a routine update.
    pub fn of(&self, i: usize) -> Option<&Run> {
        self.of.get(i).copied().flatten().map(|r| &self.runs[r])
    }
}

/// `entries`' runs, the entries of conversation `conv` (the orchestrator thread's id): per
/// thread, its routine updates ([`kind`], with `state` for stalls) since the last hub update
/// about it that is not routine. Only hub updates count: a reply of the orchestrator's about
/// the thread, a prompt or a tool call ends no run.
pub fn runs(entries: &[Entry], state: Option<&AppState>, conv: &str) -> Runs {
    let mut out = Runs {
        runs: Vec::new(),
        of: vec![None; entries.len()],
    };
    let mut open: HashMap<String, usize> = HashMap::new();
    for (i, e) in entries.iter().enumerate() {
        let Entry::HubUpdate { seq, .. } = e else {
            continue;
        };
        let Some(thread) = thread_of(e, state) else {
            continue;
        };
        if kind(e, state).is_none() {
            if let Some(r) = open.remove(&thread) {
                out.runs[r].live = false;
            }
            continue;
        }
        let runs = &mut out.runs;
        let r = *open.entry(thread.clone()).or_insert_with(|| {
            // (An entry put there without its event has no seq: its place says which.)
            let first = if *seq == 0 {
                format!("#{i}")
            } else {
                seq.to_string()
            };
            runs.push(Run {
                key: format!("routine:{thread}:{first}"),
                thread,
                conv: conv.to_string(),
                entries: Vec::new(),
                first_seq: *seq,
                newest_seq: *seq,
                live: true,
            });
            runs.len() - 1
        });
        let run = &mut out.runs[r];
        run.entries.push(i);
        run.newest_seq = *seq;
        out.of[i] = Some(r);
    }
    out
}

/// Routine update `e` (of kind `k`) said short, without its thread's name: `yielded with 2
/// background tasks still running: <its report>`, `background work finished`, `background work
/// finished; woken to continue`, `was stalled (No activity for 10 min), went on`.
fn short(e: &Entry, k: Routine) -> String {
    let Entry::HubUpdate { text, .. } = e else {
        return String::new();
    };
    let text = text.replace(['\n', '\r'], " ");
    // Its words after the thread's name.
    let rest = text
        .split_once(' ')
        .map_or(text.as_str(), |(_, rest)| rest)
        .to_string();
    match k {
        // `yielded with 2 background tasks still running (interim report: …) (success): …`:
        // what ran on, and what its report says.
        Routine::Interim => match rest.split_once("): ") {
            Some((head, said)) => {
                let head = head.split(" (").next().unwrap_or(head);
                format!("{head}: {said}")
            }
            None => rest,
        },
        Routine::BackgroundDone => "background work finished".into(),
        Routine::AutoWake => "background work finished; woken to continue".into(),
        Routine::Stall => match text.split_once(" is stalled: ") {
            Some((_, why)) => format!("was stalled ({why}), went on"),
            None => "was stalled, went on".into(),
        },
        Routine::Other => rest,
    }
}

/// A live run's folded row as a frame drew it ([`crate::select::Panes::routine`]): what a
/// summary may be asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrawnRun {
    /// [`Run::thread`].
    pub thread: String,
    /// [`Run::conv`].
    pub conv: String,
    /// [`Run::key`].
    pub key: String,
    /// [`Run::newest_seq`].
    pub newest_seq: u64,
    /// [`Run::live`].
    pub live: bool,
    /// Its row on screen.
    pub area: Rect,
}

/// A routine update, as a summary covers it: the orchestrator's conversation (its thread's
/// id) and the update's `seq` there. A reset or a switch of the orchestrator is a new thread
/// whose seqs start again: an update is told by both.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Covered {
    pub conv: String,
    pub seq: u64,
}

impl Covered {
    fn new(conv: &str, seq: u64) -> Self {
        Covered {
            conv: conv.to_string(),
            seq,
        }
    }
}

/// How the summaries of one thread are paced ([`AppState::take_summaries_due`]).
#[derive(Debug, Clone, Default, PartialEq)]
struct Pace {
    /// The newest update of the live run the last ask was about and got a summary or `none`
    /// for (or an answer of another conversation's); `None` once the ask came to nothing, so
    /// the run is due again: the hub said to ask later (too soon, its hourly cap, a runtime
    /// backing off), the call failed, the hub can't run summaries, or the ask was dropped.
    seq: Option<Covered>,
    /// The thread's activity then ([`activity_of`]).
    activity: String,
    /// No ask before this (`[tui] summary_every` after the last ask, or the hub's
    /// `next_after_secs`).
    next_at: Option<Instant>,
    /// The ask on its way ([`Ask::id`]): only its answer is taken (one of an ask dropped
    /// before, by `/summaries off`, say, may still come).
    asking: Option<u64>,
    /// The last ask failed, found the hub unable to run summaries, or was dropped: `/summaries
    /// on` asks again at once rather than at `next_at`.
    missed: bool,
    /// The newest update of a run the hub read none of (it named the conversation it read and
    /// covered no update of it: no routine update of its own marks is in the run): the run is
    /// not asked about again, its activity changing or not, until it has a newer update.
    barren: Option<Covered>,
}

impl Pace {
    /// The ask came to nothing: its run is due again once `next_at` allows (or at once after
    /// `/summaries on`).
    fn miss(&mut self) {
        self.seq = None;
        self.asking = None;
        self.missed = true;
    }
}

/// What the TUI keeps of summaries ([`AppState::summary`]; see the module docs).
#[derive(Debug, Clone, Default)]
pub struct Summaries {
    /// On (`[tui] summaries`, `/summaries`).
    pub on: bool,
    /// `[tui] summary_every` ([`Summaries::every`]).
    pub every: Duration,
    /// Each thread's answers, with the newest update each covered, oldest first: a summary,
    /// or `None` for one the hub found nothing to say of (`NONE`), the run then read as its
    /// updates. One per run ([`keep`]): the live run's latest, and the one each closed run
    /// still in its conversation's transcript ended with.
    texts: HashMap<String, VecDeque<(Covered, Option<String>)>>,
    /// Each thread's pace.
    pace: HashMap<String, Pace>,
    /// The hub's summary calls, as its last answer counted them.
    pub stats: SummaryStats,
    /// Why summaries stopped in this run: the hub can't run them; the notice saying so waits
    /// for a free status bar.
    pub off_why: Option<String>,
    /// A summary call failed already (the status bar said so once).
    failed: bool,
    /// The user turned summaries on (`/summaries on`) since the last asks: the next ones say
    /// so ([`SummaryThreadParams::retry`]), and the hub probes the agents' CLIs afresh and
    /// tries again what its calls found unable to run.
    retry: bool,
    /// The stalled items that resolved by themselves: the hub closed them because their
    /// thread went on working ([`twr_core::event::WENT_ON_KEY`]). Only their updates fold.
    /// One answered (the user, the orchestrator), closed otherwise (its thread stopped,
    /// crashed or was interrupted, its turn failed, a hung-turn card took its place, the hub's
    /// startup), closed by an older hub that doesn't say why, or still open, as far as the
    /// control stream told, never does. Kept from the control stream, its replay at start too
    /// ([`AppState::note_stall_resolved`]).
    pub went_on: HashSet<String>,
}

impl Summaries {
    /// The shortest time between two asks of a thread: `[tui] summary_every`, a minute at
    /// least.
    pub fn every(&self) -> Duration {
        self.every.max(twr_core::config::MIN_SUMMARY_EVERY)
    }

    /// Whether summaries may be asked for and shown: on, and the hub able to run them.
    pub fn running(&self) -> bool {
        self.on && self.off_why.is_none()
    }

    /// Keeps `text` as `thread`'s summary of its update `seq` in conversation `conv`, as an
    /// answer would.
    #[cfg(test)]
    pub(crate) fn texts_for_test(&mut self, thread: &str, conv: &str, seq: u64, text: &str) {
        keep(
            self.texts.entry(thread.to_string()).or_default(),
            Covered::new(conv, seq),
            Some(text.to_string()),
            &HashMap::new(),
        );
    }

    /// The id of the ask about `thread` on its way, if one is.
    #[cfg(test)]
    pub(crate) fn asking(&self, thread: &str) -> Option<u64> {
        self.pace.get(thread).and_then(|p| p.asking)
    }

    /// `thread`'s answers kept, with the update each covered (its conversation and seq),
    /// oldest first (`None`: no summary).
    #[cfg(test)]
    pub(crate) fn texts_for_thread(&self, thread: &str) -> Vec<(String, u64, Option<String>)> {
        self.texts
            .get(thread)
            .map(|t| {
                t.iter()
                    .map(|(c, text)| (c.conv.clone(), c.seq, text.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// A thread's runs in a conversation whose transcript the TUI holds, caught up ([`keep`]):
/// each run's oldest and newest update's seq, and the newest seq the transcript has. By
/// conversation, `None` for a transcript held but catching up (its runs not all there yet).
#[derive(Debug, Default)]
struct Spans {
    runs: Vec<(u64, u64)>,
    last_seq: u64,
}

impl Spans {
    /// The run update `seq` is in, if the transcript shows it in one.
    fn run_of(&self, seq: u64) -> Option<(u64, u64)> {
        self.runs
            .iter()
            .copied()
            .find(|(first, newest)| (*first..=*newest).contains(&seq))
    }
}

/// Keeps `text` as the answer of update `at` (a summary, or none) among a thread's answers
/// `kept`, oldest first, a conversation's in the order of their updates, after those of the
/// conversations before it; `spans`: the thread's runs by conversation, of the transcripts
/// held ([`Spans`]). One answer per run: `at`'s takes the place of the answers of its run (one of the
/// same update too), unless one of a newer update of it is kept (the row shows that one). A
/// closed run keeps the answer it ended with while it is in its conversation's transcript;
/// one whose run left it goes, and so do the answers of a conversation the TUI holds no
/// transcript of, but `at`'s: a run of it may come back. An answer of an update the
/// transcript has not reached yet stays, as do those of a transcript catching up.
fn keep(
    kept: &mut VecDeque<(Covered, Option<String>)>,
    at: Covered,
    text: Option<String>,
    spans: &HashMap<String, Option<Spans>>,
) {
    let run = spans
        .get(&at.conv)
        .and_then(Option::as_ref)
        .and_then(|s| s.run_of(at.seq));
    let same_run = |c: &Covered| {
        c.conv == at.conv && (c.seq == at.seq || run.is_some_and(|(f, n)| (f..=n).contains(&c.seq)))
    };
    if kept.iter().any(|(c, _)| same_run(c) && c.seq > at.seq) {
        return;
    }
    kept.retain(|(c, _)| {
        if same_run(c) {
            return false;
        }
        match spans.get(&c.conv) {
            Some(Some(s)) => c.seq > s.last_seq || s.run_of(c.seq).is_some(),
            Some(None) => true,
            None => c.conv == at.conv,
        }
    });
    let pos = kept
        .iter()
        .rposition(|(c, _)| c.conv != at.conv || c.seq < at.seq)
        .map_or(0, |p| p + 1);
    kept.insert(pos, (at, text));
}

/// What a workspace keeps of its summaries while another workspace is shown
/// ([`crate::workspace::SavedView`]): each thread's summaries, their pace and the hub's
/// counts, so coming back a run reads as it did and no thread is asked about sooner than its
/// pace allows.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Saved {
    texts: HashMap<String, VecDeque<(Covered, Option<String>)>>,
    pace: HashMap<String, Pace>,
    stats: SummaryStats,
}

/// The asks made so far (in this process): each ask's [`Ask::id`].
static ASKS: AtomicU64 = AtomicU64::new(0);

/// A summary to ask for ([`AppState::take_summaries_due`]).
#[derive(Debug, Clone, PartialEq)]
pub struct Ask {
    /// Which ask it is: its answer names it ([`AppState::summary_result`]).
    pub id: u64,
    /// The thread's id.
    pub thread: String,
    /// `summary.thread`'s parameters.
    pub params: SummaryThreadParams,
}

/// What came of an ask ([`AppState::summary_result`]).
#[derive(Debug, Clone, PartialEq)]
pub enum Answer {
    /// The hub's answer (boxed: it is large beside the others).
    Got(Box<SummaryThreadResult>),
    /// The call failed (no connection, an error of the hub's).
    Failed(String),
    /// The hub has no `summary.thread` (an older twr).
    Unsupported,
}

/// The thread's activity line as `thread.activity` last said it: what changes while it works.
fn activity_of(state: &AppState, thread: &str) -> String {
    state.activity.get(thread).map_or_else(String::new, |a| {
        format!(
            "{}\n{}\n{}",
            a.activity,
            a.said.as_deref().unwrap_or_default(),
            a.task.as_deref().unwrap_or_default()
        )
    })
}

/// `summary_every` said in words: `5 min`, `1 h`, `90 s`.
pub fn every_words(d: Duration) -> String {
    let s = d.as_secs();
    if s >= 3600 && s.is_multiple_of(3600) {
        format!("{} h", s / 3600)
    } else if s >= 60 && s.is_multiple_of(60) {
        format!("{} min", s / 60)
    } else {
        format!("{s} s")
    }
}

/// What summaries cost so far, as the usage line and `/summaries` say it: `$0.0008`.
pub fn cost_words(usd: f64) -> String {
    if usd < 1.0 {
        format!("${usd:.4}")
    } else {
        format!("${usd:.2}")
    }
}

impl AppState {
    /// What this workspace keeps of its summaries while another one is shown ([`Saved`]).
    pub(crate) fn take_summaries(&mut self) -> Saved {
        Saved {
            texts: std::mem::take(&mut self.summary.texts),
            pace: std::mem::take(&mut self.summary.pace),
            stats: self.summary.stats,
        }
    }

    /// Puts back what this workspace kept of its summaries ([`AppState::take_summaries`]).
    /// An ask that was on its way when the user left was answered to the view that is gone:
    /// its run is asked about again at once (what the hub made of it meanwhile, it answers
    /// from its cache).
    pub(crate) fn restore_summaries(&mut self, saved: Saved) {
        self.summary.texts = saved.texts;
        self.summary.pace = saved.pace;
        self.summary.stats = saved.stats;
        for p in self
            .summary
            .pace
            .values_mut()
            .filter(|p| p.asking.is_some())
        {
            p.miss();
            p.next_at = None;
        }
    }

    /// Before control event `ev` is applied: a stalled item the hub closed because its thread
    /// went on working ([`twr_core::event::WENT_ON_KEY`]) is remembered, so its update folds
    /// as one that went on ([`Summaries::went_on`]). No other: one closed `by: timeout` without
    /// it (its thread stopped, crashed or was interrupted, its turn failed, a hung-turn card
    /// took its place, the hub's startup closed it; an older hub, which never says) stays.
    pub(crate) fn note_stall_resolved(&mut self, ev: &Event) {
        if let EventBody::AttentionResolved(p) = &ev.body {
            let stalled = self
                .attention
                .get(&p.attn_id)
                .is_some_and(|i| i.reason == AttentionReason::Stalled);
            if stalled && went_on(&p.ext) {
                self.summary.went_on.insert(p.attn_id.clone());
            }
        }
    }

    /// The summary run `run`'s row shows, with summaries on and the hub able to run them: the
    /// latest answer kept that covered one of its updates (in its conversation: another's seqs
    /// say nothing of it), none when that answer was `NONE` (an earlier summary of the run
    /// says what is no longer so).
    pub fn summary_text(&self, run: &Run) -> Option<&str> {
        if !self.summary.running() {
            return None;
        }
        self.summary
            .texts
            .get(&run.thread)?
            .iter()
            .rev()
            .find(|(c, _)| c.conv == run.conv && (run.first_seq..=run.newest_seq).contains(&c.seq))
            .and_then(|(_, text)| text.as_deref())
    }

    /// The summaries to ask for now: one per thread whose live run's row the last frame drew,
    /// when the run has an update the last ask didn't or the thread works and its activity
    /// changed, once the pace allows, with none of its asks on the way (see the module
    /// docs). They are then on their way. The first ever waits for the notice of what
    /// summaries send to have been drawn whole ([`crate::firstrun::summary_notice`]): until
    /// then nothing is asked, and the notice is shown.
    pub fn take_summaries_due(&mut self, now: Instant) -> Vec<Ask> {
        if !self.summary.on {
            return Vec::new();
        }
        if self.summary.off_why.is_some() {
            self.tell_summary_unavailable();
            return Vec::new();
        }
        let mut due: Vec<(String, Covered, String)> = Vec::new();
        // The hub reads the current orchestrator's conversation: a run of a retired one's (a
        // frame drawn before the main pane followed a reset or a switch) is no run it can
        // summarize.
        let conv = self.orchestrator().map(|o| o.id.clone());
        for r in self
            .view
            .panes
            .routine
            .iter()
            .filter(|r| r.live && conv.as_deref() == Some(r.conv.as_str()))
        {
            let Some(t) = self
                .registry
                .get(&r.thread)
                .filter(|t| !t.is_orchestrator())
            else {
                continue;
            };
            if due.iter().any(|(id, _, _)| *id == t.id) {
                continue;
            }
            let activity = activity_of(self, &t.id);
            let works = matches!(t.status(), ThreadStatus::Running | ThreadStatus::Background);
            let newest = Covered::new(&r.conv, r.newest_seq);
            let ready = self.summary.pace.get(&t.id).is_none_or(|p| {
                p.asking.is_none()
                    && p.next_at.is_none_or(|at| now >= at)
                    && p.barren.as_ref() != Some(&newest)
                    && (p.seq.as_ref() != Some(&newest) || (works && p.activity != activity))
            });
            if ready {
                due.push((t.id.clone(), newest, activity));
            }
        }
        if due.is_empty() {
            return Vec::new();
        }
        if !self.seen.summary_notice {
            let every = self.summary.every();
            let notice = crate::firstrun::summary_notice(&self.view.suggest_model, every);
            let short = crate::firstrun::summary_notice_short(&self.view.suggest_model, every);
            self.wait_for_notice(Once::SummaryNotice, notice, short);
            return Vec::new();
        }
        let every = self.summary.every();
        let models = self.view.suggest_model.clone();
        let retry = std::mem::take(&mut self.summary.retry);
        due.into_iter()
            .map(|(thread, newest, activity)| {
                let id = ASKS.fetch_add(1, Ordering::Relaxed);
                self.summary.pace.insert(
                    thread.clone(),
                    Pace {
                        seq: Some(newest),
                        activity,
                        next_at: now.checked_add(every),
                        asking: Some(id),
                        missed: false,
                        barren: None,
                    },
                );
                Ask {
                    id,
                    params: SummaryThreadParams {
                        thread: thread.clone(),
                        models: Some(models.clone()),
                        every_secs: every.as_secs(),
                        retry,
                    },
                    thread,
                }
            })
            .collect()
    }

    /// What came of ask `ask` about `thread` at `now`: a summary is kept with the update it
    /// covered; the hub's `next_after_secs` puts the next ask off; asked too soon, or while
    /// the hub holds off (its hourly cap, a runtime backing off or unable to run), it is asked
    /// again once the pace allows; a failure is said once; the hub unable to run any is the
    /// end of them for this run. Only the answer of the thread's ask on its way is taken.
    pub(crate) fn summary_result(&mut self, thread: &str, ask: u64, answer: Answer, now: Instant) {
        let Some(pace) = self
            .summary
            .pace
            .get_mut(thread)
            .filter(|p| p.asking == Some(ask))
        else {
            // Turned off (and on, maybe asking again) meanwhile: its ask was dropped.
            return;
        };
        pace.asking = None;
        // An answer that is no summary leaves the run due again (`next_at` kept): the same
        // updates are asked about once the pace allows, or at once after `/summaries on`.
        let r = match answer {
            Answer::Got(r) => *r,
            Answer::Failed(e) => {
                pace.miss();
                self.note_summary_failure(&e);
                return;
            }
            Answer::Unsupported => {
                pace.miss();
                self.summaries_unavailable("this hub's twr has no thread summaries yet");
                return;
            }
        };
        self.summary.stats = r.stats;
        // (No longer than the longest span a config says: a clock can't go that far.)
        let later = r
            .next_after_secs
            .map(|secs| Duration::from_secs(secs).min(twr_core::config::MAX_SPAN))
            .and_then(|d| now.checked_add(d));
        if let Some(later) = later {
            pace.next_at = Some(pace.next_at.map_or(later, |at| at.max(later)));
        }
        if let Some(why) = r
            .reason
            .as_deref()
            .and_then(|r| r.strip_prefix(crate::suggest::UNAVAILABLE))
        {
            pace.miss();
            let why = why.to_string();
            self.summaries_unavailable(&why);
            return;
        }
        match r.reason.as_deref() {
            // The last summary is someone else's of a moment ago (or of another basis than
            // this ask's, made while it waited), or none may be made yet: asked again once
            // the pace allows.
            Some("interval" | "limit" | "backoff") => {
                pace.seq = None;
                return;
            }
            Some(why) if why.starts_with("retry: ") => {
                pace.seq = None;
                return;
            }
            Some(why) if why == "failed" || why.starts_with("failed: ") => {
                pace.miss();
                let why = why.strip_prefix("failed: ").unwrap_or(why).to_string();
                self.note_summary_failure(&why);
                return;
            }
            _ => {}
        }
        // The conversation the hub read (`covers_in`): another than the one the ask was about
        // (the orchestrator was reset or switched between the frame and the hub's read) says
        // nothing of the run, whatever seq it names: the run is due again once the pace
        // allows.
        let Some(asked) = pace.seq.clone() else {
            return;
        };
        if r.covers_in.as_deref().is_some_and(|c| c != asked.conv) {
            pace.seq = None;
            return;
        }
        // The update the hub read last (`covers`): updates may have come between the ask and
        // its read, so it says which run the summary is of, and what of the run it saw (a
        // newer update it didn't see leaves the run due again). None of a hub that names the
        // conversation it read (`covers_in`): it read no routine update (an update that is
        // not routine closed the run first, or the run is of an older hub's words), so the
        // summary is of no run, and the run is not asked about again until it has a newer
        // update ([`Pace::barren`]): asking again would pay for the same nothing each time.
        // An older hub says neither: the update asked about.
        let covered = match (r.covers, r.covers_in.is_some()) {
            (Some(seq), _) => Covered::new(&asked.conv, seq),
            (None, true) => {
                pace.barren = Some(asked);
                return;
            }
            (None, false) => asked,
        };
        pace.seq = Some(covered.clone());
        // A newer answer of the same run takes the place of its earlier ones ([`keep`]); `NONE`
        // is kept too, so the run reads as its updates rather than as an older summary of it.
        let text = r.summary.filter(|s| !s.trim().is_empty());
        let spans = self.run_spans(thread);
        keep(
            self.summary.texts.entry(thread.to_string()).or_default(),
            covered,
            text,
            &spans,
        );
    }

    /// `thread`'s runs in each conversation whose transcript is held (the main pane's, or one
    /// it showed before; `None` while it catches up), [`Spans`]: what tells which answers are
    /// of one run, and which runs left their transcript ([`keep`]).
    fn run_spans(&self, thread: &str) -> HashMap<String, Option<Spans>> {
        self.transcript
            .iter()
            .chain(&self.transcripts)
            .map(|t| {
                if t.catching_up() {
                    return (t.thread.clone(), None);
                }
                let runs = runs(&t.entries, Some(self), &t.thread)
                    .runs
                    .into_iter()
                    .filter(|r| r.thread == thread)
                    .map(|r| (r.first_seq, r.newest_seq))
                    .collect();
                let spans = Spans {
                    runs,
                    last_seq: t.last_seq,
                };
                (t.thread.clone(), Some(spans))
            })
            .collect()
    }

    /// [`AppState::summary_result`] for the ask about `thread` on its way (an ask that is not,
    /// one dropped, when none is).
    #[cfg(test)]
    pub(crate) fn summary_result_now(&mut self, thread: &str, answer: Answer, now: Instant) {
        let ask = self.summary.asking(thread).unwrap_or(u64::MAX);
        self.summary_result(thread, ask, answer, now);
    }

    /// The hub can't run summaries (`why`): none is asked for again in this run, the rows read
    /// as their updates, and the status bar says so once ever
    /// ([`crate::firstrun::Seen::summary_unavailable`]), when nothing else is on it.
    pub(crate) fn summaries_unavailable(&mut self, why: &str) {
        self.summary.off_why = Some(why.to_string());
        // The asks on their way are dropped (the IO loop aborts them): after `/summaries on`,
        // their runs are asked about again.
        for p in self
            .summary
            .pace
            .values_mut()
            .filter(|p| p.asking.is_some())
        {
            p.miss();
        }
        // The notice that summaries are on would say what can't happen here.
        if self
            .once
            .as_ref()
            .is_some_and(|(w, _)| *w == Once::SummaryNotice)
        {
            self.once = None;
            self.view.status = None;
        }
        self.tell_summary_unavailable();
    }

    /// The notice that summaries can't run here, once ever, when the status bar is free.
    fn tell_summary_unavailable(&mut self) {
        let Some(why) = &self.summary.off_why else {
            return;
        };
        let busy = self.once.is_some() || self.view.confirm.is_some() || self.view.status.is_some();
        if self.seen.summary_unavailable || busy {
            return;
        }
        let text = crate::firstrun::summary_unavailable_notice(why);
        self.show_once(Once::SummaryUnavailable, text);
    }

    /// A summary call failed: the status bar says so the first time.
    fn note_summary_failure(&mut self, why: &str) {
        if !std::mem::replace(&mut self.summary.failed, true) {
            self.view.status = Some(format!("no summary: {why} (/summaries off stops them)"));
        }
    }

    /// `/summaries [on|off]`: turns summaries on or off for this TUI (no argument toggles),
    /// and says what that means and what they cost so far. Off drops the asks on their way
    /// (the IO loop aborts them, [`Summaries::running`]).
    pub(crate) fn toggle_summaries(&mut self, on: Option<bool>) {
        let on = on.unwrap_or(!self.summary.on);
        self.summary.on = on;
        self.summary.retry = on;
        self.summary.off_why = None;
        self.summary.failed = false;
        // An ask on its way is dropped (off), or was by the off before (on): its run is
        // asked about again. On: a run whose last ask came to nothing (it failed, the hub
        // couldn't run it, it was dropped) is asked about again at once, its pace and the
        // hub's own limits allowing.
        for p in self.summary.pace.values_mut() {
            if p.asking.is_some() {
                p.miss();
            }
            if on && std::mem::take(&mut p.missed) {
                p.next_at = None;
            }
        }
        // The notice is told only by itself, drawn whole: what `/summaries` says is not it.
        if self
            .once
            .as_ref()
            .is_some_and(|(w, _)| *w == Once::SummaryNotice)
        {
            self.once = None;
        }
        let s = self.summary.stats;
        let so_far = format!("{} so far, {}", s.calls, cost_words(s.cost_usd));
        let m = &self.view.suggest_model;
        self.view.status = Some(if on {
            format!(
                "summaries on: for a thread's folded routine updates, its name, state, activity \
                 and those updates go to {} through Claude Code (to Anthropic) or {} through \
                 Codex (to OpenAI), the thread's own runtime first, with your login, at most \
                 once per {} a thread · {so_far}",
                m.claude_code,
                m.codex,
                every_words(self.summary.every())
            )
        } else {
            format!("summaries off: routine updates fold into rows of their own words · {so_far}")
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ext(v: serde_json::Value) -> Option<JsonMap> {
        v.as_object().cloned()
    }

    /// The hub's mark says the kind; an update of an older hub is told by its words; any other
    /// update is not routine, whatever its text says further on.
    #[test]
    fn routine_updates_are_told_by_their_mark_or_their_words() {
        let marked = |k: &str| ext(json!({"twr": {"routine": k}}));
        assert_eq!(Routine::of(&marked("interim"), "x"), Some(Routine::Interim));
        assert_eq!(
            Routine::of(&marked("background_done"), "x"),
            Some(Routine::BackgroundDone)
        );
        assert_eq!(
            Routine::of(&marked("auto_wake"), "x"),
            Some(Routine::AutoWake)
        );
        assert_eq!(Routine::of(&marked("later"), "x"), Some(Routine::Other));
        // Older hubs.
        for (text, kind) in [
            (
                "api yielded with 2 background task(s) (success): started the build",
                Routine::Interim,
            ),
            (
                "api yielded with 1 background task still running (interim report: the result \
                 comes later) (success): tests run",
                Routine::Interim,
            ),
            ("api background work finished.", Routine::BackgroundDone),
            (
                "api's background work finished and no turn started after it: twr woke it to \
                 continue (after its interim report of turn 3).",
                Routine::AutoWake,
            ),
        ] {
            assert_eq!(Routine::of(&None, text), Some(kind), "{text}");
        }
        for text in [
            "api finished (success): yielded with 2 background tasks",
            "api failed: background work finished.",
            "api needs permission: Bash: ls",
            "api seems hung: turn 3",
            "api is stalled: No activity for 10 min",
            "api request a1 closed.",
            // The hub didn't wake it: the user is asked to act.
            "api's background work finished and no turn started after it; twr did not wake \
             it (woken 5 times in a row already). Send it a message to continue.",
            "Confirmed. Applied: stop api. Skipped: nothing.",
            "the api yielded with 2 background tasks",
            // A failed turn's report, as an older hub said it.
            "api yielded with 2 background tasks still running (interim report: the result \
             comes later) (gave_up): Turn 3 failed.",
            "api yielded with 1 background task(s) (gave_up): it broke",
        ] {
            assert_eq!(Routine::of(&None, text), None, "{text}");
        }
        // Marked otherwise by a hub that knows the mark: not by its words (a failed turn's
        // report, though its tasks ran on, whatever its grade).
        assert_eq!(
            Routine::of(
                &ext(json!({"twr": {"routine": null}})),
                "api background work finished."
            ),
            None
        );
        assert_eq!(
            Routine::of(
                &ext(json!({"twr": {"routine": null, "background_tasks": 2}})),
                "api yielded with 2 background tasks still running (interim report: the result \
                 comes later) (success): Turn 3 failed."
            ),
            None
        );
    }

    /// The short forms: what ran on and what the report said, without the thread's name.
    #[test]
    fn a_runs_plain_text_is_its_newest_update_said_short() {
        let update = |text: &str, routine| Entry::HubUpdate {
            text: text.into(),
            user_message: None,
            attn_id: None,
            about: Some("t-api".into()),
            outcome: None,
            seq: 0,
            resolution: false,
            routine: Some(routine),
        };
        let entries = vec![
            update(
                "api yielded with 2 background tasks still running (interim report: the \
                 result comes later) (success): started the build",
                Routine::Interim,
            ),
            update("api background work finished.", Routine::BackgroundDone),
        ];
        let r = runs(&entries, None, "t-orch");
        assert_eq!(r.runs.len(), 1);
        assert_eq!(
            r.runs[0].plain(&entries, None),
            "background work finished · 2 updates"
        );
        assert_eq!(
            short(&entries[0], Routine::Interim),
            "yielded with 2 background tasks still running: started the build"
        );
        let stall = update("api is stalled: No activity for 10 min", Routine::Stall);
        assert_eq!(
            short(&stall, Routine::Stall),
            "was stalled (No activity for 10 min), went on"
        );
        assert_eq!(every_words(Duration::from_secs(300)), "5 min");
        assert_eq!(every_words(Duration::from_secs(3600)), "1 h");
        assert_eq!(every_words(Duration::from_secs(90)), "90 s");
        assert_eq!(cost_words(0.0008), "$0.0008");
        assert_eq!(cost_words(1.5), "$1.50");
    }
}

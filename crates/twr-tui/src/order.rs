//! Terminal card height measurement, using the same stable ordering as graphical clients.
use crate::select::ListLayout;
use std::collections::{HashMap, HashSet};
#[cfg(test)]
use std::time::Duration;
use std::time::Instant;
pub use twr_core::card_order::*;

/// Card `id` as a draw measured it on list `layout` (the column, the row, the grid or a
/// compact list), `width` cells wide: the height its text takes there, borders included
/// (`natural`), and how many of its rows are its status section (`section`: the state and
/// what the thread does, which changes every few seconds while it works).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Measure {
    pub layout: ListLayout,
    pub id: String,
    pub natural: u16,
    pub section: u16,
    pub width: u16,
}

/// How tall a card (or tile) is on one list ([`Measure`]): as tall as its text at once, and,
/// when its text gets shorter, the height it was until it may shrink ([`Fit::settle`]). So a
/// card whose text comes and goes (a working thread's activity line, an item answered) moves
/// the cards below it once in a while, not each time it changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fit {
    /// The height it is kept at, borders included: the tallest its text was since it last
    /// shrank.
    pub rows: u16,
    /// The height its text took as last drawn ([`Measure::natural`]).
    pub natural: u16,
    /// Its status section's rows as last drawn ([`Measure::section`]).
    pub section: u16,
    /// The most rows its status section took since it last shrank to its text.
    pub section_max: u16,
    /// The width it was drawn at: at another, it takes its natural height at once.
    pub width: u16,
    /// Since when it has waited to shrink: when its text got shorter, or when the wait last
    /// started again (the cards held, its thread at work).
    pub since: Option<Instant>,
    /// The wait was held at the last [`Fit::settle`], or by what it left when a draw found the
    /// text shorter ([`note`]): it starts again at the next one that lets it go.
    pub held: bool,
    /// What the last [`Fit::settle`] was told: the cards held (`.0`), its thread at work
    /// (`.1`). A draw after it that finds the text shorter judges the wait by them ([`note`]),
    /// so a turn that ends before the next settle starts the wait afresh, as one that ends
    /// after it does.
    pub settled: (bool, bool),
}

/// The cards' heights per list and card ([`Fit`]).
pub type Fits = HashMap<(ListLayout, String), Fit>;

impl Fit {
    fn of(m: &Measure) -> Self {
        Fit {
            rows: m.natural,
            natural: m.natural,
            section: m.section,
            section_max: m.section,
            width: m.width,
            since: None,
            held: false,
            settled: (false, false),
        }
    }

    /// The least it may shrink to: its natural height; for a thread at work (`working`), with
    /// its status section as tall as it was at most since it last shrank, its activity line
    /// coming and going: what an answered item took goes, what the activity took stays until
    /// the turn ends.
    fn floor(&self, working: bool) -> u16 {
        if working {
            let kept = self.section_max.saturating_sub(self.section);
            self.natural.saturating_add(kept).min(self.rows)
        } else {
            self.natural
        }
    }

    /// Shrinks it at `now` when it is due: its text was shorter for [`ORDER_DELAY`], the
    /// cards are not `held` (the same hold as the deferred moves'), and it has rows to give
    /// back (its thread at work or not, `working`, [`Fit::floor`]). Held, or with shorter text
    /// but no rows to give back, the wait starts again now, and again once let go.
    pub fn settle(&mut self, now: Instant, held: bool, working: bool) {
        self.settled = (held, working);
        if self.natural >= self.rows {
            return;
        }
        let floor = self.floor(working);
        let blocked = held || floor >= self.rows;
        if blocked || self.held {
            self.since = Some(now);
            self.held = blocked;
            return;
        }
        let since = *self.since.get_or_insert(now);
        if now >= since + ORDER_DELAY {
            self.rows = floor;
            if !working {
                self.section_max = self.section;
            }
            self.since = (self.natural < self.rows).then_some(now);
        }
    }

    /// When it is due to shrink ([`Fit::settle`]), if it waits to; `None` while it has no rows
    /// to give back (the end of its thread's turn draws a frame, which settles it).
    pub fn due(&self, working: bool) -> Option<Instant> {
        (self.natural < self.rows && self.floor(working) < self.rows)
            .then_some(self.since?)
            .map(|since| since + ORDER_DELAY)
    }
}

/// The height card `id` is drawn at on list `layout`, `width` cells wide, its text taking
/// `natural` rows: those, or the height it is kept at when that is taller and it is as wide as
/// when it was drawn so.
pub fn drawn_height(fits: &Fits, layout: ListLayout, id: &str, natural: u16, width: u16) -> u16 {
    match fits.get(&(layout, id.to_string())) {
        Some(f) if f.width == width => natural.max(f.rows),
        _ => natural,
    }
}

/// Notes what a draw at `now` measured of the cards ([`Measure`]): a card grows to its
/// natural height at once, and at once takes it at another width; one whose text got shorter
/// starts its wait to shrink then ([`Fit::settle`]), held when the settle before the draw had
/// the cards held or its thread at work with no rows to give back (so the wait starts again
/// once that ends, whether or not a frame came between); one whose text came back to its
/// height stops waiting. A card the draw did not measure (scrolled out of the row, on a list
/// not drawn) is forgotten: it next shows at its natural height.
pub fn note(fits: &mut Fits, measured: Vec<Measure>, now: Instant) {
    let mut seen = HashSet::new();
    for m in measured {
        let key = (m.layout, m.id.clone());
        match fits.get_mut(&key) {
            Some(f) if f.width == m.width => {
                f.natural = m.natural;
                f.section = m.section;
                f.section_max = f.section_max.max(m.section);
                if m.natural < f.rows {
                    f.since.get_or_insert(now);
                    let (held, working) = f.settled;
                    f.held |= held || f.floor(working) >= f.rows;
                } else {
                    f.rows = m.natural;
                    f.since = None;
                    f.held = false;
                }
            }
            _ => {
                fits.insert(key.clone(), Fit::of(&m));
            }
        }
        seen.insert(key);
    }
    fits.retain(|k, _| seen.contains(k));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A card's height: grows at once; shorter text waits [`ORDER_DELAY`], restarted while
    /// held and once let go; a thread at work keeps the rows its section took (an item's
    /// rows go); another width, or a draw that leaves it out, starts it afresh.
    #[test]
    fn a_card_grows_at_once_and_shrinks_late() {
        let t0 = Instant::now();
        let s = |k: u64| t0 + Duration::from_secs(k);
        let key = (ListLayout::Column, "t-a".to_string());
        let m = |natural: u16, section: u16, width: u16| Measure {
            layout: ListLayout::Column,
            id: "t-a".into(),
            natural,
            section,
            width,
        };
        let mut fits = Fits::new();
        note(&mut fits, vec![m(4, 1, 40)], s(0));
        assert_eq!(drawn_height(&fits, ListLayout::Column, "t-a", 6, 40), 6);
        note(&mut fits, vec![m(6, 3, 40)], s(0));
        assert_eq!(fits[&key].rows, 6);
        // Shorter: drawn as tall, waiting from the draw that found it shorter.
        note(&mut fits, vec![m(4, 1, 40)], s(1));
        assert_eq!(drawn_height(&fits, ListLayout::Column, "t-a", 4, 40), 6);
        assert_eq!(fits[&key].due(false), Some(s(1) + ORDER_DELAY));
        // At work, the section's rows stay: nothing to wait for.
        assert_eq!(fits[&key].due(true), None);
        fits.get_mut(&key).unwrap().settle(s(9), false, true);
        assert_eq!(fits[&key].rows, 6);
        // The turn over: the wait starts again at the next settle, then it shrinks.
        let f = fits.get_mut(&key).unwrap();
        f.settle(s(10), false, false);
        assert_eq!((f.rows, f.due(false)), (6, Some(s(10) + ORDER_DELAY)));
        f.settle(s(11), true, false);
        f.settle(s(20), false, false);
        assert_eq!(f.rows, 6, "held, then let go: waits again");
        f.settle(s(20) + ORDER_DELAY, false, false);
        assert_eq!((f.rows, f.since, f.due(false)), (4, None, None));
        // An item's rows (not the section's) go also while it works.
        note(&mut fits, vec![m(8, 1, 40)], s(30));
        note(&mut fits, vec![m(4, 1, 40)], s(31));
        let f = fits.get_mut(&key).unwrap();
        assert_eq!(f.due(true), Some(s(31) + ORDER_DELAY));
        f.settle(s(31) + ORDER_DELAY, false, true);
        assert_eq!(f.rows, 4);
        // Another width: its natural height at once.
        note(&mut fits, vec![m(6, 3, 40)], s(40));
        note(&mut fits, vec![m(4, 1, 40)], s(41));
        assert_eq!(drawn_height(&fits, ListLayout::Column, "t-a", 4, 36), 4);
        note(&mut fits, vec![m(4, 1, 36)], s(41));
        assert_eq!((fits[&key].rows, fits[&key].since), (4, None));
        // Not drawn: forgotten.
        note(&mut fits, Vec::new(), s(42));
        assert!(fits.is_empty());
    }

    /// A card whose text got shorter while the settle before the draw held it (its thread at
    /// work with nothing to give back, or the cards held) waits [`ORDER_DELAY`] from when that
    /// ends, also when it ends before the next frame: no frame between the shorter draw and
    /// the turn's end (or the cards let go) shortens the wait.
    #[test]
    fn a_wait_held_at_the_shorter_draw_starts_again_once_let_go() {
        let t0 = Instant::now();
        let s = |k: u64| t0 + Duration::from_secs(k);
        let key = (ListLayout::Column, "t-a".to_string());
        let m = |natural: u16, section: u16| Measure {
            layout: ListLayout::Column,
            id: "t-a".into(),
            natural,
            section,
            width: 40,
        };
        // (held, working) as the settle before the shorter draw had them, then as the turn's
        // end (or the cards let go) has them.
        for (before, after) in [
            ((false, true), (false, false)),
            ((true, false), (false, false)),
        ] {
            let mut fits = Fits::new();
            note(&mut fits, vec![m(6, 3)], s(0));
            fits.get_mut(&key).unwrap().settle(s(0), before.0, before.1);
            // The draw that finds it shorter, at once after that settle.
            note(&mut fits, vec![m(4, 1)], s(0));
            // The turn ends (the cards let go) a second later, with no frame between.
            let f = fits.get_mut(&key).unwrap();
            f.settle(s(1), after.0, after.1);
            assert_eq!(f.due(false), Some(s(1) + ORDER_DELAY), "{before:?}");
            f.settle(s(0) + ORDER_DELAY, after.0, after.1);
            assert_eq!(f.rows, 6, "{before:?}: not before the delay from the end");
            f.settle(s(1) + ORDER_DELAY, after.0, after.1);
            assert_eq!(f.rows, 4, "{before:?}");
        }
        // Nothing held it then: it waits from the shorter draw, as before.
        let mut fits = Fits::new();
        note(&mut fits, vec![m(6, 3)], s(0));
        fits.get_mut(&key).unwrap().settle(s(0), false, false);
        note(&mut fits, vec![m(4, 1)], s(0));
        let f = fits.get_mut(&key).unwrap();
        f.settle(s(1), false, false);
        assert_eq!(f.due(false), Some(s(0) + ORDER_DELAY));
        f.settle(s(0) + ORDER_DELAY, false, false);
        assert_eq!(f.rows, 4);
    }
}

//! The order the cards (and grid tiles) show in with `[tui] card_order = "stable"`: the order
//! the hub's state asks for ([`twr_core::reduce::stack_order`]) is only a target. A card that
//! comes to need you (a new permission, question, error, review or stall: an item it did not
//! show) moves to its place among those at once, a new card shows at once and a gone one goes,
//! but any other move (a thread finishing, failing, going idle or parked, an answered item
//! letting its card go down) waits ([`settle`]): it is applied [`ORDER_DELAY`] after the
//! target last changed (at most [`ORDER_WAIT_MAX`] after it first differed), and only while
//! the cards are left alone (no card selected, no action popup or detail view open, the
//! mouse not over the cards); once they are, the delay starts again. Until then the cards
//! keep the layout last shown ([`Shown`]).
//!
//! A card's height follows the same rule ([`Fit`]): it is as tall as its text at once, and
//! when its text gets shorter it keeps its height until the shorter text has held for
//! [`ORDER_DELAY`] with the cards left alone and its thread not at work.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use crate::select::ListLayout;

/// How long a move other than to the top waits after the target last changed.
pub const ORDER_DELAY: Duration = Duration::from_secs(3);

/// The longest a move waits while the target keeps changing (the cards left alone).
pub const ORDER_WAIT_MAX: Duration = Duration::from_secs(10);

/// How long the mouse holds the cards in place after it last moved over them: a pointer left
/// there (or that left the window without the terminal saying so) stops holding them then.
pub const HOVER_HOLD: Duration = Duration::from_secs(30);

/// A card as the hub's state has it: its id, the items it has open (attention ids, the top
/// one first; none when it does not need you), and whether its thread is parked (the grid
/// puts those last).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Card {
    pub id: String,
    pub items: Vec<String>,
    pub parked: bool,
}

/// The cards as laid out: their order, the items each card placed as needing you had open
/// when it was placed and since (it keeps its place until the layout is next applied), and
/// which are parked.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Layout {
    pub order: Vec<String>,
    pub attention: HashMap<String, HashSet<String>>,
    pub parked: HashSet<String>,
}

impl Layout {
    /// The layout of `target` as it is.
    pub fn of(target: &[Card]) -> Self {
        Layout {
            order: target.iter().map(|c| c.id.clone()).collect(),
            attention: target
                .iter()
                .filter(|c| !c.items.is_empty())
                .map(|c| (c.id.clone(), c.items.iter().cloned().collect()))
                .collect(),
            parked: target
                .iter()
                .filter(|c| c.parked)
                .map(|c| c.id.clone())
                .collect(),
        }
    }
}

/// A move that waits: the layout it moves to, since when one has waited, when the target last
/// changed, and whether the cards were held at the last [`settle`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending {
    pub target: Layout,
    pub since: Instant,
    pub changed: Instant,
    pub held: bool,
}

impl Pending {
    /// When it is applied if the cards are left alone until then.
    pub fn due(&self) -> Instant {
        (self.changed + ORDER_DELAY).min(self.since + ORDER_WAIT_MAX)
    }
}

/// The cards as last shown, and the move that waits, if one does.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Shown {
    pub layout: Layout,
    pub pending: Option<Pending>,
}

/// Whether `card` moves at once over `shown`: it is new, or its top item is one it did not
/// have open as shown (a new permission, question, error, review or stall). An item that was
/// open under the one answered coming to the top is no new one: that card waits.
fn moves(card: &Card, shown: &Layout, known: &HashSet<&str>) -> bool {
    !known.contains(card.id.as_str())
        || card.items.first().is_some_and(|top| {
            !shown
                .attention
                .get(&card.id)
                .is_some_and(|had| had.contains(top))
        })
}

/// The order to show `target` in, given the layout shown: the shown order with the cards that
/// are gone left out, and, at their place in `target` (after the card `target` has before
/// them), the cards that move at once ([`moves`]). Every other card keeps its place.
pub fn arrange(target: &[Card], shown: &Layout) -> Vec<String> {
    let known: HashSet<&str> = shown.order.iter().map(String::as_str).collect();
    let stays: HashSet<&str> = target
        .iter()
        .filter(|c| !moves(c, shown, &known))
        .map(|c| c.id.as_str())
        .collect();
    let mut out: Vec<String> = shown
        .order
        .iter()
        .filter(|id| stays.contains(id.as_str()))
        .cloned()
        .collect();
    for (i, card) in target.iter().enumerate() {
        if stays.contains(card.id.as_str()) {
            continue;
        }
        // After the nearest card before it in the target that is placed already.
        let at = target[..i]
            .iter()
            .rev()
            .find_map(|before| out.iter().position(|o| *o == before.id))
            .map_or(0, |p| p + 1);
        out.insert(at, card.id.clone());
    }
    out
}

/// What to show of `target` at `now` after `shown` (`None`: nothing shown yet, which shows
/// `target` as it is), the cards `held` or not: the target when what differs is only what
/// shows at once, or when the move that waits is due and the cards are not held; else the
/// cards kept as shown but for what moves at once ([`arrange`]), and the move waiting, its
/// delay starting again while the cards are held and when they stop being held.
pub fn settle(target: &[Card], shown: Option<&Shown>, now: Instant, held: bool) -> Shown {
    let fresh = Layout::of(target);
    let Some(shown) = shown else {
        return Shown {
            layout: fresh,
            pending: None,
        };
    };
    let known: HashSet<&str> = shown.layout.order.iter().map(String::as_str).collect();
    let order = arrange(target, &shown.layout);
    // Placed as needing you: still listed, with the items it had and has open.
    let mut attention: HashMap<String, HashSet<String>> = shown
        .layout
        .attention
        .iter()
        .filter(|(id, _)| fresh.order.contains(id))
        .map(|(id, items)| (id.clone(), items.clone()))
        .collect();
    for (id, items) in &fresh.attention {
        attention
            .entry(id.clone())
            .or_default()
            .extend(items.iter().cloned());
    }
    let parked = target
        .iter()
        .filter(|c| match known.contains(c.id.as_str()) {
            true => shown.layout.parked.contains(&c.id),
            false => c.parked,
        })
        .map(|c| c.id.clone())
        .collect();
    let kept = Layout {
        order,
        attention,
        parked,
    };
    if kept == fresh {
        return Shown {
            layout: fresh,
            pending: None,
        };
    }
    let mut pending = match &shown.pending {
        Some(p) if p.target == fresh => p.clone(),
        Some(p) => Pending {
            target: fresh.clone(),
            changed: now,
            ..p.clone()
        },
        None => Pending {
            target: fresh.clone(),
            since: now,
            changed: now,
            held: false,
        },
    };
    if held || pending.held {
        // Held, or just let go: the delay starts (again) now.
        pending.since = now;
        pending.changed = now;
        pending.held = held;
    } else if now >= pending.due() {
        return Shown {
            layout: fresh,
            pending: None,
        };
    }
    Shown {
        layout: kept,
        pending: Some(pending),
    }
}

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

    fn cards(spec: &[(&str, Option<&str>)]) -> Vec<Card> {
        spec.iter()
            .map(|(id, item)| Card {
                id: id.to_string(),
                items: item.iter().map(|i| i.to_string()).collect(),
                parked: false,
            })
            .collect()
    }

    #[test]
    fn only_new_cards_and_cards_that_need_you_move() {
        let shown = Layout::of(&cards(&[("a", None), ("b", None), ("c", None)]));
        // The hub's order changed (c finished first): nothing moves.
        assert_eq!(
            arrange(&cards(&[("c", None), ("a", None), ("b", None)]), &shown),
            ["a", "b", "c"]
        );
        // c needs you: to the top at once.
        assert_eq!(
            arrange(
                &cards(&[("c", Some("p1")), ("a", None), ("b", None)]),
                &shown
            ),
            ["c", "a", "b"]
        );
        // A new card after the one the target has before it; a gone one goes.
        assert_eq!(
            arrange(&cards(&[("a", None), ("d", None), ("c", None)]), &shown),
            ["a", "d", "c"]
        );
        // The first card of the target, new: first.
        assert_eq!(
            arrange(
                &cards(&[("d", None), ("a", None), ("b", None), ("c", None)]),
                &shown
            ),
            ["d", "a", "b", "c"]
        );
    }

    /// A card placed as needing you keeps its place when its item is answered; a new item
    /// (a permission after its question) places it again at once, as does a second card that
    /// comes to need you.
    #[test]
    fn a_new_item_places_a_card_again() {
        let shown = Layout::of(&cards(&[("a", Some("q1")), ("b", Some("q2")), ("c", None)]));
        assert_eq!(
            arrange(
                &cards(&[("c", None), ("a", None), ("b", Some("q2"))]),
                &shown
            ),
            ["a", "b", "c"]
        );
        // b's permission comes before a's question in the target: b goes first.
        assert_eq!(
            arrange(
                &cards(&[("b", Some("p1")), ("a", Some("q1")), ("c", None)]),
                &shown
            ),
            ["b", "a", "c"]
        );
        assert_eq!(
            arrange(
                &cards(&[("a", Some("q1")), ("c", Some("e1")), ("b", Some("q2"))]),
                &shown
            ),
            ["a", "c", "b"]
        );
        // a had a permission over a review; b a question. The permission answered, the
        // review comes to a's top, behind b's question in the target: a waits.
        let a = |items: &[&str]| Card {
            id: "a".into(),
            items: items.iter().map(|i| i.to_string()).collect(),
            parked: false,
        };
        let b = Card {
            id: "b".into(),
            items: vec!["q2".into()],
            parked: false,
        };
        let shown = Layout::of(&[a(&["p1", "r1"]), b.clone()]);
        assert_eq!(arrange(&[b.clone(), a(&["r1"])], &shown), ["a", "b"]);
        // A new item on a: placed again.
        assert_eq!(arrange(&[b, a(&["e2", "r1"])], &shown), ["b", "a"]);
    }

    /// A move waits ORDER_DELAY after the target last changed, at most ORDER_WAIT_MAX after it
    /// first differed; while the cards are held it does not happen, and once they are let go
    /// its delay starts again.
    #[test]
    fn a_move_waits_for_the_delay_and_for_the_cards_to_be_let_go() {
        let t0 = Instant::now();
        let s = |secs: f64| t0 + Duration::from_secs_f64(secs);
        let abc = cards(&[("a", None), ("b", None), ("c", None)]);
        let bca = cards(&[("b", None), ("c", None), ("a", None)]);
        let cab = cards(&[("c", None), ("a", None), ("b", None)]);
        let shown = settle(&abc, None, t0, false);
        let shown = settle(&bca, Some(&shown), s(0.0), false);
        assert_eq!(shown.layout.order, ["a", "b", "c"]);
        // Another change at 2.9 s: its own delay.
        let shown = settle(&cab, Some(&shown), s(2.9), false);
        let shown = settle(&cab, Some(&shown), s(3.1), false);
        assert_eq!(
            shown.layout.order,
            ["a", "b", "c"],
            "not 0.2 s after it changed"
        );
        assert_eq!(shown.pending.as_ref().unwrap().due(), s(5.9));
        let done = settle(&cab, Some(&shown), s(5.9), false);
        assert_eq!(done.layout.order, ["c", "a", "b"]);
        assert_eq!(done.pending, None);
        // A target that keeps changing: applied ORDER_WAIT_MAX after it first differed.
        let mut shown = settle(&bca, Some(&settle(&abc, None, t0, false)), t0, false);
        for k in 1..=3 {
            let target = if k % 2 == 0 { &bca } else { &cab };
            shown = settle(target, Some(&shown), s(2.5 * k as f64), false);
        }
        assert_eq!(shown.layout.order, ["a", "b", "c"]);
        let shown = settle(&bca, Some(&shown), s(10.0), false);
        assert_eq!(shown.layout.order, ["b", "c", "a"]);
        // Held long past it: kept; let go, a full delay from then.
        let shown = settle(&bca, Some(&settle(&abc, None, t0, false)), t0, true);
        let shown = settle(&bca, Some(&shown), s(60.0), true);
        assert_eq!(shown.layout.order, ["a", "b", "c"]);
        let shown = settle(&bca, Some(&shown), s(61.0), false);
        assert_eq!(shown.layout.order, ["a", "b", "c"], "just let go");
        assert_eq!(shown.pending.as_ref().unwrap().due(), s(64.0));
        let shown = settle(&bca, Some(&shown), s(64.0), false);
        assert_eq!(shown.layout.order, ["b", "c", "a"]);
    }

    /// A card that is parked keeps the place (and the grid its unparked place) until the
    /// move is applied; a new card's parked state is its own.
    #[test]
    fn parking_is_a_move_that_waits() {
        let t0 = Instant::now();
        let abc = cards(&[("a", None), ("b", None), ("c", None)]);
        let mut parked = cards(&[("b", None), ("c", None), ("a", None)]);
        parked[2].parked = true;
        let shown = settle(&abc, None, t0, false);
        let shown = settle(&parked, Some(&shown), t0, false);
        assert!(shown.layout.parked.is_empty());
        let mut new = parked.clone();
        new.push(Card {
            id: "d".into(),
            items: Vec::new(),
            parked: true,
        });
        let shown = settle(&new, Some(&shown), t0, false);
        assert_eq!(shown.layout.parked, HashSet::from(["d".to_string()]));
        let shown = settle(&new, Some(&shown), t0 + ORDER_DELAY, false);
        assert_eq!(shown.layout.parked.len(), 2);
    }

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

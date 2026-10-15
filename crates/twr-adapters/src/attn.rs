//! Attention ids an adapter hands out: one id names exactly one question.
//!
//! An adapter never changes what an open item asks under the same id. When a runtime asks
//! again under a request id that is still waiting (it replaced its request), the adapter
//! withdraws the old item (`attention.resolved{by: timeout, answer: null}` with
//! `ext.<runtime>.superseded = true`) and raises the new content under a NEW id, an
//! *incarnation* of the request id (`<request id>~<n>`, [`INCARNATION_SEP`]). Each adapter
//! keeps, per open id, exactly the content that id was raised with, and builds the runtime's
//! answer only from the content bound to the id that was answered; an answer to a withdrawn or
//! unknown id is ignored (logged). So an answer that crossed a replacement in flight (the hub
//! had not yet seen the withdrawal when the user answered) never reaches the replacement.
//!
//! [`IssuedIds`] gives the ids: never one of the last [`ISSUED_KEPT`] ids handed out, nor one
//! still open.

use std::collections::{HashSet, VecDeque};

/// Separator of a request id and its incarnation number in an attention id.
pub const INCARNATION_SEP: char = '~';

/// Attention ids remembered (never handed out again while remembered; open ids never are).
pub const ISSUED_KEPT: usize = 4096;

/// The attention ids a session handed out lately.
#[derive(Debug)]
pub struct IssuedIds {
    set: HashSet<String>,
    order: VecDeque<String>,
    cap: usize,
}

impl Default for IssuedIds {
    fn default() -> Self {
        IssuedIds::new(ISSUED_KEPT)
    }
}

impl IssuedIds {
    /// Remembers the last `cap` ids.
    pub fn new(cap: usize) -> Self {
        IssuedIds {
            set: HashSet::new(),
            order: VecDeque::new(),
            cap: cap.max(1),
        }
    }

    /// Whether `id` was handed out lately.
    pub fn contains(&self, id: &str) -> bool {
        self.set.contains(id)
    }

    fn remember(&mut self, id: String) {
        if !self.set.insert(id.clone()) {
            return;
        }
        self.order.push_back(id);
        while self.order.len() > self.cap {
            if let Some(old) = self.order.pop_front() {
                self.set.remove(&old);
            }
        }
    }

    /// A fresh base for `n` items of request `key`: `key` itself, else `key~1`, `key~2`, ...:
    /// the first base that, like each of its item ids (`item(base, i)` for `i < n`), was not
    /// handed out lately and is not `open`. The base and the item ids are remembered.
    pub fn fresh(
        &mut self,
        key: &str,
        n: usize,
        item: impl Fn(&str, usize) -> String,
        open: impl Fn(&str) -> bool,
    ) -> String {
        let n = n.max(1);
        let mut base = key.to_string();
        let mut k = 1u64;
        loop {
            let ids: Vec<String> = (0..n).map(|i| item(&base, i)).collect();
            let unique: HashSet<&String> = ids.iter().collect();
            let taken = |id: &String| self.contains(id) || open(id);
            if unique.len() == ids.len() && !self.contains(&base) && !ids.iter().any(taken) {
                // The base too: a request id asked again always gets a new incarnation, even
                // when its earlier items had other ids (`<id>#q<i>`).
                self.remember(base.clone());
                for id in ids {
                    self.remember(id);
                }
                return base;
            }
            base = format!("{key}{INCARNATION_SEP}{k}");
            k += 1;
        }
    }

    /// [`IssuedIds::fresh`] for a single item whose id is its base.
    pub fn fresh_one(&mut self, key: &str, open: impl Fn(&str) -> bool) -> String {
        self.fresh(key, 1, |b, _| b.to_string(), open)
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.set.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_id_asked_again_gets_a_new_incarnation() {
        let mut ids = IssuedIds::default();
        assert_eq!(ids.fresh_one("r", |_| false), "r");
        assert_eq!(ids.fresh_one("r", |_| false), "r~1");
        assert_eq!(ids.fresh_one("r", |_| false), "r~2");
        // A runtime id that looks like an incarnation never takes one handed out.
        assert_eq!(ids.fresh_one("r~1", |_| false), "r~1~1");
        // Open ids are never handed out, even once forgotten.
        let mut small = IssuedIds::new(1);
        assert_eq!(small.fresh_one("a", |_| false), "a");
        assert_eq!(small.fresh_one("b", |_| false), "b");
        assert_eq!(small.len(), 1);
        assert_eq!(small.fresh_one("a", |id| id == "a"), "a~1");
    }

    #[test]
    fn groups_avoid_every_item_id() {
        let mut ids = IssuedIds::default();
        let item = |b: &str, i: usize| format!("{b}#q{i}");
        assert_eq!(ids.fresh_one("x#q1", |_| false), "x#q1");
        // `x` with two questions would reuse `x#q1`.
        assert_eq!(ids.fresh("x", 2, item, |_| false), "x~1");
        assert!(ids.contains("x~1#q0") && ids.contains("x~1#q1"));
        // Asked again as a single item: a new base, although `x~1` itself named no item.
        assert_eq!(ids.fresh_one("x~1", |_| false), "x~1~1");
        // `x` itself was never handed out.
        assert_eq!(ids.fresh_one("x", |_| false), "x");
        assert_eq!(ids.fresh_one("x", |_| false), "x~2");
    }
}

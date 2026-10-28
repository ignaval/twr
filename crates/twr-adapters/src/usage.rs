//! What the adapters share for usage tracking ([`twr_core::usage`]): when to read a runtime's
//! usage again ([`Reads`]), its timestamps, the `runtime.limits` event, and the limit a failed
//! turn ended on ([`Limit`], `ext.twr.limit`).
//!
//! A read never starts a model turn: Claude Code's `get_usage` control request, Codex's
//! `account/rateLimits/read` and Grok Build's `_x.ai/billing` ask the provider's usage
//! endpoint. An adapter reads when its session is up and again when a turn ends, once what it
//! last read or heard is [`REFRESH_AFTER`] old.

use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::{json, Value};
use twr_core::event::{EventBody, RuntimeLimits};
use twr_core::usage::{UsageWindow, REFRESH_AFTER};

/// When a session's usage is due to be read again.
#[derive(Debug, Clone, Default)]
pub struct Reads {
    /// When the usage was last asked for or heard about.
    last: Option<DateTime<Utc>>,
    /// The windows known to exist: those of the last answered read, and any the runtime's own
    /// reports named since. A report of the runtime's own refreshes the data only when it
    /// covers all of them.
    windows: Vec<String>,
    /// Reads asked so far (the request ids' counter).
    asked: u64,
    /// The runtime cannot answer (an older version, an API key login): never asked again.
    off: bool,
}

impl Reads {
    /// Whether to read at `now`: never read yet, or the last read or report is
    /// [`REFRESH_AFTER`] old (an unanswered read is asked again then).
    pub fn due(&self, now: DateTime<Utc>) -> bool {
        !self.off && self.last.is_none_or(|t| now - t >= REFRESH_AFTER)
    }

    /// A read is asked at `now`; returns its number (1, 2, ...).
    pub fn ask(&mut self, now: DateTime<Utc>) -> u64 {
        self.last = Some(now);
        self.asked += 1;
        self.asked
    }

    /// A read was answered with `windows`: known from now on. One it leaves out stays known
    /// (a runtime leaves out a window it knows nothing of at the moment, not one that is
    /// gone).
    pub fn answered(&mut self, windows: &[UsageWindow]) {
        for w in windows {
            if !self.windows.contains(&w.window) {
                self.windows.push(w.window.clone());
            }
        }
    }

    /// The runtime reported `windows` on its own at `now`: no read is due before
    /// [`REFRESH_AFTER`], unless the report leaves out a window known to exist (a sparse
    /// report says nothing about those, and they would go stale). A window it names is known
    /// from now on.
    pub fn heard(&mut self, now: DateTime<Utc>, windows: &[UsageWindow]) {
        let covers = self
            .windows
            .iter()
            .all(|id| windows.iter().any(|w| &w.window == id));
        if !windows.is_empty() && covers {
            self.last = Some(now);
        }
        for w in windows {
            if !self.windows.contains(&w.window) {
                self.windows.push(w.window.clone());
            }
        }
    }

    /// The runtime cannot answer reads: none is asked again.
    pub fn stop(&mut self) {
        self.off = true;
    }

    /// Whether reads were stopped.
    pub fn stopped(&self) -> bool {
        self.off
    }
}

/// A Unix timestamp in seconds (Claude's `resetsAt`, Codex's `resetsAt`). A value too large to
/// be seconds is taken as milliseconds, in case a runtime ever switches.
pub fn epoch(v: &Value) -> Option<DateTime<Utc>> {
    let n = v.as_i64().or_else(|| v.as_f64().map(|f| f as i64))?;
    if n <= 0 {
        return None;
    }
    if n > 100_000_000_000 {
        DateTime::from_timestamp_millis(n)
    } else {
        DateTime::from_timestamp(n, 0)
    }
}

/// An RFC 3339 timestamp (Claude's `get_usage` `resets_at`, Grok's period `end`).
pub fn rfc3339(v: &Value) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(v.as_str()?)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

/// A percentage, if `v` is a finite number.
pub fn pct(v: &Value) -> Option<f64> {
    v.as_f64().filter(|f| f.is_finite())
}

/// The `runtime.limits` event for `windows` read by `source` (`event` or `read`).
pub fn limits(windows: Vec<UsageWindow>, source: &str, plan: Option<String>) -> EventBody {
    EventBody::RuntimeLimits(RuntimeLimits {
        windows,
        source: source.into(),
        plan: plan.filter(|p| !p.is_empty()),
        ..Default::default()
    })
}

/// Key of [`Limit`] in a failed `turn.ended`'s `ext.twr`.
pub const LIMIT_KEY: &str = "limit";

/// What kind of limit a turn ended on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitKind {
    /// A usage window is used up: the account can work again when it resets.
    Usage,
    /// Credits or a spending cap ran out: it needs the user.
    Spend,
}

impl LimitKind {
    /// Wire name (`usage` / `spend`).
    pub fn as_str(self) -> &'static str {
        match self {
            LimitKind::Usage => "usage",
            LimitKind::Spend => "spend",
        }
    }
}

/// The usage or spending limit a failed turn ended on (`ext.twr.limit`). A turn that carries
/// one is not transient: retrying before the reset cannot help.
#[derive(Debug, Clone, PartialEq)]
pub struct Limit {
    /// Usage or spend.
    pub kind: LimitKind,
    /// The window used up ([`twr_core::usage::FIVE_HOUR`], Claude's `seven_day_opus`, ...),
    /// when the runtime says.
    pub window: Option<String>,
    /// When the window resets, when the runtime says.
    pub resets_at: Option<DateTime<Utc>>,
    /// The runtime's own words.
    pub message: String,
}

impl Limit {
    /// `{"kind", "window"?, "resets_at"? (RFC 3339), "message"}`.
    pub fn to_value(&self) -> Value {
        let mut v = json!({ "kind": self.kind.as_str(), "message": self.message });
        if let Some(w) = &self.window {
            v["window"] = json!(w);
        }
        if let Some(t) = self.resets_at {
            v["resets_at"] = json!(t.to_rfc3339_opts(SecondsFormat::Secs, true));
        }
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeDelta;
    use serde_json::json;

    #[test]
    fn reads_are_due_once_the_last_is_old() {
        let t0 = DateTime::from_timestamp(1_000_000, 0).unwrap();
        let mut r = Reads::default();
        assert!(r.due(t0));
        assert_eq!(r.ask(t0), 1);
        assert!(!r.due(t0 + TimeDelta::minutes(9)));
        assert!(r.due(t0 + REFRESH_AFTER));
        let w = |id: &str| UsageWindow {
            window: id.into(),
            ..Default::default()
        };
        r.heard(t0 + TimeDelta::minutes(9), &[w("five_hour")]);
        assert!(!r.due(t0 + TimeDelta::minutes(10)));
        assert!(r.due(t0 + TimeDelta::minutes(19)));
        // Once a read had two windows, a report of one of them is not fresh data.
        r.answered(&[w("five_hour"), w("seven_day")]);
        r.heard(t0 + TimeDelta::minutes(15), &[w("seven_day")]);
        assert!(r.due(t0 + TimeDelta::minutes(19)));
        r.heard(
            t0 + TimeDelta::minutes(15),
            &[w("seven_day"), w("five_hour")],
        );
        assert!(!r.due(t0 + TimeDelta::minutes(19)));
        r.heard(t0 + TimeDelta::minutes(30), &[]);
        assert!(r.due(t0 + TimeDelta::minutes(25)));
        // A window only a report named counts too: a read had the weekly one alone, a report
        // named both, and reports of the weekly one alone no longer put reads off.
        let mut r = Reads::default();
        r.ask(t0);
        r.answered(&[w("seven_day")]);
        r.heard(
            t0 + TimeDelta::minutes(5),
            &[w("five_hour"), w("seven_day")],
        );
        assert!(!r.due(t0 + TimeDelta::minutes(14)));
        r.heard(t0 + TimeDelta::minutes(14), &[w("seven_day")]);
        assert!(r.due(t0 + TimeDelta::minutes(15)));
        // A read that leaves a known window out (Codex's null primary) does not forget it.
        r.ask(t0 + TimeDelta::minutes(15));
        r.answered(&[w("seven_day")]);
        r.heard(t0 + TimeDelta::minutes(20), &[w("seven_day")]);
        assert!(r.due(t0 + TimeDelta::minutes(25)));
        r.stop();
        assert!(!r.due(t0 + TimeDelta::days(1)));
    }

    #[test]
    fn limit_shape() {
        let l = Limit {
            kind: LimitKind::Usage,
            window: Some("five_hour".into()),
            resets_at: DateTime::from_timestamp(1791559200, 0),
            message: "hit".into(),
        };
        assert_eq!(
            l.to_value(),
            json!({"kind": "usage", "window": "five_hour",
                   "resets_at": "2026-10-09T15:20:00Z", "message": "hit"})
        );
        let s = Limit {
            kind: LimitKind::Spend,
            window: None,
            resets_at: None,
            message: "cap".into(),
        };
        assert_eq!(s.to_value(), json!({"kind": "spend", "message": "cap"}));
    }

    #[test]
    fn timestamps() {
        assert_eq!(epoch(&json!(1791976804)).unwrap().timestamp(), 1791976804);
        assert_eq!(
            epoch(&json!(1791976804000_i64)).unwrap().timestamp(),
            1791976804
        );
        assert_eq!(epoch(&json!(0)), None);
        assert_eq!(epoch(&json!("1791976804")), None);
        assert_eq!(
            rfc3339(&json!("2026-10-09T15:20:00.055826+00:00"))
                .unwrap()
                .timestamp(),
            1791559200
        );
        assert_eq!(rfc3339(&json!(null)), None);
        assert_eq!(pct(&json!(22)), Some(22.0));
        assert_eq!(pct(&json!(null)), None);
    }
}

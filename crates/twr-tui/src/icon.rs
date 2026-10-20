//! The working icon: the cursor of the landing page's `twr_` alone, one cell, on whatever is
//! running a turn right now (the main pane's header while the orchestrator or the thread dived
//! into works, worker cards, grid tiles) and where the main pane's next text will appear
//! ([`cursor`]). It breathes from the underscore up to a block and back ([`FRAMES`]); with
//! `[tui] animate = false` it stays an underscore.
//!
//! One clock drives every icon: the frame is a function of the time since the TUI started
//! ([`frame_at`]), so all icons show the same frame, and the IO loop wakes for the next frame
//! only while the last draw showed a moving icon ([`next_tick`]). Nothing working, nothing
//! ticks.

use std::time::{Duration, Instant};

use ratatui::style::{Color, Style};
use ratatui::text::Span;
use twr_core::reduce::{ThreadStatus, ThreadView};

/// The cursor's frames: the underscore, then lower blocks rising to three quarters of the
/// cell and falling back. Frame 0 is the still marker.
pub const FRAMES: [char; 12] = ['_', '▁', '▂', '▃', '▄', '▅', '▆', '▅', '▄', '▃', '▂', '▁'];

/// How long each frame shows (a breath is [`FRAMES`] long: 1.5 s).
pub const FRAME_INTERVAL: Duration = Duration::from_millis(125);

/// The cursor's color: the landing page's orange (`#e88a4f`), as the 256-color palette has
/// it (`#d7875f`), which reads on dark and light backgrounds alike.
pub const ACCENT: Color = Color::Indexed(173);

/// Cells the icon takes: the cursor's one.
pub const WIDTH: u16 = 1;

/// Whether `t` is working: a turn or background tasks run, with nothing blocking on the user. A thread
/// waiting on a permission or a question, idle, done, failed, parked or attached is not.
pub fn is_working(t: &ThreadView) -> bool {
    matches!(t.status(), ThreadStatus::Running | ThreadStatus::Background)
}

/// The frame to show `elapsed` after the clock started.
pub fn frame_at(elapsed: Duration) -> usize {
    let n = elapsed.as_nanos() / FRAME_INTERVAL.as_nanos();
    (n % FRAMES.len() as u128) as usize
}

/// When the IO loop must draw again for the icon: the next frame boundary after `now` on the
/// clock started at `epoch`, while the last draw showed a moving icon (`animated`); `None`
/// (no timer) otherwise.
pub fn next_tick(animated: bool, epoch: Instant, now: Instant) -> Option<Instant> {
    if !animated {
        return None;
    }
    let n = now.saturating_duration_since(epoch).as_nanos() / FRAME_INTERVAL.as_nanos();
    let next = u32::try_from(n + 1).unwrap_or(u32::MAX);
    Some(epoch + FRAME_INTERVAL.saturating_mul(next))
}

/// The cursor at `frame` (`None`: the still `_`), in [`ACCENT`]: [`WIDTH`] cells. It leads
/// the status of a working card or tile and the main pane's header while the thread it shows
/// works, and waits where that thread's next text will appear.
pub fn cursor(frame: Option<usize>) -> Span<'static> {
    let c = FRAMES[frame.unwrap_or(0) % FRAMES.len()];
    Span::styled(c.to_string(), Style::default().fg(ACCENT))
}

/// The icon's text at `frame` ([`cursor`]).
pub fn text(frame: Option<usize>) -> String {
    cursor(frame).content.into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edit::width;
    use twr_core::event::{AttentionReason, TurnResult};
    use twr_core::reduce::LastTurn;

    #[test]
    fn frames_follow_the_clock() {
        let ms = Duration::from_millis;
        assert_eq!(frame_at(Duration::ZERO), 0);
        assert_eq!(frame_at(ms(124)), 0);
        assert_eq!(frame_at(ms(125)), 1);
        assert_eq!(frame_at(ms(6 * 125 + 10)), 6, "the top of the breath");
        assert_eq!(frame_at(ms(11 * 125)), 11);
        assert_eq!(frame_at(ms(12 * 125)), 0, "a breath is 12 frames");
        assert_eq!(
            frame_at(ms(1_000 * 1_500 + 250)),
            2,
            "no drift after a long run"
        );
        assert_eq!(FRAMES[0], '_', "frame 0 is the still marker");
        // Every frame is as wide: the layout never moves.
        for f in 0..FRAMES.len() {
            assert_eq!(width(&text(Some(f))), usize::from(WIDTH), "frame {f}");
        }
        assert_eq!(text(None), "_");
        assert_eq!(text(Some(6)), "▆");
        assert_eq!(cursor(Some(3)).style.fg, Some(ACCENT));
    }

    #[test]
    fn ticks_land_on_frame_boundaries_and_only_while_animated() {
        let epoch = Instant::now();
        let ms = Duration::from_millis;
        assert_eq!(next_tick(false, epoch, epoch + ms(10)), None);
        assert_eq!(next_tick(true, epoch, epoch), Some(epoch + ms(125)));
        assert_eq!(
            next_tick(true, epoch, epoch + ms(10)),
            Some(epoch + ms(125))
        );
        // A late wakeup aims at the next boundary of the same clock, not 125 ms from now.
        assert_eq!(
            next_tick(true, epoch, epoch + ms(260)),
            Some(epoch + ms(375))
        );
        let t = next_tick(true, epoch, epoch + ms(375)).unwrap();
        assert_eq!(t, epoch + ms(500));
        assert_eq!(frame_at(t - epoch), 4);
    }

    #[test]
    fn working_is_a_running_turn_nothing_blocks() {
        let mut t = ThreadView::new("t1");
        assert!(!is_working(&t), "idle");
        t.current_turn = Some(1);
        assert!(is_working(&t), "running");
        t.open_attention
            .insert("a1".into(), AttentionReason::Stalled);
        assert!(is_working(&t), "a stall does not block the turn");
        t.open_attention
            .insert("a2".into(), AttentionReason::Permission);
        assert!(!is_working(&t), "waiting on a permission");
        t.open_attention.remove("a2");
        t.open_attention
            .insert("a3".into(), AttentionReason::Question);
        assert!(!is_working(&t), "waiting on a question");
        t.open_attention.clear();
        t.attached = Some("cli".into());
        assert!(!is_working(&t), "attached");
        t.attached = None;
        t.parked = true;
        assert!(!is_working(&t), "parked");
        t.parked = false;
        t.current_turn = None;
        t.last_turn = Some(LastTurn {
            turn: 1,
            result: TurnResult::Error,
            retry_of: None,
            withdrawn: false,
            interrupted: false,
        });
        assert!(!is_working(&t), "failed");
        t.last_turn = Some(LastTurn {
            turn: 1,
            result: TurnResult::Completed,
            retry_of: None,
            withdrawn: false,
            interrupted: false,
        });
        t.reported = true;
        assert!(!is_working(&t), "done");
    }
}

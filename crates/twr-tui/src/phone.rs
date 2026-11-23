//! `/phone`: pairing a phone from the TUI, as `twr phone` does. The view runs `twr phone
//! --json` (which starts the gateway and opens the invitation) and shows what it prints: a QR
//! code, the link, the code, the addresses and the expiry. Esc cancels the invitation. The code
//! stays in this view: never in the input, the history or anything sent to the hub.
use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Clear, Paragraph, Wrap},
    Frame,
};
use serde_json::Value;
use twr_core::slash::PHONE_LEVELS;

/// The view's state. Private display state: its `Debug` shows nothing of it.
#[derive(Clone, PartialEq)]
pub enum PhonePanel {
    /// Choosing the access (an index into [`PHONE_LEVELS`]).
    Choose(usize),
    /// Starting the gateway and opening an invitation with this access.
    Preparing(String),
    /// The invitation: the access and `twr phone --json`'s first line.
    Showing { level: String, line: Value },
    /// How it ended, or what went wrong; any key closes.
    Note(String),
}

impl std::fmt::Debug for PhonePanel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PhonePanel(<private>)")
    }
}

/// What each access level grants, for the choice.
pub fn describe(level: &str) -> &'static str {
    match level {
        "read-only" => "read threads and requests",
        "terminal" => "operate, and remote shells, attach and jobs",
        "admin" => "terminal, and accounts, boxes, configuration and modes",
        _ => "read, send, answer and start threads",
    }
}

/// What the view shows for a result of the `twr phone --json` it ran (`phone.local`): the
/// invitation, how it ended, or an error. `None` closes the view (paired, or cancelled), with
/// the status to show.
pub fn apply(panel: &PhonePanel, result: &Value) -> (Option<PhonePanel>, Option<String>) {
    if let Some(line) = result.get("line") {
        let level = match panel {
            PhonePanel::Preparing(level) => level.clone(),
            _ => "operate".into(),
        };
        return (
            Some(PhonePanel::Showing {
                level,
                line: line.clone(),
            }),
            None,
        );
    }
    let outcome = &result["outcome"];
    if let Some(device) = outcome.get("paired") {
        let name = device["name"].as_str().unwrap_or("a phone");
        return (None, Some(format!("paired {name}")));
    }
    if outcome.get("cancelled").is_some() {
        return (None, None);
    }
    let note = if outcome.get("expired").is_some() {
        "The code expired or was used up. /phone makes a new one.".to_string()
    } else if outcome.get("replaced").is_some() {
        "A newer invitation replaced this one.".to_string()
    } else {
        format!(
            "Could not pair a phone: {}",
            result["error"].as_str().unwrap_or("twr phone ended")
        )
    };
    (Some(PhonePanel::Note(note)), None)
}

/// The view over `area`.
pub fn render(frame: &mut Frame, area: Rect, panel: &PhonePanel) {
    frame.render_widget(Clear, area);
    let block = Block::bordered().title("Pair a phone");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let text = |lines: Vec<Line<'static>>| Paragraph::new(lines).wrap(Wrap { trim: false });
    match panel {
        PhonePanel::Choose(selected) => {
            let mut lines = vec![Line::from("Access for the new phone:"), Line::from("")];
            for (i, level) in PHONE_LEVELS.iter().enumerate() {
                let style = if i == *selected {
                    Style::default().add_modifier(Modifier::REVERSED)
                } else {
                    Style::default()
                };
                lines.push(Line::from(vec![
                    Span::styled(format!(" {} {level} ", i + 1), style),
                    Span::raw(format!("  {}", describe(level))),
                ]));
            }
            lines.push(Line::from(""));
            lines.push(Line::from("Enter pairs · ↑↓ or 1–4 choose · Esc closes"));
            frame.render_widget(text(lines), inner);
        }
        PhonePanel::Preparing(_) => frame.render_widget(
            text(vec![
                Line::from("Starting the phone gateway and opening an invitation…"),
                Line::from(""),
                Line::from("Esc cancels."),
            ]),
            inner,
        ),
        PhonePanel::Note(note) => frame.render_widget(
            text(vec![
                Line::from(note.clone()),
                Line::from(""),
                Line::from("Any key closes."),
            ]),
            inner,
        ),
        PhonePanel::Showing { level, line } => render_invitation(frame, inner, level, line),
    }
}

/// The invitation: its QR code when the view has room, then what to type or tap.
fn render_invitation(frame: &mut Frame, area: Rect, level: &str, line: &Value) {
    let code = twr_core::phone_pairing::display_code(line["code"].as_str().unwrap_or(""));
    let addresses: Vec<String> = line["invitation"]["routes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| r.as_str()?.strip_prefix("tls:").map(String::from))
        .collect();
    let until = chrono::DateTime::from_timestamp(line["expires_at"].as_i64().unwrap_or(0), 0)
        .map(|t| t.with_timezone(&chrono::Local).format("%H:%M").to_string())
        .unwrap_or_default();
    let details = vec![
        Line::from(vec![
            Span::raw("Code: "),
            Span::styled(code, Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(format!("   access: {level}")),
        ]),
        Line::from(format!("Addresses: {}", addresses.join("  "))),
        Line::from(format!("Link: {}", line["link"].as_str().unwrap_or(""))),
        Line::from(format!(
            "One use, until {until}. Esc cancels · c copies the link"
        )),
    ];
    let qr = crate::qr::invitation_text(&line["invitation"]);
    let rows = crate::qr::half_blocks(&qr).unwrap_or_default();
    let side = rows.first().map_or(0, |r| r.chars().count()) as u16;
    // The details take about six rows with the link wrapped.
    let room = area.width >= side && area.height >= rows.len() as u16 + 6;
    let mut y = area.y;
    if room && side > 0 {
        let left = area.x + (area.width - side) / 2;
        let paint = Style::default().fg(Color::Black).bg(Color::White);
        let lines: Vec<Line> = rows
            .into_iter()
            .map(|r| Line::from(Span::styled(r, paint)))
            .collect();
        let height = lines.len() as u16;
        frame.render_widget(Paragraph::new(lines), Rect::new(left, y, side, height));
        y += height;
        let hint = "Scan it in the twr app: Pair a hub › Scan QR code.";
        frame.render_widget(Paragraph::new(hint), Rect::new(area.x, y, area.width, 1));
        y += 1;
    } else {
        let note = format!(
            "Too small for the QR code ({side} columns, {} rows would do): type the code and an \
             address, or open the link on the phone.",
            side.div_ceil(2) + 6
        );
        let height = 2.min(area.height);
        frame.render_widget(
            Paragraph::new(note).wrap(Wrap { trim: false }),
            Rect::new(area.x, y, area.width, height),
        );
        y += height;
    }
    let rest = Rect::new(area.x, y, area.width, area.bottom().saturating_sub(y));
    frame.render_widget(Paragraph::new(details).wrap(Wrap { trim: false }), rest);
}

/// The arguments of the `twr phone` the view runs for `level`.
pub fn arguments(level: &str) -> Vec<&'static str> {
    let mut args = vec!["phone", "--json"];
    match level {
        "read-only" => args.push("--read-only"),
        "terminal" => args.push("--terminal"),
        "admin" => args.push("--admin"),
        _ => {}
    }
    args
}

/// Sends SIGTERM to a `twr phone` still running when dropped (the TUI quit, or the view was
/// closed): it then cancels its invitation itself.
struct Term(Option<u32>);
impl Drop for Term {
    fn drop(&mut self) {
        if let Some(pid) = self.0.and_then(|p| i32::try_from(p).ok()) {
            // SAFETY: a signal to our own child, not yet reaped (the pid is still its own).
            unsafe {
                libc::kill(pid, libc::SIGTERM);
            }
        }
    }
}

/// Runs `twr phone --json` for `level` until it ends or `stop` fires (then it is told to stop,
/// and cancels the invitation): each line it prints goes to `send` as a `phone.local` result
/// (`{"line": …}` for the invitation, `{"outcome": …}` for its end), else `{"error": …}`.
pub async fn run(level: String, send: impl Fn(Value), stop: tokio::sync::oneshot::Receiver<()>) {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt};
    let program = match std::env::current_exe() {
        Ok(program) => program,
        Err(e) => return send(serde_json::json!({"error": e.to_string()})),
    };
    let child = tokio::process::Command::new(program)
        .args(arguments(&level))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn();
    let mut child = match child {
        Ok(child) => child,
        Err(e) => return send(serde_json::json!({"error": e.to_string()})),
    };
    let mut term = Term(child.id());
    let (Some(stdout), Some(mut stderr)) = (child.stdout.take(), child.stderr.take()) else {
        return;
    };
    let errors = tokio::spawn(async move {
        let mut text = String::new();
        let _ = (&mut stderr)
            .take(16 * 1024)
            .read_to_string(&mut text)
            .await;
        text
    });
    let mut lines = tokio::io::BufReader::new(stdout).lines();
    let mut ended = false;
    tokio::pin!(stop);
    let mut stopping = false;
    loop {
        tokio::select! {
            _ = &mut stop, if !stopping => {
                stopping = true;
                drop(Term(term.0));
            }
            line = lines.next_line() => match line {
                Ok(Some(line)) => {
                    let Ok(value) = serde_json::from_str::<Value>(&line) else { continue };
                    if value.get("code").is_some() {
                        send(serde_json::json!({"line": value}));
                    } else {
                        ended = true;
                        send(serde_json::json!({"outcome": value}));
                    }
                }
                _ => break,
            },
        }
    }
    let status = child.wait().await;
    term.0 = None;
    if !ended && !stopping {
        let text = errors.await.unwrap_or_default();
        let why = text
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .map(|l| l.trim_start_matches("twr: ").to_string())
            .unwrap_or_else(|| format!("twr phone ended ({status:?})"));
        send(serde_json::json!({"error": why}));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line() -> Value {
        serde_json::json!({
            "code": "ABCDEFGHJK23", "expires_at": 1_763_900_000u64, "scopes": ["read", "operate"],
            "gateway_id": "8b0f5c7e-2a44-4b8e-9d21-6a4e1c3f7b90",
            "link": "twr://pair#eyJ0eXBlIjoidHdyLXBhaXIifQ",
            "invitation": {"type": "twr-pair", "version": 2,
                "id": "8b0f5c7e-2a44-4b8e-9d21-6a4e1c3f7b90", "name": "box",
                "pin": "m5zL1y2tJm0W4cTGsl3xn0Hc6aM1yhsAa0Tq3hdyKdM",
                "iroh": "ae58ff8833241ac82d6ff7611046ed67b5072d142c588d0063e942d9a75502b6",
                "code": "ABCDEFGHJK23", "expiresAt": 1_763_900_000_000u64,
                "routes": ["tls:192.168.1.20:47321", "tls:[2001:db8::5]:47321"],
                "scopes": ["read", "operate"]}
        })
    }

    fn drawn(panel: &PhonePanel, width: u16, height: u16) -> ratatui::buffer::Buffer {
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| render(f, f.area(), panel)).unwrap();
        terminal.backend().buffer().clone()
    }

    fn text(buffer: &ratatui::buffer::Buffer) -> String {
        buffer.content().iter().map(|c| c.symbol()).collect()
    }

    /// With room, the QR code is drawn black on white above the code and addresses; without,
    /// a note says so and the code and addresses still show.
    #[test]
    fn the_invitation_draws_its_code_when_there_is_room() {
        let panel = PhonePanel::Showing {
            level: "operate".into(),
            line: line(),
        };
        let wide = drawn(&panel, 100, 60);
        let painted = wide
            .content()
            .iter()
            .filter(|c| c.symbol() == "█" && c.fg == Color::Black && c.bg == Color::White)
            .count();
        assert!(painted > 50, "{painted}");
        let shown = text(&wide);
        assert!(shown.contains("ABCD-EFGH-JK23") && shown.contains("192.168.1.20:47321"));
        let small = text(&drawn(&panel, 60, 20));
        assert!(small.contains("Too small for the QR code"));
        assert!(small.contains("ABCD-EFGH-JK23"));
        assert_eq!(format!("{panel:?}"), "PhonePanel(<private>)");
    }

    #[test]
    fn results_move_the_view_on() {
        let preparing = PhonePanel::Preparing("admin".into());
        let (next, _) = apply(&preparing, &serde_json::json!({"line": line()}));
        assert!(matches!(next, Some(PhonePanel::Showing { ref level, .. }) if level == "admin"));
        let paired = serde_json::json!({"outcome": {"paired": {"name": "Pixel"}}});
        assert_eq!(
            apply(&preparing, &paired),
            (None, Some("paired Pixel".into()))
        );
        let cancelled = serde_json::json!({"outcome": {"cancelled": true}});
        assert_eq!(apply(&preparing, &cancelled), (None, None));
        let expired = serde_json::json!({"outcome": {"expired": true}});
        assert!(matches!(
            apply(&preparing, &expired).0,
            Some(PhonePanel::Note(_))
        ));
        let failed = serde_json::json!({"error": "no hub"});
        let Some(PhonePanel::Note(note)) = apply(&preparing, &failed).0 else {
            panic!()
        };
        assert!(note.contains("no hub"));
        // `twr phone`'s word on a hub from an older twr reaches the view whole.
        let older = "the twr hub that runs is from an older twr, which can't pair phones with \
                     this one: restart it (`twr hub stop`, then run twr again)";
        let Some(PhonePanel::Note(note)) =
            apply(&preparing, &serde_json::json!({"error": older})).0
        else {
            panic!()
        };
        assert!(note.ends_with(older), "{note}");
        assert_eq!(arguments("admin"), ["phone", "--json", "--admin"]);
        assert_eq!(arguments("operate"), ["phone", "--json"]);
    }
}

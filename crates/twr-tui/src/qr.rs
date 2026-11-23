//! QR codes for a terminal (`twr phone`, `/phone`): drawn with Unicode half blocks, two module
//! rows to a line and one module to a column, so a module is about square in a terminal's
//! cells, inside a light quiet zone. Draw them dark on light (black on white), which reads on
//! any terminal theme: a phone's camera wants dark modules on a light ground.

/// The light modules around a code, each side. The standard asks for 4; a screen's code reads
/// with 2, and every module saved is a column an 80-column terminal over SSH keeps.
pub const QUIET: usize = 2;

/// The modules of `text`'s code (dark is `true`), quiet zone included, at the lowest error
/// correction (a screen is not a smudged label): `None` when it does not fit any QR code.
pub fn modules(text: &str) -> Option<Vec<Vec<bool>>> {
    let code =
        qrcode::QrCode::with_error_correction_level(text.as_bytes(), qrcode::EcLevel::L).ok()?;
    let width = code.width();
    let colors = code.to_colors();
    let side = width + 2 * QUIET;
    let mut rows = vec![vec![false; side]; side];
    for (i, color) in colors.into_iter().enumerate() {
        rows[QUIET + i / width][QUIET + i % width] = color == qrcode::Color::Dark;
    }
    Some(rows)
}

/// How many columns (and modules) `text`'s code takes, quiet zone included.
pub fn width(text: &str) -> Option<usize> {
    modules(text).map(|rows| rows.len())
}

/// `text`'s code as lines of half blocks: `█` where both modules of a cell are dark, `▀` the
/// upper one only, `▄` the lower one only, a space where both are light.
pub fn half_blocks(text: &str) -> Option<Vec<String>> {
    let rows = modules(text)?;
    let light = vec![false; rows.len()];
    Some(
        rows.chunks(2)
            .map(|pair| {
                let (upper, lower) = (&pair[0], pair.get(1).unwrap_or(&light));
                upper
                    .iter()
                    .zip(lower)
                    .map(|(u, l)| match (u, l) {
                        (true, true) => '█',
                        (true, false) => '▀',
                        (false, true) => '▄',
                        (false, false) => ' ',
                    })
                    .collect()
            })
            .collect(),
    )
}

/// What a terminal's QR code of version 2 invitation `invitation` carries: its JSON without
/// the optional `iroh` and `scopes` (phones learn `iroh` over the connection), which keeps a
/// typical invitation within 80 columns. Every scanner reads the JSON; the `twr://pair#` link,
/// a third larger in base64, goes beside the code as text.
pub fn invitation_text(invitation: &serde_json::Value) -> String {
    let mut slim = invitation.clone();
    if let Some(object) = slim.as_object_mut() {
        object.remove("iroh");
        object.remove("scopes");
    }
    slim.to_string()
}

/// The QR version of `text`'s code (1 to 40).
pub fn version(text: &str) -> Option<i16> {
    let code =
        qrcode::QrCode::with_error_correction_level(text.as_bytes(), qrcode::EcLevel::L).ok()?;
    match code.version() {
        qrcode::Version::Normal(v) | qrcode::Version::Micro(v) => Some(v),
    }
}

/// Whether `text`'s code fits `columns`.
pub fn fits(text: &str, columns: usize) -> bool {
    width(text).is_some_and(|w| w <= columns)
}

/// `lines` for a terminal: each black on white, the colours reset at its end.
pub fn ansi(lines: &[String]) -> String {
    lines
        .iter()
        .map(|line| format!("\x1b[30;47m{line}\x1b[0m\n"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SNAPSHOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/src/qr_snapshot.txt");

    /// The code of a fixed link, drawn: compared with `qr_snapshot.txt` (rewritten with
    /// `TWR_WRITE_SNAPSHOT=1`).
    #[test]
    fn a_fixed_link_draws_as_its_snapshot() {
        let link = "twr://pair#eyJ0eXBlIjoidHdyLXBhaXIiLCJ2ZXJzaW9uIjoyfQ";
        let lines = half_blocks(link).unwrap();
        let drawn = lines.join("\n") + "\n";
        if std::env::var_os("TWR_WRITE_SNAPSHOT").is_some() {
            std::fs::write(SNAPSHOT, &drawn).unwrap();
        }
        assert_eq!(std::fs::read_to_string(SNAPSHOT).unwrap(), drawn);
        // Square: as many columns as module rows, two rows a line.
        let side = width(link).unwrap();
        assert_eq!(lines.len(), side.div_ceil(2));
        assert!(lines.iter().all(|l| l.chars().count() == side));
        // The quiet zone: the first line and the first columns are light.
        assert!(lines[0].chars().all(|c| c == ' '));
        assert!(lines.iter().all(|l| l.starts_with("  ")));
        assert_eq!(
            ansi(&lines[..1]),
            format!("\x1b[30;47m{}\x1b[0m\n", lines[0])
        );
    }

    /// A three-route invitation, without `iroh` and `scopes`, fits 80 columns: 282 bytes, version
    /// 11 at low error correction, 61 modules and 65 columns with the quiet zone (in full: 384
    /// bytes, version 13, 73 columns). Its link would not (version 16, 85 columns).
    #[test]
    fn a_three_route_invitation_fits_80_columns() {
        let json = r#"{"type":"twr-pair","version":2,"id":"8b0f5c7e-2a44-4b8e-9d21-6a4e1c3f7b90","name":"box","pin":"m5zL1y2tJm0W4cTGsl3xn0Hc6aM1yhsAa0Tq3hdyKdM","iroh":"ae58ff8833241ac82d6ff7611046ed67b5072d142c588d0063e942d9a75502b6","code":"ABCDEFGHJK23","expiresAt":1763900000000,"routes":["tls:192.168.1.20:47321","tls:[2001:db8::5]:47321","tls:box.tail1234.ts.net:47321"],"scopes":["read","operate"]}"#;
        let invitation: serde_json::Value = serde_json::from_str(json).unwrap();
        let text = invitation_text(&invitation);
        assert!(!text.contains("iroh") && !text.contains("scopes"));
        let parsed = twr_core::phone_pairing::Invitation::parse(&text).unwrap();
        assert_eq!(parsed.iroh, None);
        assert_eq!(parsed.routes.len(), 3);
        assert_eq!(
            (text.len(), version(&text), width(&text)),
            (282, Some(11), Some(65))
        );
        assert!(fits(&text, 80) && !fits(&text, 64));
        // In full, and as a link, for comparison.
        assert_eq!(
            (json.len(), version(json), width(json)),
            (384, Some(13), Some(73))
        );
        let link = format!(
            "twr://pair#{}",
            twr_core::phone_pairing::b64url_encode(json.as_bytes())
        );
        assert_eq!(
            (link.len(), version(&link), width(&link)),
            (523, Some(16), Some(85))
        );
        assert!(!fits(&link, 80));
        assert!(modules(&"x".repeat(4000)).is_none());
    }
}

//! The service text `twr speak --print-service` prints, to paste: a LaunchAgent (macOS) or a
//! systemd user unit (Linux) that keeps `twr speak` running with the same arguments. twr
//! installs neither itself.
//!
//! Every value is escaped for where it goes: XML in the plist ([`xml_escape`]), systemd's own
//! quoting in `ExecStart=` ([`systemd_word`]). A value that can't be written there safely (a
//! control character; in a unit also `"` or `\`) is refused rather than mangled.

use anyhow::anyhow;

/// The LaunchAgent's label (and its plist's name, `<label>.plist`).
pub const AGENT_LABEL: &str = "tools.twr.speak";

/// The systemd user unit's file name.
pub const UNIT_NAME: &str = "twr-speak.service";

/// Whether XML 1.0 can hold `s` (no control character but tab, newline and carriage return; no
/// non-character).
fn xml_representable(s: &str) -> bool {
    s.chars().all(|c| {
        matches!(c, '\t' | '\n' | '\r' | '\u{20}'..='\u{D7FF}' | '\u{E000}'..='\u{FFFD}')
            || c >= '\u{10000}'
    })
}

/// `s` escaped for XML text: `&`, `<`, `>`, `"`, `'` and a carriage return.
pub fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            '\r' => out.push_str("&#13;"),
            c => out.push(c),
        }
    }
    out
}

/// `w` as one word of a systemd unit's `ExecStart=`: `%` as `%%` (a specifier otherwise), and
/// in an argument (not the executable, which systemd doesn't expand) `$` as `$$`; in double
/// quotes when it holds a space, `'` or `;`. `None` for a word with `"`, `\` or a control
/// character, or an empty one.
pub fn systemd_word(w: &str, executable: bool) -> Option<String> {
    if w.is_empty() || w.chars().any(|c| c.is_control() || c == '"' || c == '\\') {
        return None;
    }
    let mut w = w.replace('%', "%%");
    if !executable {
        w = w.replace('$', "$$");
    }
    Some(if w.contains([' ', '\'', ';']) {
        format!("\"{w}\"")
    } else {
        w
    })
}

/// The LaunchAgent that keeps `exe args...` (`twr speak ...`) running in your session: started
/// at load and kept alive, interactive (audio), a umask of 077, its stderr in
/// `<home>/Library/Logs/twr-speak.log`.
pub fn launchd_plist(exe: &str, args: &[String], home: &str) -> anyhow::Result<String> {
    let s = |v: &str| {
        if xml_representable(v) && !v.chars().any(char::is_control) {
            Ok(format!("<string>{}</string>", xml_escape(v)))
        } else {
            Err(anyhow!("{v:?} can't be written in a plist"))
        }
    };
    let mut argv = format!("    {}\n", s(exe)?);
    for a in args {
        argv.push_str(&format!("    {}\n", s(a)?));
    }
    let log = format!("{}/Library/Logs/twr-speak.log", home.trim_end_matches('/'));
    Ok(format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
         \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n\
         <dict>\n\
         \x20 <key>Label</key>{label}\n\
         \x20 <key>ProgramArguments</key>\n\
         \x20 <array>\n\
         {argv}\
         \x20 </array>\n\
         \x20 <key>RunAtLoad</key><true/>\n\
         \x20 <key>KeepAlive</key><true/>\n\
         \x20 <key>ProcessType</key><string>Interactive</string>\n\
         \x20 <key>Umask</key><integer>63</integer>\n\
         \x20 <key>StandardErrorPath</key>{log}\n\
         </dict>\n\
         </plist>\n",
        label = s(AGENT_LABEL)?,
        log = s(&log)?,
    ))
}

/// The systemd user unit that keeps `exe args...` (`twr speak ...`) running in your session,
/// restarted 10 s after it ends.
pub fn systemd_unit(exe: &str, args: &[String]) -> anyhow::Result<String> {
    let word = |w: &str, executable| {
        systemd_word(w, executable).ok_or_else(|| anyhow!("{w:?} can't be written in a unit"))
    };
    let mut exec = vec![word(exe, true)?];
    for a in args {
        exec.push(word(a, false)?);
    }
    Ok(format!(
        "[Unit]\n\
         Description=twr speaker: reads twr's messages aloud\n\
         \n\
         [Service]\n\
         ExecStart={exec}\n\
         Restart=always\n\
         RestartSec=10\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        exec = exec.join(" "),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_unit_quotes_for_systemd() {
        let unit = systemd_unit(
            "/home/me/.local/bin/twr",
            &args(&["speak", "--ssh", "me@box", "--name", "my mac; 100% $HOME"]),
        )
        .unwrap();
        assert_eq!(
            unit,
            "[Unit]\nDescription=twr speaker: reads twr's messages aloud\n\n[Service]\n\
             ExecStart=/home/me/.local/bin/twr speak --ssh me@box --name \"my mac; 100%% $$HOME\"\n\
             Restart=always\nRestartSec=10\n\n[Install]\nWantedBy=default.target\n"
        );
        for bad in ["a\"b", "a\\b", "a\nb", ""] {
            assert!(
                systemd_unit("/twr", &args(&["speak", bad])).is_err(),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn the_plist_escapes_for_xml() {
        let plist = launchd_plist(
            "/opt/twr & co/twr",
            &args(&["speak", "--name", "<Nacho's>"]),
            "/Users/me/",
        )
        .unwrap();
        assert!(plist.contains("<key>Label</key><string>tools.twr.speak</string>"));
        assert!(plist
            .contains("    <string>/opt/twr &amp; co/twr</string>\n    <string>speak</string>\n"));
        assert!(plist.contains("<string>&lt;Nacho&apos;s&gt;</string>"));
        assert!(plist.contains(
            "<key>StandardErrorPath</key><string>/Users/me/Library/Logs/twr-speak.log</string>"
        ));
        assert!(launchd_plist("/twr", &args(&["a\u{1}b"]), "/Users/me").is_err());
        assert!(launchd_plist("/twr", &args(&["a\nb"]), "/Users/me").is_err());
    }
}

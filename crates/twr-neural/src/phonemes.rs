//! Turning text into what a model reads: sentences ([`sentences`]), and for the models that
//! read phonemes (Kokoro, Piper) the `espeak-ng` program's IPA for them ([`Espeak`]), spelled
//! as the model's tokens spell it ([`for_kokoro`], [`for_piper`]).
//!
//! espeak-ng runs as a program, never linked: it is GPL, twr is Apache-2.0. The text goes to
//! its stdin, one clause a line, with fixed arguments, so a message can't name an option or a
//! file; it is killed if it takes longer than [`ESPEAK_TIMEOUT`].

use std::ffi::OsStr;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context};

/// How long espeak-ng may take for one chunk (it takes milliseconds).
pub const ESPEAK_TIMEOUT: Duration = Duration::from_secs(10);

/// The program's name on `PATH`.
pub const ESPEAK: &str = "espeak-ng";

/// Where package managers put espeak-ng outside the `PATH` an app started from the desktop
/// gets (Homebrew on Apple silicon and Intel Macs).
const ESPEAK_DIRS: &[&str] = &["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"];

/// The espeak-ng program to run: `configured` (`[speech] espeak`) when set, else `espeak-ng`
/// on `path` or in one of [`ESPEAK_DIRS`].
pub fn find_espeak(configured: &str, path: Option<&OsStr>) -> Option<PathBuf> {
    let runnable = |p: &Path| {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    };
    let configured = configured.trim();
    if !configured.is_empty() {
        let p = PathBuf::from(configured);
        return runnable(&p).then_some(p);
    }
    path.into_iter()
        .flat_map(std::env::split_paths)
        .chain(ESPEAK_DIRS.iter().map(PathBuf::from))
        .filter(|d| !d.as_os_str().is_empty())
        .map(|d| d.join(ESPEAK))
        .find(|p| runnable(p))
}

/// What to do when espeak-ng is missing, for an error.
pub fn espeak_hint() -> &'static str {
    if cfg!(target_os = "macos") {
        "install espeak-ng (brew install espeak-ng) or pick a Supertonic voice"
    } else {
        "install espeak-ng (Arch: sudo pacman -S espeak-ng) or pick a Supertonic voice"
    }
}

/// `text` in sentences, each with the mark that ends it: cut after `.`, `!`, `?` or `…` that
/// a space or the end follows (`main.rs` and `3.5` stay whole).
pub fn sentences(text: &str) -> Vec<String> {
    split_after(text, &['.', '!', '?', '…'])
}

/// A sentence in clauses, each with the mark that ends it (`,`, `;`, `:` or the sentence's
/// own), the clause's text without it. Spanish's opening `¿` and `¡` go.
pub fn clauses(sentence: &str) -> Vec<(String, Option<char>)> {
    split_after(sentence, &[',', ';', ':', '.', '!', '?', '…'])
        .into_iter()
        .filter_map(|c| {
            let c = c.replace(['¿', '¡'], "");
            let c = c.trim();
            let mark = c
                .chars()
                .last()
                .filter(|m| [',', ';', ':', '.', '!', '?', '…'].contains(m));
            let body = match mark {
                Some(m) => c[..c.len() - m.len_utf8()].trim(),
                None => c,
            };
            (!body.is_empty()).then(|| (body.to_string(), mark))
        })
        .collect()
}

fn split_after(text: &str, marks: &[char]) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        cur.push(c);
        if marks.contains(&c) && chars.peek().is_none_or(|n| n.is_whitespace()) {
            // A run of marks ("?!", "...") stays with its sentence.
            while chars.peek().is_some_and(|n| marks.contains(n)) {
                cur.push(chars.next().unwrap());
            }
            if !cur.trim().is_empty() {
                out.push(cur.trim().to_string());
            }
            cur.clear();
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

/// The espeak-ng program, with the voice (`en-us`, `es`) it spells with.
#[derive(Debug, Clone)]
pub struct Espeak {
    /// The program.
    pub program: PathBuf,
}

impl Espeak {
    /// The IPA of each of `lines`, in order: one line each. Fails when the program fails or
    /// gives a different number of lines.
    pub fn ipa(&self, voice: &str, lines: &[String]) -> anyhow::Result<Vec<String>> {
        if lines.is_empty() {
            return Ok(Vec::new());
        }
        let mut input = String::new();
        for l in lines {
            // Each line its own clause: the period makes espeak-ng end it there. A line has
            // no newline of its own (the text was cut into clauses, and whitespace squashed).
            let l: String = l
                .chars()
                .map(|c| if c.is_control() { ' ' } else { c })
                .collect();
            input.push_str(l.trim());
            input.push_str(".\n");
        }
        let out = self.run(voice, &input)?;
        let got: Vec<String> = out
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect();
        if got.len() != lines.len() {
            bail!(
                "espeak-ng gave {} lines for {} clauses",
                got.len(),
                lines.len()
            );
        }
        Ok(got)
    }

    fn run(&self, voice: &str, input: &str) -> anyhow::Result<String> {
        let mut child = Command::new(&self.program)
            .args(["-q", "--ipa", "-b", "1", "-v", voice])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("running {}", self.program.display()))?;
        let mut stdin = child.stdin.take().context("espeak-ng's stdin")?;
        let input = input.to_string();
        let writer = std::thread::spawn(move || {
            let _ = stdin.write_all(input.as_bytes());
        });
        let mut stdout = child.stdout.take().context("espeak-ng's stdout")?;
        let reader = std::thread::spawn(move || {
            let mut s = Vec::new();
            let _ = stdout.read_to_end(&mut s);
            s
        });
        let start = Instant::now();
        let status = loop {
            if let Some(s) = child.try_wait()? {
                break s;
            }
            if start.elapsed() > ESPEAK_TIMEOUT {
                let _ = child.kill();
                let _ = child.wait();
                bail!("espeak-ng took longer than {ESPEAK_TIMEOUT:?}");
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        let _ = writer.join();
        let out = reader.join().map_err(|_| anyhow!("reading espeak-ng"))?;
        if !status.success() {
            let mut err = String::new();
            if let Some(mut e) = child.stderr.take() {
                let _ = e.read_to_string(&mut err);
            }
            bail!(
                "espeak-ng failed ({status}): {}",
                err.lines().next().unwrap_or("")
            );
        }
        Ok(String::from_utf8_lossy(&out).into_owned())
    }

    /// A sentence's phonemes as `spell` writes them for a model: its clauses' IPA, each with
    /// its mark, the clauses one space apart (the marks give the pauses).
    pub fn sentence(
        &self,
        voice: &str,
        sentence: &str,
        spell: impl Fn(&str) -> String,
    ) -> anyhow::Result<String> {
        let clauses = clauses(sentence);
        let texts: Vec<String> = clauses.iter().map(|(t, _)| t.clone()).collect();
        let ipa = match self.ipa(voice, &texts) {
            Ok(ipa) => ipa,
            // One clause espeak-ng read as two (an abbreviation's period): ask clause by
            // clause, joining what each gives.
            Err(_) if texts.len() > 1 => texts
                .iter()
                .map(|t| {
                    self.run(voice, &format!("{t}.\n"))
                        .map(|o| o.split_whitespace().collect::<Vec<_>>().join(" "))
                })
                .collect::<anyhow::Result<_>>()?,
            Err(e) => return Err(e),
        };
        let mut out = String::new();
        for ((_, mark), ipa) in clauses.iter().zip(ipa) {
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(&spell(&ipa));
            out.push(mark.unwrap_or('.'));
        }
        Ok(out)
    }
}

/// espeak-ng's IPA as Kokoro's tokens spell it (as misaki, Kokoro's own front end, does with
/// espeak's): for English its one-letter diphthongs (`aɪ` is `I`) and American length marks;
/// for every language the affricates as one letter, no tie bars, no glottal stops.
pub fn for_kokoro(ipa: &str, espeak_voice: &str) -> String {
    let mut s = ipa.replace(['\u{0361}', '\u{035C}'], "");
    if espeak_voice.starts_with("en") {
        for (from, to) in [
            ("ʔn\u{0329}", "tᵊn"),
            ("n\u{0329}", "ᵊn"),
            ("l\u{0329}", "ᵊl"),
            ("aɪ", "I"),
            ("aʊ", "W"),
            ("eɪ", "A"),
            ("oʊ", "O"),
            ("ɔɪ", "Y"),
            ("əʊ", "Q"),
            ("ɚ", "əɹ"),
            ("ʔ", "t"),
            ("r", "ɹ"),
            ("x", "k"),
            ("ç", "k"),
            ("ɐ", "ə"),
            ("ɬ", "l"),
            ("ʲ", ""),
        ] {
            s = s.replace(from, to);
        }
        if espeak_voice == "en-us" {
            s = s.replace("ɜː", "ɜɹ").replace('ː', "");
        }
    } else {
        s = s.replace('ʔ', "");
    }
    s.replace("dʒ", "ʤ").replace("tʃ", "ʧ")
}

/// espeak-ng's IPA as Piper's tokens spell it: as espeak-ng gives it (Piper was trained on
/// espeak-ng's phonemes), without tie bars.
pub fn for_piper(ipa: &str) -> String {
    ipa.replace(['\u{0361}', '\u{035C}'], "")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sentences_and_clauses() {
        assert_eq!(
            sentences("I reviewed main.rs. Version 3.5 passes! Done? Yes… ok"),
            vec![
                "I reviewed main.rs.",
                "Version 3.5 passes!",
                "Done?",
                "Yes…",
                "ok"
            ]
        );
        assert_eq!(sentences("Wait?! Really..."), vec!["Wait?!", "Really..."]);
        assert_eq!(
            clauses("¿Quieres que arregle ambas, ahora?"),
            vec![
                ("Quieres que arregle ambas".to_string(), Some(',')),
                ("ahora".to_string(), Some('?'))
            ]
        );
        assert_eq!(
            clauses("Two things: the script, and the sheet"),
            vec![
                ("Two things".to_string(), Some(':')),
                ("the script".to_string(), Some(',')),
                ("and the sheet".to_string(), None)
            ]
        );
        assert!(clauses(" , . ").is_empty());
    }

    #[test]
    fn kokoro_spelling() {
        assert_eq!(for_kokoro("aɪ ɹᵻvjˈuːd", "en-us"), "I ɹᵻvjˈud");
        assert_eq!(for_kokoro("tʃˈeɪndʒ", "en-us"), "ʧˈAnʤ");
        assert_eq!(for_kokoro("bˈʌʔn\u{0329}", "en-us"), "bˈʌtᵊn");
        // British keeps length.
        assert_eq!(for_kokoro("ɡˈəʊ fˈɑː", "en-gb"), "ɡˈQ fˈɑː");
        // Spanish: only the shared rules.
        assert_eq!(for_kokoro("kjˈeɾes ke arˈeɣle", "es"), "kjˈeɾes ke arˈeɣle");
        assert_eq!(for_kokoro("tʃˈiko", "es"), "ʧˈiko");
        assert_eq!(for_piper("t\u{0361}ʃ"), "tʃ");
    }

    #[test]
    fn espeak_is_found_where_configured() {
        let dir = tempfile::tempdir().unwrap();
        let prog = dir.path().join("espeak-ng");
        std::fs::write(&prog, "#!/bin/sh\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&prog, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            find_espeak(prog.to_str().unwrap(), None),
            Some(prog.clone())
        );
        assert_eq!(find_espeak("", Some(dir.path().as_os_str())), Some(prog));
        assert_eq!(find_espeak("/nonexistent/espeak-ng", None), None);
    }

    /// With the real program: a clause per line, in order.
    #[test]
    fn espeak_spells_clauses_in_order() {
        let Some(program) = find_espeak("", std::env::var_os("PATH").as_deref()) else {
            eprintln!("skipping: no espeak-ng");
            return;
        };
        let e = Espeak { program };
        let s = e
            .sentence("es", "¿Quieres que arregle ambas, ahora?", |i| {
                for_kokoro(i, "es")
            })
            .unwrap();
        assert!(s.ends_with('?') && s.contains(", "), "{s}");
        // A message can't name an option: it is text on stdin.
        let s = e
            .sentence("en-us", "--help -v es", |i| for_kokoro(i, "en-us"))
            .unwrap();
        assert!(!s.is_empty());
    }
}

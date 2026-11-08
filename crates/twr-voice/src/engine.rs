//! What recognizes: whisper.cpp's CLI on this machine, or a program of the user's.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, bail, Context};
use serde::Serialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use twr_core::config::{VoiceConfig, VoiceEngine};
use twr_speech::engine::which;

/// The model a `[voice] model` left empty means, in twr's data directory.
pub const DEFAULT_MODEL: &str = "voice/ggml-base.en.bin";
/// whisper.cpp's CLI by the names its packages give it.
pub const WHISPER_PROGRAMS: [&str; 2] = ["whisper-cli", "whisper-cpp"];
/// Most bytes kept of a recognizer's stdout, and of its stderr (for its error).
const STDOUT_MAX: usize = 64 * 1024;
const STDERR_MAX: usize = 4096;

/// A recognizer, resolved from `[voice]`.
#[derive(Debug, Clone, PartialEq)]
pub enum Kind {
    /// whisper.cpp's CLI at `program`, with `model`, for `language`.
    Whisper {
        /// The CLI.
        program: PathBuf,
        /// The ggml model file.
        model: PathBuf,
        /// `en`, `es`, ..., or `auto`.
        language: String,
    },
    /// `[voice] command`.
    Command(Vec<String>),
}

impl Kind {
    /// Its name, as `twr voice status` says it.
    pub fn name(&self) -> &'static str {
        match self {
            Kind::Whisper { .. } => "whisper",
            Kind::Command(_) => "command",
        }
    }

    /// Whether the audio stays on this machine (a command may send it anywhere).
    pub fn local(&self) -> bool {
        matches!(self, Kind::Whisper { .. })
    }
}

/// What `twr voice status` says.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Status {
    /// Whether a recognizer is there.
    pub available: bool,
    /// Its name (`whisper`, `command`), when there is one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub engine: Option<String>,
    /// Whether the audio stays on this machine.
    pub local: bool,
    /// Why there is none, and how to set one up.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
    /// Whether `twr voice setup` sets it up: whisper.cpp is there and only the default model
    /// is missing (the desktop app offers to download it).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub setup: bool,
    /// `[voice] pause_ms`: what the app listening hands-free takes for the end of an utterance.
    pub pause_ms: u64,
    /// `[voice] listen_while_speaking`: whether hands-free listens while replies are read.
    pub listen_while_speaking: bool,
}

/// The default model is not there yet: `twr voice setup` downloads it ([`MODEL_URL`]).
#[derive(Debug, Clone, PartialEq)]
pub struct MissingModel(pub PathBuf);

impl std::fmt::Display for MissingModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "no speech model yet: `twr voice setup` downloads whisper's base.en ({MODEL_SIZE}) to \
             {}, or name yours with [voice] model (docs/voice.md)",
            self.0.display()
        )
    }
}

impl std::error::Error for MissingModel {}

/// The recognizer `cfg` names, with `data_dir` (twr's data directory: the default model) and
/// `path` (`$PATH`). Fails, saying how to set one up, when there is none.
pub fn resolve(cfg: &VoiceConfig, data_dir: &Path, path: Option<&OsStr>) -> anyhow::Result<Kind> {
    let whisper = || -> anyhow::Result<Kind> {
        let program = if cfg.whisper.trim().is_empty() {
            WHISPER_PROGRAMS
                .iter()
                .find_map(|p| which(p, path))
                .ok_or_else(|| anyhow!("{NO_WHISPER}"))?
        } else {
            which(cfg.whisper.trim(), path)
                .ok_or_else(|| anyhow!("voice.whisper: {} not found", cfg.whisper))?
        };
        let model = if cfg.model.trim().is_empty() {
            let model = data_dir.join(DEFAULT_MODEL);
            if !model.is_file() {
                return Err(MissingModel(model).into());
            }
            model
        } else {
            expand_home(cfg.model.trim())
        };
        if !model.is_file() {
            bail!(
                "no whisper model at {} ([voice] model): download one, or leave [voice] model \
                 empty and run `twr voice setup` (docs/voice.md)",
                model.display()
            );
        }
        Ok(Kind::Whisper {
            program,
            model,
            language: cfg.language.clone(),
        })
    };
    let command = || -> anyhow::Result<Kind> {
        let Some(program) = cfg.command.first().filter(|p| !p.trim().is_empty()) else {
            bail!("voice.command: engine = \"command\" needs a program to run");
        };
        which(program, path).ok_or_else(|| anyhow!("voice.command: {program} not found"))?;
        Ok(Kind::Command(cfg.command.clone()))
    };
    match cfg.engine {
        VoiceEngine::Whisper => whisper(),
        VoiceEngine::Command => command(),
        VoiceEngine::Auto => match whisper() {
            Ok(k) => Ok(k),
            Err(e) if cfg.command.is_empty() => Err(e),
            Err(_) => command(),
        },
    }
}

/// [`resolve`], as `twr voice status` says it.
pub fn status(cfg: &VoiceConfig, data_dir: &Path, path: Option<&OsStr>) -> Status {
    match resolve(cfg, data_dir, path) {
        Ok(k) => Status {
            available: true,
            engine: Some(k.name().to_string()),
            local: k.local(),
            problem: None,
            setup: false,
            pause_ms: cfg.pause_ms,
            listen_while_speaking: cfg.listen_while_speaking,
        },
        Err(e) => Status {
            available: false,
            engine: None,
            local: false,
            setup: e.downcast_ref::<MissingModel>().is_some(),
            problem: Some(format!("{e:#}")),
            pause_ms: cfg.pause_ms,
            listen_while_speaking: cfg.listen_while_speaking,
        },
    }
}

const NO_WHISPER: &str = "no speech recognizer: install whisper.cpp (`brew install whisper-cpp` \
                          on macOS, `whisper.cpp` from your distribution or source on Linux) so \
                          `whisper-cli` is on PATH, or set [voice] engine = \"command\" \
                          (docs/voice.md)";

/// Where `twr voice setup` downloads [`DEFAULT_MODEL`] from: whisper.cpp's own models.
pub const MODEL_URL: &str =
    "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.en.bin";
/// Its size, as said to the user.
pub const MODEL_SIZE: &str = "148 MB";
/// Most bytes `twr voice setup` takes: base.en is 147,964,211.
pub const MODEL_MAX_BYTES: u64 = 256 * 1024 * 1024;
/// Fewest bytes of a model it keeps (an error page is not one).
pub const MODEL_MIN_BYTES: u64 = 16 * 1024 * 1024;

/// Whether `head`, a downloaded file's first bytes, starts a whisper.cpp model: ggml's magic,
/// `0x67676d6c` little-endian.
pub fn is_model(head: &[u8]) -> bool {
    head.starts_with(&0x6767_6d6c_u32.to_le_bytes())
}

/// `~/x` as the home directory's `x`.
fn expand_home(p: &str) -> PathBuf {
    match (p.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => PathBuf::from(p),
    }
}

/// Most words of a hint ([`hint_words`]).
pub const HINT_WORDS: usize = 64;

/// The words of `hint` a recognizer is told to expect (the names of the threads: "api" is
/// heard as "appy" without it): each 1 to 64 of `[A-Za-z0-9._-]`, at most [`HINT_WORDS`],
/// anything else left out. They reach whisper as one argument after `--prompt`, and a
/// command as `TWR_VOICE_HINT`, never through a shell.
pub fn hint_words(hint: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for w in hint.iter().flat_map(|h| h.split([' ', ','])) {
        let ok = (1..=64).contains(&w.len())
            && w.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
            && !w.starts_with('-');
        if ok && !out.iter().any(|o| o == w) && out.len() < HINT_WORDS {
            out.push(w.to_string());
        }
    }
    out
}

/// The text `recording` (checked by [`crate::wav::check`]) says, recognized by `kind` within
/// `timeout`, expecting the words of `hint` ([`hint_words`]): one line, empty when nothing
/// was said ([`crate::text::clean`]).
pub async fn transcribe(
    kind: &Kind,
    recording: &[u8],
    timeout: Duration,
    hint: &[String],
) -> anyhow::Result<String> {
    let hint = hint_words(hint);
    let rec = crate::wav::check(recording)?;
    // Silence: no recognizer is run (whisper makes words up for it).
    if rec.peak < 0.004 {
        return Ok(String::new());
    }
    let raw = match kind {
        Kind::Whisper {
            program,
            model,
            language,
        } => {
            // whisper.cpp reads a file: a private one, removed when done.
            let dir = tempfile::Builder::new()
                .prefix("twr-voice-")
                .tempdir()
                .context("making a private directory for the recording")?;
            let file = dir.path().join("speech.wav");
            write_private(&file, recording)?;
            let mut args: Vec<String> = vec![
                "-m".into(),
                model.to_string_lossy().into_owned(),
                "-l".into(),
                language.clone(),
                "-nt".into(),
                "-np".into(),
            ];
            if let Some(n) = std::thread::available_parallelism()
                .ok()
                .map(|n| n.get().clamp(1, 8))
            {
                args.extend(["-t".into(), n.to_string()]);
            }
            if !hint.is_empty() {
                args.extend(["--prompt".into(), format!("{}.", hint.join(", "))]);
            }
            args.extend(["-f".into(), file.to_string_lossy().into_owned()]);
            run(program, &args, None, &[], timeout).await?
        }
        Kind::Command(argv) => {
            let program = PathBuf::from(&argv[0]);
            let env = [("TWR_VOICE_HINT", hint.join(" "))];
            run(&program, &argv[1..], Some(recording), &env, timeout).await?
        }
    };
    Ok(crate::text::clean(&raw))
}

/// Writes `bytes` to a new file at `path` that only this user can read.
fn write_private(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("writing {}", path.display()))?;
    f.write_all(bytes)?;
    Ok(())
}

/// The process group of a recognizer: killed whole when dropped before it was reaped.
struct Group(Option<i32>);

impl Drop for Group {
    fn drop(&mut self) {
        if let Some(pid) = self.0.take() {
            // SAFETY: `kill` with a negative pid signals that process group; its leader is
            // ours and not reaped yet, so the id still names it.
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
            }
        }
    }
}

/// Runs `program` with `args` (no shell, its own process group), `stdin` on its stdin, until it
/// ends or `timeout`; its stdout (at most [`STDOUT_MAX`] bytes) when it succeeds.
async fn run(
    program: &Path,
    args: &[String],
    stdin: Option<&[u8]>,
    env: &[(&str, String)],
    timeout: Duration,
) -> anyhow::Result<String> {
    let label = program
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| program.display().to_string());
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args)
        .envs(env.iter().map(|(k, v)| (*k, v.as_str())))
        .stdin(if stdin.is_some() {
            std::process::Stdio::piped()
        } else {
            std::process::Stdio::null()
        })
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .process_group(0);
    let mut child = cmd.spawn().map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => anyhow!("{label}: {} not found", program.display()),
        _ => anyhow!("{label}: running {}: {e}", program.display()),
    })?;
    let mut group = Group(child.id().and_then(|p| i32::try_from(p).ok()));
    let input = child.stdin.take();
    let mut out = child.stdout.take().context("the recognizer's stdout")?;
    let mut err = child.stderr.take().context("the recognizer's stderr")?;
    let work = async {
        let write = async {
            if let (Some(mut w), Some(bytes)) = (input, stdin) {
                // A program that stops reading early closes its stdin: not an error of ours.
                let _ = w.write_all(bytes).await;
                let _ = w.shutdown().await;
            }
        };
        let read_out = async {
            let mut buf = Vec::new();
            let _ = (&mut out)
                .take(STDOUT_MAX as u64)
                .read_to_end(&mut buf)
                .await;
            // The rest is drained, so the program never blocks on a full pipe.
            let _ = tokio::io::copy(&mut out, &mut tokio::io::sink()).await;
            buf
        };
        let read_err = async {
            let mut buf = Vec::new();
            let _ = (&mut err)
                .take(STDERR_MAX as u64)
                .read_to_end(&mut buf)
                .await;
            let _ = tokio::io::copy(&mut err, &mut tokio::io::sink()).await;
            buf
        };
        let ((), stdout, stderr) = tokio::join!(write, read_out, read_err);
        let status = child.wait().await;
        (status, stdout, stderr)
    };
    let (status, stdout, stderr) = tokio::time::timeout(timeout, work)
        .await
        .map_err(|_| anyhow!("{label}: no text after {} seconds", timeout.as_secs()))?;
    let status = status.with_context(|| format!("{label}: waiting for it"))?;
    // Reaped: its id may be reused, so the group is not killed any more.
    group.0 = None;
    if !status.success() {
        let why = String::from_utf8_lossy(&stderr);
        let why = why
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("");
        bail!(
            "{label} failed ({status}){}{}",
            if why.is_empty() { "" } else { ": " },
            why.trim()
        );
    }
    Ok(String::from_utf8_lossy(&stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wav::{encode, RATE};
    use std::os::unix::fs::PermissionsExt;

    fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    fn speech() -> Vec<u8> {
        let samples: Vec<i16> = (0..RATE as usize)
            .map(|i| if i % 32 < 16 { 8000 } else { -8000 })
            .collect();
        encode(&samples)
    }

    fn cfg() -> VoiceConfig {
        VoiceConfig::default()
    }

    #[test]
    fn auto_wants_whisper_and_a_model_and_says_how_to_get_them() {
        let d = tempfile::tempdir().unwrap();
        let bin = d.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let path = bin.as_os_str().to_owned();
        let e = resolve(&cfg(), d.path(), Some(&path))
            .unwrap_err()
            .to_string();
        assert!(e.contains("whisper-cli"), "{e}");
        let program = script(&bin, "whisper-cli", "exit 0");
        let e = resolve(&cfg(), d.path(), Some(&path))
            .unwrap_err()
            .to_string();
        assert!(e.contains("twr voice setup"), "{e}");
        let s = status(&cfg(), d.path(), Some(&path));
        assert!(!s.available && s.setup, "{s:?}");
        std::fs::create_dir_all(d.path().join("voice")).unwrap();
        std::fs::write(d.path().join(DEFAULT_MODEL), b"model").unwrap();
        let k = resolve(&cfg(), d.path(), Some(&path)).unwrap();
        assert_eq!(
            k,
            Kind::Whisper {
                program,
                model: d.path().join(DEFAULT_MODEL),
                language: "en".into()
            }
        );
        assert!(k.local());
        let s = status(&cfg(), d.path(), Some(&path));
        assert!(s.available && s.local && !s.setup, "{s:?}");
    }

    #[test]
    fn setup_is_offered_only_for_the_default_model() {
        let d = tempfile::tempdir().unwrap();
        let bin = d.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let path = bin.as_os_str().to_owned();
        // No whisper.cpp: a download would not help.
        assert!(!status(&cfg(), d.path(), Some(&path)).setup);
        script(&bin, "whisper-cli", "exit 0");
        // A model of the user's that isn't there: theirs to fix.
        let mut c = cfg();
        c.model = d.path().join("mine.bin").to_string_lossy().into_owned();
        let s = status(&c, d.path(), Some(&path));
        assert!(!s.available && !s.setup, "{s:?}");
        assert!(s.problem.unwrap().contains("mine.bin"));
        let json = serde_json::to_value(status(&cfg(), d.path(), Some(&path))).unwrap();
        assert_eq!(json["setup"], true, "{json}");
    }

    #[test]
    fn status_tells_the_app_how_to_listen_hands_free() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().as_os_str().to_owned();
        let json = serde_json::to_value(status(&cfg(), d.path(), Some(&path))).unwrap();
        assert_eq!(json["pause_ms"], 2000, "{json}");
        assert_eq!(json["listen_while_speaking"], false, "{json}");
        let mut c = cfg();
        c.pause_ms = 3500;
        c.listen_while_speaking = true;
        let json = serde_json::to_value(status(&c, d.path(), Some(&path))).unwrap();
        assert_eq!(json["pause_ms"], 3500, "{json}");
        assert_eq!(json["listen_while_speaking"], true, "{json}");
    }

    #[test]
    fn a_model_starts_with_ggml_magic() {
        assert!(is_model(b"lmgg\x00\x00"));
        assert!(!is_model(b"<!DOCTYPE html>"));
        assert!(!is_model(b"lm"));
    }

    #[test]
    fn auto_falls_back_to_a_command_and_a_command_must_be_there() {
        let d = tempfile::tempdir().unwrap();
        let prog = script(d.path(), "stt", "cat >/dev/null; echo hi");
        let mut c = cfg();
        c.command = vec![prog.to_string_lossy().into_owned()];
        let k = resolve(&c, d.path(), Some(OsStr::new(""))).unwrap();
        assert_eq!(k.name(), "command");
        assert!(!k.local());
        c.command = vec![d.path().join("missing").to_string_lossy().into_owned()];
        c.engine = VoiceEngine::Command;
        assert!(resolve(&c, d.path(), None).is_err());
        let s = status(&c, d.path(), None);
        assert!(!s.available && s.problem.is_some(), "{s:?}");
    }

    #[tokio::test]
    async fn a_command_gets_the_recording_on_stdin_and_its_stdout_is_the_text() {
        let d = tempfile::tempdir().unwrap();
        // It proves it read a WAV: the text names the file's first four bytes.
        let prog = script(
            d.path(),
            "stt",
            "head -c 4 > \"$0.head\"; cat >/dev/null; printf ' Check the api thread.\\n[BLANK_AUDIO]\\n'",
        );
        let k = Kind::Command(vec![prog.to_string_lossy().into_owned()]);
        let text = transcribe(&k, &speech(), Duration::from_secs(10), &[])
            .await
            .unwrap();
        assert_eq!(text, "Check the api thread.");
        assert_eq!(
            std::fs::read(format!("{}.head", prog.display())).unwrap(),
            b"RIFF"
        );
    }

    #[tokio::test]
    async fn whisper_gets_a_private_file_and_fixed_arguments() {
        let d = tempfile::tempdir().unwrap();
        // Records its arguments and the file's mode, and answers as whisper-cli -nt does.
        let prog = script(
            d.path(),
            "whisper-cli",
            "printf '%s\\n' \"$@\" > \"$0.args\"; for a; do f=$a; done; \
             stat -f %Lp \"$f\" 2>/dev/null > \"$0.mode\" || stat -c %a \"$f\" > \"$0.mode\"; \
             printf '\\n Stop the api worker.\\n'",
        );
        let model = d.path().join("m.bin");
        std::fs::write(&model, b"m").unwrap();
        let k = Kind::Whisper {
            program: prog.clone(),
            model: model.clone(),
            language: "en".into(),
        };
        let hint = vec![
            "api docs".to_string(),
            "-x".into(),
            "a;b".into(),
            "api".into(),
        ];
        let text = transcribe(&k, &speech(), Duration::from_secs(10), &hint)
            .await
            .unwrap();
        assert_eq!(text, "Stop the api worker.");
        let args = std::fs::read_to_string(format!("{}.args", prog.display())).unwrap();
        let args: Vec<&str> = args.lines().collect();
        assert_eq!(
            &args[..6],
            ["-m", model.to_str().unwrap(), "-l", "en", "-nt", "-np"]
        );
        let at = args.iter().position(|a| *a == "--prompt").unwrap();
        assert_eq!(args[at + 1], "api, docs.", "only plain words, once each");
        assert_eq!(args[args.len() - 2], "-f");
        assert!(
            !Path::new(args[args.len() - 1]).exists(),
            "the file is removed"
        );
        let mode = std::fs::read_to_string(format!("{}.mode", prog.display())).unwrap();
        assert_eq!(mode.trim(), "600");
    }

    #[tokio::test]
    async fn silence_runs_no_recognizer_and_failures_say_why() {
        let d = tempfile::tempdir().unwrap();
        let never = script(d.path(), "never", "touch \"$0.ran\"");
        let k = Kind::Command(vec![never.to_string_lossy().into_owned()]);
        let quiet = encode(&vec![3; RATE as usize]);
        assert_eq!(
            transcribe(&k, &quiet, Duration::from_secs(5), &[])
                .await
                .unwrap(),
            ""
        );
        assert!(!Path::new(&format!("{}.ran", never.display())).exists());
        let fails = script(
            d.path(),
            "fails",
            "cat >/dev/null; echo 'no key set' >&2; exit 3",
        );
        let k = Kind::Command(vec![fails.to_string_lossy().into_owned()]);
        let e = transcribe(&k, &speech(), Duration::from_secs(5), &[])
            .await
            .unwrap_err();
        assert!(e.to_string().contains("no key set"), "{e}");
        let slow = script(d.path(), "slow", "cat >/dev/null; sleep 30");
        let k = Kind::Command(vec![slow.to_string_lossy().into_owned()]);
        let e = transcribe(&k, &speech(), Duration::from_millis(500), &[])
            .await
            .unwrap_err();
        assert!(e.to_string().contains("no text after"), "{e}");
        assert!(transcribe(&k, b"junk", Duration::from_secs(1), &[])
            .await
            .is_err());
    }
}

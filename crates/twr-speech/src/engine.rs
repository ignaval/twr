//! Engines: what turns an utterance into sound.
//!
//! Every engine says one utterance (a chunk of at most `twr_core::speech::CHUNK_MAX`
//! characters) at a time: [`Engine::speak`] returns a future that ends when it was said.
//! Dropping that future stops the voice at once; [`Engine::after_stop`] then silences anything
//! the engine keeps on its own (speech-dispatcher's queue). An engine that can pause in place
//! says so from [`Engine::pause`]; for the others the player stops the chunk and says it again
//! on resume.
//!
//! The programs (`say`, `spd-say`, `[speech] command`) are run without a shell, with fixed
//! arguments: the text goes to their stdin, never into an argument, so a message can't name an
//! option or a file. Each runs in a process group of its own, killed whole when the chunk is
//! stopped.

use std::ffi::OsStr;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, bail};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use twr_core::config::{SpeechConfig, SpeechEngine};

/// The future of one utterance ([`Engine::speak`]): `Ok` once it was said.
pub type SpeakFuture = Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send>>;

/// A future with nothing to tell ([`Engine::after_stop`]).
pub type QuietFuture = Pin<Box<dyn Future<Output = ()> + Send>>;

/// How the voice speaks.
#[derive(Debug, Clone, PartialEq)]
pub struct Prefs {
    /// The engine's voice; `None`: its default.
    pub voice: Option<String>,
    /// Words per minute; 0: the engine's default.
    pub rate: u32,
    /// Volume, 0.0 to 1.0, where the engine has one.
    pub volume: f32,
}

impl Default for Prefs {
    fn default() -> Self {
        Prefs {
            voice: None,
            rate: 0,
            volume: 1.0,
        }
    }
}

/// Something that speaks (see the module documentation).
pub trait Engine: Send {
    /// Its name, as a speaker reports it (`say`, `spd`, `command`, `avspeech`, `fake`).
    fn name(&self) -> &str;

    /// Says `chunk` with `prefs`. The future ends once it was said, or with an error (the
    /// program missing, failing, killed by someone else). Dropping it stops the voice now.
    fn speak(&mut self, chunk: &str, prefs: &Prefs) -> SpeakFuture;

    /// Pauses what it says, in place; whether it could (false: the player stops the chunk and
    /// says it again on resume).
    fn pause(&mut self) -> bool {
        false
    }

    /// Goes on after [`Engine::pause`]; whether it could.
    fn resume(&mut self) -> bool {
        false
    }

    /// After a [`Engine::speak`] future was dropped before it ended: silences what the engine
    /// keeps speaking by itself. Awaited before the next chunk starts.
    fn after_stop(&mut self) -> QuietFuture {
        Box::pin(async {})
    }
}

/// The engine `[speech]` names (`engine` overrides `cfg.engine`): `auto` is `say` on macOS and
/// `spd` on Linux when `spd-say` is installed. Fails when there is none, or its program is not
/// there.
pub fn from_config(
    cfg: &SpeechConfig,
    engine: Option<SpeechEngine>,
) -> anyhow::Result<Box<dyn Engine>> {
    let kind = resolve(
        engine.unwrap_or(cfg.engine),
        &cfg.command,
        std::env::consts::OS,
        std::env::var_os("PATH").as_deref(),
    )?;
    Ok(match kind {
        Kind::Say(p) => Box::new(SayEngine::with_program(p)),
        Kind::Spd(p) => Box::new(SpdEngine::with_program(p)),
        Kind::Command(argv) => Box::new(CommandEngine::new(argv)?),
        Kind::AvSpeech => avspeech()?,
    })
}

/// Whether `[speech]` names an engine this machine has ([`from_config`] would make one).
pub fn engine_available(cfg: &SpeechConfig) -> bool {
    resolve(
        cfg.engine,
        &cfg.command,
        std::env::consts::OS,
        std::env::var_os("PATH").as_deref(),
    )
    .is_ok()
}

/// The error of `auto` on a machine with no engine.
pub const NO_ENGINE: &str = "no speech engine: install speech-dispatcher or set [speech] engine";

/// An engine chosen, with its program.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    /// `say`, at this path.
    Say(PathBuf),
    /// `spd-say`, at this path.
    Spd(PathBuf),
    /// `[speech] command`.
    Command(Vec<String>),
    /// AVSpeechSynthesizer.
    AvSpeech,
}

/// The engine `engine` means on `os` (`std::env::consts::OS`) with `path` (`$PATH`), and
/// `command` (`[speech] command`).
pub fn resolve(
    engine: SpeechEngine,
    command: &[String],
    os: &str,
    path: Option<&OsStr>,
) -> anyhow::Result<Kind> {
    let found = |name: &str| {
        which(name, path).ok_or_else(|| anyhow!("speech engine: {name} not found on PATH"))
    };
    match engine {
        SpeechEngine::Auto => match os {
            "macos" => Ok(Kind::Say(found("say")?)),
            "linux" => which("spd-say", path)
                .map(Kind::Spd)
                .ok_or_else(|| anyhow!(NO_ENGINE)),
            _ => bail!(NO_ENGINE),
        },
        SpeechEngine::Say => Ok(Kind::Say(found("say")?)),
        SpeechEngine::Spd => Ok(Kind::Spd(found("spd-say")?)),
        SpeechEngine::Avspeech if os == "macos" => Ok(Kind::AvSpeech),
        SpeechEngine::Avspeech => bail!("speech engine: avspeech needs macOS"),
        SpeechEngine::Command => {
            let Some(program) = command.first().filter(|p| !p.trim().is_empty()) else {
                bail!("speech.command: engine = \"command\" needs a program to run");
            };
            found(program)?;
            Ok(Kind::Command(command.to_vec()))
        }
    }
}

/// Where `name` is: itself when it names a path (has a `/`), else the first executable file
/// of that name in a directory of `path`.
pub fn which(name: &str, path: Option<&OsStr>) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let runnable = |p: &Path| {
        std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    };
    if name.contains('/') {
        let p = PathBuf::from(name);
        return runnable(&p).then_some(p);
    }
    std::env::split_paths(path?)
        .filter(|d| !d.as_os_str().is_empty())
        .map(|d| d.join(name))
        .find(|p| runnable(p))
}

/// The process group of a program being run: killed whole when dropped before the program
/// was reaped (its id can't be reused until then).
struct Group(Option<i32>);

impl Drop for Group {
    fn drop(&mut self) {
        if let Some(pid) = self.0.take() {
            // SAFETY: `kill` with a negative pid signals that process group; the group leader
            // is ours and not reaped yet, so the id still names it.
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
            }
        }
    }
}

/// Most bytes kept of what a program wrote to stderr (for its error).
const STDERR_MAX: usize = 4096;

/// How long `spd-say -C` may take.
const SILENCE_TIMEOUT: Duration = Duration::from_secs(2);

/// Runs `program` with `args` (no shell, its own process group, stdout discarded) with `text`
/// and a newline on its stdin, until it ends. `label` names it in errors. Dropping the future
/// kills its process group.
async fn run_program(
    label: &str,
    program: &Path,
    args: &[String],
    text: &str,
) -> anyhow::Result<()> {
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .process_group(0);
    // "Text file busy" (ETXTBSY): a program written just now (a script) whose write
    // descriptor a process forked meanwhile still holds until its own exec. It passes in a
    // moment: tried again a few times.
    let mut tries = 0;
    let spawned = loop {
        match cmd.spawn() {
            Err(e) if e.raw_os_error() == Some(libc::ETXTBSY) && tries < 5 => {
                tries += 1;
                tokio::time::sleep(Duration::from_millis(20 * tries)).await;
            }
            r => break r,
        }
    };
    let mut child = spawned.map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => anyhow!("{label}: {} not found", program.display()),
        _ => anyhow!("{label}: running {}: {e}", program.display()),
    })?;
    let mut group = Group(child.id().and_then(|p| i32::try_from(p).ok()));
    let mut stdin = child.stdin.take();
    // One line: an utterance holds no line break (`clean` makes them spaces), but a line
    // reader (`spd-say -e`) needs the newline.
    let line = format!("{}\n", text.replace(['\n', '\r'], " "));
    let write = async {
        if let Some(mut w) = stdin.take() {
            // A program that doesn't read its stdin closes it: not an error of ours.
            match w.write_all(line.as_bytes()).await {
                Err(e) if e.kind() != std::io::ErrorKind::BrokenPipe => return Err(e),
                _ => {}
            }
        }
        Ok(())
    };
    // Stderr is read meanwhile (so a chatty program never blocks on it), keeping the start.
    let kept = Arc::new(Mutex::new(Vec::new()));
    // Aborted when this future is dropped (stop, skip, quit), not left reading on its own.
    let reader = child.stderr.take().map(|mut err| {
        let kept = kept.clone();
        crate::link::AbortOnDrop(tokio::spawn(async move {
            let mut buf = [0u8; 1024];
            while let Ok(n) = err.read(&mut buf).await {
                if n == 0 {
                    break;
                }
                let mut k = kept.lock().unwrap_or_else(|p| p.into_inner());
                let room = STDERR_MAX.saturating_sub(k.len());
                k.extend_from_slice(&buf[..n.min(room)]);
            }
        }))
    });
    let (written, status) = tokio::join!(write, child.wait());
    // Reaped: its id may be reused from now on.
    group.0 = None;
    let status = status.map_err(|e| anyhow!("{label}: waiting for it: {e}"))?;
    if let Some(mut r) = reader {
        // A program that left a process holding its stderr would keep the reader: it ends
        // here either way. One that exits closes its stderr at once; the 2 s only matter for
        // the first kind (a loaded machine needs more than a moment for the rest).
        if tokio::time::timeout(Duration::from_secs(2), &mut r.0)
            .await
            .is_err()
        {
            r.0.abort();
            let _ = (&mut r.0).await;
        }
    }
    if status.success() {
        written.map_err(|e| anyhow!("{label}: writing its text: {e}"))?;
        return Ok(());
    }
    let said = String::from_utf8_lossy(&kept.lock().unwrap_or_else(|p| p.into_inner()))
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(|l| format!(": {}", twr_core::speech::clean(l)))
        .unwrap_or_default();
    bail!("{label} failed ({status}){said}")
}

/// macOS's `say`: `say [-v VOICE] [-r WPM]`, the text on its stdin.
///
/// That `say` reads its text from stdin when given none in arguments, and the meaning of `-v`
/// and `-r` (words per minute), are taken from the ss64 mirror of its manual page
/// (<https://ss64.com/mac/say.html>): not yet checked on a real Mac. It has no volume option.
#[derive(Debug, Clone)]
pub struct SayEngine {
    program: PathBuf,
}

impl SayEngine {
    /// `say` from `PATH`.
    pub fn new() -> SayEngine {
        SayEngine::with_program("say")
    }

    /// The program at `program` (tests: a recorder script).
    pub fn with_program(program: impl Into<PathBuf>) -> SayEngine {
        SayEngine {
            program: program.into(),
        }
    }

    /// Its arguments for `prefs`.
    pub fn args(prefs: &Prefs) -> Vec<String> {
        let mut args = Vec::new();
        if let Some(v) = &prefs.voice {
            args.extend(["-v".to_string(), v.clone()]);
        }
        if prefs.rate > 0 {
            args.extend(["-r".to_string(), prefs.rate.to_string()]);
        }
        args
    }
}

impl Default for SayEngine {
    fn default() -> Self {
        SayEngine::new()
    }
}

impl Engine for SayEngine {
    fn name(&self) -> &str {
        "say"
    }

    fn speak(&mut self, chunk: &str, prefs: &Prefs) -> SpeakFuture {
        let (program, args, text) = (self.program.clone(), Self::args(prefs), apple_text(chunk));
        Box::pin(async move { run_program("say", &program, &args, &text).await })
    }
}

/// A chunk as `spd-say -e` takes it: as text, never as a command. In pipe mode, a line that
/// starts with `!-!` is a command to speech-dispatcher itself (`!-!CANCEL ALL` would silence
/// every program's speech); the text reaches it as one line (`run_program`), and a space in
/// front of it means it never starts so.
pub fn spd_text(chunk: &str) -> String {
    format!(" {chunk}")
}

/// A chunk as macOS's synthesizer (`say`, AVSpeechSynthesizer) takes it: as text, never as
/// commands. It reads `[[...]]` in the text as embedded speech commands (the rate, the
/// volume, silence, how to read what follows); `[ [` is read as the brackets it is.
pub fn apple_text(chunk: &str) -> String {
    let mut t = chunk.to_string();
    while t.contains("[[") {
        t = t.replace("[[", "[ [");
    }
    t
}

/// speech-dispatcher's `spd-say -w -e [-y VOICE] [-r N] [-i N]`: it waits until the text was
/// said (`-w`) and reads it from stdin (`-e`, which also echoes it to stdout: discarded). Rate
/// and volume are speech-dispatcher's -100 to 100. Stopped by killing it, then `spd-say -C`
/// (cancel what speech-dispatcher still has).
#[derive(Debug, Clone)]
pub struct SpdEngine {
    program: PathBuf,
}

impl SpdEngine {
    /// `spd-say` from `PATH`.
    pub fn new() -> SpdEngine {
        SpdEngine::with_program("spd-say")
    }

    /// The program at `program` (tests: a recorder script).
    pub fn with_program(program: impl Into<PathBuf>) -> SpdEngine {
        SpdEngine {
            program: program.into(),
        }
    }

    /// Its arguments for `prefs`.
    pub fn args(prefs: &Prefs) -> Vec<String> {
        let mut args = vec!["-w".to_string(), "-e".to_string()];
        if let Some(v) = &prefs.voice {
            args.extend(["-y".to_string(), v.clone()]);
        }
        if prefs.rate > 0 {
            args.extend(["-r".to_string(), spd_rate(prefs.rate).to_string()]);
        }
        args.extend(["-i".to_string(), spd_volume(prefs.volume).to_string()]);
        args
    }
}

impl Default for SpdEngine {
    fn default() -> Self {
        SpdEngine::new()
    }
}

/// Words per minute as speech-dispatcher's rate: 175 (about its default) is 0, 80 is -100,
/// 500 is 100, straight lines between.
pub fn spd_rate(wpm: u32) -> i32 {
    let wpm = wpm.clamp(80, 500) as i32;
    if wpm <= 175 {
        (wpm - 175) * 100 / (175 - 80)
    } else {
        (wpm - 175) * 100 / (500 - 175)
    }
}

/// A volume of 0.0 to 1.0 as speech-dispatcher's -100 to 100.
pub fn spd_volume(volume: f32) -> i32 {
    (volume.clamp(0.0, 1.0) * 200.0 - 100.0).round() as i32
}

impl Engine for SpdEngine {
    fn name(&self) -> &str {
        "spd"
    }

    fn speak(&mut self, chunk: &str, prefs: &Prefs) -> SpeakFuture {
        let (program, args, text) = (self.program.clone(), Self::args(prefs), spd_text(chunk));
        Box::pin(async move { run_program("spd-say", &program, &args, &text).await })
    }

    fn after_stop(&mut self) -> QuietFuture {
        let program = self.program.clone();
        Box::pin(async move {
            let args = ["-C".to_string()];
            let cancel = run_program("spd-say -C", &program, &args, "");
            match tokio::time::timeout(SILENCE_TIMEOUT, cancel).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => tracing::debug!("{e}"),
                Err(_) => tracing::debug!("spd-say -C did not finish"),
            }
        })
    }
}

/// `[speech] command`: a program of yours and its arguments, run as given (no shell), with each
/// utterance on its stdin. The voice and rate are its own business.
#[derive(Debug, Clone)]
pub struct CommandEngine {
    program: PathBuf,
    args: Vec<String>,
}

impl CommandEngine {
    /// The program `argv[0]` with the arguments `argv[1..]`.
    pub fn new(argv: Vec<String>) -> anyhow::Result<CommandEngine> {
        let mut argv = argv.into_iter();
        let Some(program) = argv.next().filter(|p| !p.trim().is_empty()) else {
            bail!("speech.command: engine = \"command\" needs a program to run");
        };
        Ok(CommandEngine {
            program: program.into(),
            args: argv.collect(),
        })
    }
}

impl Engine for CommandEngine {
    fn name(&self) -> &str {
        "command"
    }

    fn speak(&mut self, chunk: &str, _prefs: &Prefs) -> SpeakFuture {
        let (program, args, text) = (self.program.clone(), self.args.clone(), chunk.to_string());
        Box::pin(async move { run_program("speech command", &program, &args, &text).await })
    }
}

/// The `avspeech` engine (macOS's AVSpeechSynthesizer, `twr-avspeech`).
fn avspeech() -> anyhow::Result<Box<dyn Engine>> {
    let avs = twr_avspeech::AvSpeech::new().map_err(|e| anyhow!("speech engine: {e}"))?;
    Ok(Box::new(AvSpeechEngine { avs: Arc::new(avs) }))
}

/// macOS's AVSpeechSynthesizer, in-process: it pauses at a word and goes on from there.
/// Completion is polled every [`twr_avspeech::POLL`].
#[derive(Debug)]
pub struct AvSpeechEngine {
    avs: Arc<twr_avspeech::AvSpeech>,
}

/// Stops an utterance of [`AvSpeechEngine`] whose future was dropped before it ended.
struct AvStop {
    avs: Arc<twr_avspeech::AvSpeech>,
    id: u64,
}

impl Drop for AvStop {
    fn drop(&mut self) {
        if !self.avs.finished(self.id) {
            self.avs.stop();
        }
    }
}

impl Engine for AvSpeechEngine {
    fn name(&self) -> &str {
        "avspeech"
    }

    fn speak(&mut self, chunk: &str, prefs: &Prefs) -> SpeakFuture {
        let avs = self.avs.clone();
        let (text, prefs) = (apple_text(chunk), prefs.clone());
        Box::pin(async move {
            let rate = (prefs.rate > 0).then_some(prefs.rate);
            let id = avs
                .speak(&text, prefs.voice.as_deref(), rate, prefs.volume)
                .map_err(|e| anyhow!("{e}"))?;
            let stop = AvStop { avs, id };
            while !stop.avs.finished(stop.id) {
                tokio::time::sleep(twr_avspeech::POLL).await;
            }
            Ok(())
        })
    }

    fn pause(&mut self) -> bool {
        self.avs.pause()
    }

    fn resume(&mut self) -> bool {
        self.avs.resume()
    }
}

/// What a [`FakeEngine`] did.
#[derive(Debug, Clone, PartialEq)]
pub enum FakeEvent {
    /// Began saying a chunk with these prefs.
    Speak {
        /// The chunk.
        chunk: String,
        /// Its prefs.
        prefs: Prefs,
    },
    /// Said it to its end.
    Finished(String),
    /// Was stopped while saying it (its future dropped).
    Stopped(String),
    /// Paused in place (only with [`FakeEngine::with_native_pause`]).
    Paused,
    /// Went on.
    Resumed,
}

/// What a [`FakeEngine`] and its [`FakeHandle`]s share.
#[derive(Debug)]
struct FakeShared {
    events: Vec<FakeEvent>,
    fail_next: Option<String>,
    speaking: bool,
}

/// An engine that makes no sound, for tests: it records what it was asked ([`FakeHandle`]),
/// and says each chunk in a set time ([`FakeEngine::timed`], virtual under
/// `tokio::time::pause`) or when told ([`FakeEngine::manual`], [`FakeHandle::finish`]).
#[derive(Debug)]
pub struct FakeEngine {
    shared: Arc<Mutex<FakeShared>>,
    per_chunk: Option<Duration>,
    native_pause: bool,
    /// How long [`Engine::after_stop`] takes (`spd-say -C`'s wait).
    slow_stop: Option<Duration>,
    paused: tokio::sync::watch::Sender<bool>,
    finish: Arc<tokio::sync::Notify>,
}

/// A look into a [`FakeEngine`] (and a hand on it).
#[derive(Debug, Clone)]
pub struct FakeHandle {
    shared: Arc<Mutex<FakeShared>>,
    finish: Arc<tokio::sync::Notify>,
}

impl FakeEngine {
    /// Says each chunk in `per_chunk`.
    pub fn timed(per_chunk: Duration) -> (FakeEngine, FakeHandle) {
        Self::make(Some(per_chunk))
    }

    /// Says a chunk once [`FakeHandle::finish`] is called.
    pub fn manual() -> (FakeEngine, FakeHandle) {
        Self::make(None)
    }

    /// Pauses in place, like AVSpeech, instead of being stopped and restarted.
    pub fn with_native_pause(mut self) -> FakeEngine {
        self.native_pause = true;
        self
    }

    /// Takes `wait` to silence itself after a chunk was stopped ([`Engine::after_stop`]), as
    /// `spd-say -C` may.
    pub fn with_slow_stop(mut self, wait: Duration) -> FakeEngine {
        self.slow_stop = Some(wait);
        self
    }

    fn make(per_chunk: Option<Duration>) -> (FakeEngine, FakeHandle) {
        let shared = Arc::new(Mutex::new(FakeShared {
            events: Vec::new(),
            fail_next: None,
            speaking: false,
        }));
        let finish = Arc::new(tokio::sync::Notify::new());
        let (paused, _) = tokio::sync::watch::channel(false);
        (
            FakeEngine {
                shared: shared.clone(),
                per_chunk,
                native_pause: false,
                slow_stop: None,
                paused,
                finish: finish.clone(),
            },
            FakeHandle { shared, finish },
        )
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, FakeShared> {
        self.shared.lock().unwrap_or_else(|p| p.into_inner())
    }
}

impl FakeHandle {
    fn lock(&self) -> std::sync::MutexGuard<'_, FakeShared> {
        self.shared.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Everything it did, in order.
    pub fn events(&self) -> Vec<FakeEvent> {
        self.lock().events.clone()
    }

    /// The chunks it began saying, in order (a restarted chunk twice).
    pub fn spoken(&self) -> Vec<String> {
        self.lock()
            .events
            .iter()
            .filter_map(|e| match e {
                FakeEvent::Speak { chunk, .. } => Some(chunk.clone()),
                _ => None,
            })
            .collect()
    }

    /// The chunks it said to their end, in order.
    pub fn finished(&self) -> Vec<String> {
        self.lock()
            .events
            .iter()
            .filter_map(|e| match e {
                FakeEvent::Finished(c) => Some(c.clone()),
                _ => None,
            })
            .collect()
    }

    /// Whether it is saying a chunk now.
    pub fn speaking(&self) -> bool {
        self.lock().speaking
    }

    /// Ends the chunk being said ([`FakeEngine::manual`]); with none, the next one ends at once.
    pub fn finish(&self) {
        self.finish.notify_one();
    }

    /// The next chunk fails with `error`.
    pub fn fail_next(&self, error: &str) {
        self.lock().fail_next = Some(error.to_string());
    }
}

/// Records [`FakeEvent::Stopped`] for a chunk whose future was dropped before it ended.
struct FakeStop {
    shared: Arc<Mutex<FakeShared>>,
    chunk: String,
    done: bool,
}

impl Drop for FakeStop {
    fn drop(&mut self) {
        let mut s = self.shared.lock().unwrap_or_else(|p| p.into_inner());
        s.speaking = false;
        let chunk = std::mem::take(&mut self.chunk);
        s.events.push(if self.done {
            FakeEvent::Finished(chunk)
        } else {
            FakeEvent::Stopped(chunk)
        });
    }
}

impl Engine for FakeEngine {
    fn name(&self) -> &str {
        "fake"
    }

    fn speak(&mut self, chunk: &str, prefs: &Prefs) -> SpeakFuture {
        let fail = {
            let mut s = self.lock();
            s.events.push(FakeEvent::Speak {
                chunk: chunk.to_string(),
                prefs: prefs.clone(),
            });
            s.fail_next.take()
        };
        self.paused.send_replace(false);
        let (shared, chunk) = (self.shared.clone(), chunk.to_string());
        let (per_chunk, finish) = (self.per_chunk, self.finish.clone());
        let mut paused = self.paused.subscribe();
        Box::pin(async move {
            if let Some(e) = fail {
                bail!("{e}");
            }
            shared.lock().unwrap_or_else(|p| p.into_inner()).speaking = true;
            let mut stop = FakeStop {
                shared,
                chunk,
                done: false,
            };
            match per_chunk {
                None => finish.notified().await,
                Some(mut left) => loop {
                    if *paused.borrow_and_update() {
                        if paused.changed().await.is_err() {
                            futures::future::pending::<()>().await;
                        }
                        continue;
                    }
                    let start = tokio::time::Instant::now();
                    tokio::select! {
                        _ = tokio::time::sleep(left) => break,
                        _ = paused.changed() => left = left.saturating_sub(start.elapsed()),
                    }
                },
            }
            stop.done = true;
            Ok(())
        })
    }

    fn pause(&mut self) -> bool {
        if !self.native_pause || !self.lock().speaking {
            return false;
        }
        self.paused.send_replace(true);
        self.lock().events.push(FakeEvent::Paused);
        true
    }

    fn resume(&mut self) -> bool {
        if !self.native_pause || !*self.paused.borrow() {
            return false;
        }
        self.paused.send_replace(false);
        self.lock().events.push(FakeEvent::Resumed);
        true
    }

    fn after_stop(&mut self) -> QuietFuture {
        let wait = self.slow_stop;
        Box::pin(async move {
            if let Some(w) = wait {
                tokio::time::sleep(w).await;
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// A program at `dir/name` that records its arguments (one per line) in `name.args` and its
    /// stdin in `name.stdin`, then exits 0.
    fn recorder(dir: &Path, name: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(
            &p,
            "#!/bin/sh\nprintf '%s\\n' \"$@\" >> \"$0.args\"\nprintf -- '--\\n' >> \"$0.args\"\ncat >> \"$0.stdin\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    fn read(p: &Path, ext: &str) -> String {
        std::fs::read_to_string(format!("{}.{ext}", p.display())).unwrap_or_default()
    }

    fn prefs(voice: Option<&str>, rate: u32, volume: f32) -> Prefs {
        Prefs {
            voice: voice.map(str::to_string),
            rate,
            volume,
        }
    }

    /// A text that looks like options goes to stdin, never into the arguments.
    const SNEAKY: &str = "-o /tmp/x --help; rm -rf ~";

    #[tokio::test]
    async fn say_gets_its_text_on_stdin_and_only_fixed_arguments() {
        let dir = tempfile::tempdir().unwrap();
        let p = recorder(dir.path(), "say");
        let mut e = SayEngine::with_program(&p);
        e.speak(SNEAKY, &prefs(Some("Ava"), 200, 0.5))
            .await
            .unwrap();
        e.speak("plain", &Prefs::default()).await.unwrap();
        // No arguments at all the second time (printf then prints one empty line).
        assert_eq!(read(&p, "args"), "-v\nAva\n-r\n200\n--\n\n--\n");
        assert_eq!(read(&p, "stdin"), format!("{SNEAKY}\nplain\n"));
    }

    #[tokio::test]
    async fn spd_waits_reads_stdin_and_maps_rate_and_volume() {
        let dir = tempfile::tempdir().unwrap();
        let p = recorder(dir.path(), "spd-say");
        let mut e = SpdEngine::with_program(&p);
        e.speak(SNEAKY, &prefs(Some("female1"), 500, 0.5))
            .await
            .unwrap();
        e.speak("plain", &Prefs::default()).await.unwrap();
        assert_eq!(
            read(&p, "args"),
            "-w\n-e\n-y\nfemale1\n-r\n100\n-i\n0\n--\n-w\n-e\n-i\n100\n--\n"
        );
        assert_eq!(read(&p, "stdin"), format!(" {SNEAKY}\n plain\n"));
        // A text that is a speech-dispatcher command in pipe mode reaches it as text.
        e.speak("!-!CANCEL ALL", &Prefs::default()).await.unwrap();
        assert!(read(&p, "stdin").ends_with("\n !-!CANCEL ALL\n"));
        assert_eq!(spd_text("!-!x"), " !-!x");
        assert_eq!(apple_text("a [[volm 0]] b [[[x"), "a [ [volm 0]] b [ [ [x");
        e.after_stop().await;
        assert!(read(&p, "args").ends_with("--\n-C\n--\n"));
        assert_eq!(
            (spd_rate(80), spd_rate(175), spd_rate(9999)),
            (-100, 0, 100)
        );
        assert_eq!((spd_volume(0.0), spd_volume(1.0)), (-100, 100));
    }

    #[tokio::test]
    async fn a_command_gets_its_own_arguments_and_the_text_on_stdin() {
        let dir = tempfile::tempdir().unwrap();
        let p = recorder(dir.path(), "my-tts");
        let mut e = CommandEngine::new(vec![
            p.display().to_string(),
            "--fast".into(),
            "$HOME".into(),
        ])
        .unwrap();
        e.speak(SNEAKY, &prefs(Some("Ava"), 200, 1.0))
            .await
            .unwrap();
        // No shell: `$HOME` is passed as written.
        assert_eq!(read(&p, "args"), "--fast\n$HOME\n--\n");
        assert_eq!(read(&p, "stdin"), format!("{SNEAKY}\n"));
        assert!(CommandEngine::new(vec![]).is_err());
    }

    #[tokio::test]
    async fn a_failing_or_missing_program_is_an_error_with_its_words() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("say");
        std::fs::write(
            &p,
            "#!/bin/sh\necho 'Voice `Nobody` not found.' >&2\nexit 1\n",
        )
        .unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        let e = SayEngine::with_program(&p)
            .speak("hi", &Prefs::default())
            .await
            .unwrap_err();
        assert!(e.to_string().contains("Voice `Nobody` not found."), "{e}");
        let e = SayEngine::with_program(dir.path().join("nope"))
            .speak("hi", &Prefs::default())
            .await
            .unwrap_err();
        assert!(e.to_string().contains("not found"), "{e}");
    }

    #[tokio::test]
    async fn dropping_the_future_kills_the_program_and_its_group() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("slow");
        let pids = dir.path().join("pids");
        // The program starts a child of its own (same group), records both ids, and waits.
        std::fs::write(
            &p,
            format!(
                "#!/bin/sh\nsleep 30 &\necho $$ $! > {}\nwait\n",
                pids.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut e = CommandEngine::new(vec![p.display().to_string()]).unwrap();
        let fut = e.speak("hi", &Prefs::default());
        let task = tokio::spawn(fut);
        let mut ids = String::new();
        for _ in 0..200 {
            ids = std::fs::read_to_string(&pids).unwrap_or_default();
            if ids.ends_with('\n') {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let ids: Vec<i32> = ids.split_whitespace().map(|s| s.parse().unwrap()).collect();
        assert_eq!(ids.len(), 2, "{ids:?}");
        task.abort();
        let _ = task.await;
        // Both gone (the child may linger as a zombie of init for a moment).
        let alive = |pid: i32| {
            std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .is_ok_and(|s| !s.split_whitespace().nth(2).is_some_and(|st| st == "Z"))
        };
        for _ in 0..200 {
            if !ids.iter().any(|&p| alive(p)) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(!ids.iter().any(|&p| alive(p)), "still running: {ids:?}");
    }

    #[test]
    fn auto_is_say_on_macos_and_spd_on_linux_when_installed() {
        let dir = tempfile::tempdir().unwrap();
        let empty = tempfile::tempdir().unwrap();
        let spd = recorder(dir.path(), "spd-say");
        let say = recorder(dir.path(), "say");
        let path = Some(dir.path().as_os_str());
        let none = Some(empty.path().as_os_str());
        let auto = |os, path| resolve(SpeechEngine::Auto, &[], os, path);
        assert_eq!(auto("linux", path).unwrap(), Kind::Spd(spd));
        assert_eq!(auto("macos", path).unwrap(), Kind::Say(say));
        assert_eq!(auto("linux", none).unwrap_err().to_string(), NO_ENGINE);
        assert_eq!(auto("windows", path).unwrap_err().to_string(), NO_ENGINE);
        assert!(resolve(SpeechEngine::Avspeech, &[], "linux", path).is_err());
        assert_eq!(
            resolve(SpeechEngine::Avspeech, &[], "macos", path).unwrap(),
            Kind::AvSpeech
        );
        let cmd = vec!["spd-say".to_string(), "-x".to_string()];
        assert_eq!(
            resolve(SpeechEngine::Command, &cmd, "linux", path).unwrap(),
            Kind::Command(cmd.clone())
        );
        assert!(resolve(SpeechEngine::Command, &cmd, "linux", none).is_err());
        assert!(resolve(SpeechEngine::Command, &[], "linux", path).is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn the_fake_says_in_time_pauses_in_place_and_records_a_stop() {
        let (mut e, h) = FakeEngine::timed(Duration::from_secs(2));
        let mut e2 = FakeEngine::timed(Duration::from_secs(2))
            .0
            .with_native_pause();
        let start = tokio::time::Instant::now();
        e.speak("one", &Prefs::default()).await.unwrap();
        assert_eq!(start.elapsed(), Duration::from_secs(2));
        assert_eq!(h.finished(), vec!["one"]);
        drop(e.speak("two", &Prefs::default()));
        assert!(matches!(h.events().last(), Some(FakeEvent::Speak { .. })));
        let fut = tokio::spawn(e.speak("three", &Prefs::default()));
        tokio::time::sleep(Duration::from_millis(500)).await;
        fut.abort();
        let _ = fut.await;
        assert_eq!(h.events().last(), Some(&FakeEvent::Stopped("three".into())));
        assert!(!e.pause(), "no native pause");
        // Native pause: the time paused doesn't count.
        let fut = tokio::spawn(e2.speak("four", &Prefs::default()));
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert!(e2.pause());
        tokio::time::sleep(Duration::from_secs(10)).await;
        assert!(!fut.is_finished());
        assert!(e2.resume());
        tokio::time::sleep(Duration::from_millis(1100)).await;
        assert!(fut.is_finished());
    }
}

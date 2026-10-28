//! twr's speaker: reads the orchestrator's messages aloud on the machine you listen on
//! (`docs/read-aloud.md`). `twr speak` runs one; the TUI runs its own when it is on your own
//! machine.
//!
//! The hub decides what may be read and sends it (`speech.attach`); the speaker decides what
//! is played, and when. Its parts:
//!
//! - [`engine`]: what makes the sound. [`engine::Engine`] says one utterance at a time;
//!   [`engine::from_config`] picks the one `[speech] engine` names (`auto`: `say` on macOS,
//!   `spd-say` on Linux), [`engine::engine_available`] says whether there is one, and
//!   [`engine::FakeEngine`] makes none (tests).
//! - [`player`]: the queue and what is being said ([`player::Machine`], pure), and the driver
//!   that makes an engine say it ([`player::Player`]).
//! - [`state`]: what the device keeps (`speech-state.json`): its id, live switches,
//!   watermarks, voice and rate.
//! - [`link`]: how hubs are reached: this machine's ([`link::LinkSpec::Local`]), a remote one's
//!   over SSH ([`link::LinkSpec::Ssh`], `twr speech feed --proto` on the other end), or by hand
//!   ([`link::manual`]).
//! - [`speaker`]: [`speaker::run`] ties them together; [`speaker::tty_keys`] reads `s`, `p`,
//!   `n`, `q` in a terminal.
//! - [`service`]: the LaunchAgent or systemd unit `twr speak --print-service` prints.
//!
//! A speaker never listens on a socket: it connects out (a hub's socket, or ssh) and nothing
//! else. The text it gets is a log event's; it goes to an engine's stdin, never into a
//! command line.

pub mod engine;
pub mod link;
pub mod player;
pub mod service;
pub mod speaker;
pub mod state;

pub use engine::{engine_available, from_config, Engine, Prefs};
pub use link::{LinkSpec, SshSpec, Workspaces};
pub use speaker::{raw_mode, run, tty_keys, Key, KeyInput, Reporter, SpeakerOpts};
pub use state::StateStore;

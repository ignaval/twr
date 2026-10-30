//! The channel between the hub and an SSH box's daemon: the hub's box key ([`HubKey`]), the
//! two-line key exchange that opens the channel, and the sealed byte stream after it
//! ([`SealedStream`]), inside which the runner protocol runs unchanged (WebSocket and all).
//!
//! # Why
//!
//! The hub reaches an SSH box's daemon through `ssh` and a relay on the box that connects to
//! the daemon's Unix socket. Any process on the box runs as the same user as the daemon (the
//! agents among them) and can connect to that socket too: it could relay a genuine hub's
//! handshake and then inject commands into the session. So the daemon trusts nothing but what
//! the hub's key signed, and everything after the key exchange is encrypted and authenticated
//! end to end between the hub and the daemon, with keys a relay in between never learns.
//!
//! # Opening
//!
//! 1. The daemon sends one line: `TWR-BOX-HELLO {"v":1,"box":..,"nonce":..,"eph":..,"build":..}`
//!    ([`DaemonHello`]): its box, a fresh nonce (64 hex digits), a fresh X25519 public key
//!    (64 hex digits) and its build id ([`crate::build_id`], or `null`). The hub skips what
//!    comes before it (a login script on the box may print), at most [`MAX_JUNK_LINES`] lines
//!    and [`MAX_JUNK_BYTES`] bytes.
//! 2. The hub answers one line: `TWR-BOX-KEY {"nonce":..,"eph":..,"sig":..}`: its own nonce and
//!    X25519 public key, and its Ed25519 signature (128 hex digits) over the transcript:
//!    `twr box channel v1`, NUL, then the box, the daemon's nonce, the hub's nonce, the
//!    daemon's public key, the hub's public key and the hub's [`HubKey`] public key (all as
//!    the hex the lines carry), separated by NUL bytes.
//! 3. The daemon checks the signature with the hub key it was started with. Both sides then
//!    compute the X25519 shared secret and derive one ChaCha20-Poly1305 key per direction
//!    with HKDF-SHA256 (salt: both nonces' bytes; info: the direction and the transcript's
//!    SHA-256).
//!
//! Each line is JSON after its fixed prefix, ends with `\n` and has at most [`MAX_LINE_BYTES`]
//! bytes. Lines are read one byte at a time, so neither side reads past its line into the
//! sealed records that follow it.
//!
//! The hub does not authenticate the box side: a process on the box that stops the daemon
//! and answers in its place poses as that box's runner (as one that reads a token runner's
//! token file does). What it cannot do is read or change a session between the hub and the
//! genuine daemon.
//!
//! Neither side times out here: callers bound [`hub_side`] and [`daemon_side`] with their
//! own timeouts.
//!
//! # Records
//!
//! After the key line, each direction is a sequence of records: a `u32` (big-endian) length,
//! then that many bytes of ciphertext and its 16-byte tag. A record holds at most
//! [`MAX_RECORD_PLAINTEXT`] bytes of plaintext. Its nonce is 4 zero bytes and a 64-bit
//! big-endian counter of the direction's records, starting at 0, so a record that was
//! altered, replayed, reordered or dropped fails to open, and the stream fails for good.

use std::fmt;
use std::io;
use std::path::Path;
use std::pin::Pin;
use std::task::{ready, Context, Poll};

use anyhow::Context as _;
use ring::aead::{self, Aad, LessSafeKey, Nonce, UnboundKey};
use ring::agreement::{self, EphemeralPrivateKey, UnparsedPublicKey};
use ring::rand::SystemRandom;
use ring::signature::{Ed25519KeyPair, KeyPair};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};

use crate::runner::{new_nonce, valid_nonce, validate_box_name};

/// Version of the channel ([`DaemonHello::v`]).
pub const CHANNEL_VERSION: u32 = 1;

/// Prefix of the daemon's line.
pub const HELLO_PREFIX: &str = "TWR-BOX-HELLO ";

/// Prefix of the hub's line.
pub const KEY_PREFIX: &str = "TWR-BOX-KEY ";

/// Longest line of the opening, its `\n` included (the hello and the key line, and each line
/// the hub skips before the hello).
pub const MAX_LINE_BYTES: usize = 4096;

/// Most lines the hub skips before the daemon's hello.
pub const MAX_JUNK_LINES: usize = 64;

/// Most bytes the hub skips before the daemon's hello, in all.
pub const MAX_JUNK_BYTES: usize = 16 << 10;

/// Most plaintext in one record.
pub const MAX_RECORD_PLAINTEXT: usize = 64 << 10;

/// Length of a record's tag (ChaCha20-Poly1305).
const TAG_LEN: usize = 16;

/// Longest record on the wire: its length, the most plaintext, the tag.
const MAX_RECORD_BYTES: usize = 4 + MAX_RECORD_PLAINTEXT + TAG_LEN;

/// What the hub's signature starts with.
const TRANSCRIPT_LABEL: &[u8] = b"twr box channel v1\0";

/// HKDF info of the hub-to-daemon key (the transcript's SHA-256 follows it).
const HUB_TO_DAEMON: &[u8] = b"twr box channel v1 hub->daemon";

/// HKDF info of the daemon-to-hub key (the transcript's SHA-256 follows it).
const DAEMON_TO_HUB: &[u8] = b"twr box channel v1 daemon->hub";

/// The hub's box key: an Ed25519 key pair, kept as PKCS#8 in a file of the hub's data
/// directory. Its public key goes to the box on the ssh command line (it is public), and its
/// signature opens the channel ([`hub_side`]); the private key never leaves the hub's
/// machine, so nothing on a box can sign. `Debug` shows the public key's id only.
pub struct HubKey {
    pair: Ed25519KeyPair,
}

impl HubKey {
    /// Loads the key at `path`, or creates it there when there is none: a new key pair,
    /// written with mode 0600 to a temporary file in the same directory and linked into
    /// place only if `path` still does not exist, so a concurrent creator's key wins whole
    /// and a crash leaves no partial key. The directory must exist.
    ///
    /// Refused: a `path` that is not a regular file (a symbolic link included), one group or
    /// others can access (it signs for the hub: `chmod 600` it), and one that holds no
    /// Ed25519 PKCS#8 key.
    pub fn load_or_create(path: &Path) -> anyhow::Result<HubKey> {
        // Created here or by someone else meanwhile, the key in place is read back, as any
        // later start reads it. Three rounds: only a file removed as soon as it appears
        // keeps this going.
        for _ in 0..3 {
            if let Some(key) = Self::load(path)? {
                return Ok(key);
            }
            Self::create(path)?;
        }
        anyhow::bail!("{} keeps disappearing", path.display())
    }

    /// A key that lives only in memory (tests, and callers that keep it elsewhere).
    pub fn generate() -> anyhow::Result<HubKey> {
        let doc = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new())
            .map_err(|_| anyhow::anyhow!("the OS gave no random bytes for a key"))?;
        let pair = Ed25519KeyPair::from_pkcs8(doc.as_ref())
            .map_err(|_| anyhow::anyhow!("ring refused the key it generated"))?;
        Ok(HubKey { pair })
    }

    /// Reads the key at `path`: `None` when the file does not exist.
    fn load(path: &Path) -> anyhow::Result<Option<HubKey>> {
        use std::io::Read;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        // O_NOFOLLOW: a symbolic link is refused, not followed; O_NONBLOCK: a FIFO in its
        // place does not hang the open. The checks below are on the file as opened, so the
        // file checked is the file read.
        let f = match std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
            .open(path)
        {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) if e.raw_os_error() == Some(libc::ELOOP) => {
                anyhow::bail!(
                    "{} is a symbolic link: the hub's box key must be a regular file",
                    path.display()
                )
            }
            Err(e) => return Err(e).with_context(|| format!("opening {}", path.display())),
        };
        let meta = f
            .metadata()
            .with_context(|| format!("reading {}", path.display()))?;
        if !meta.is_file() {
            anyhow::bail!(
                "{} is not a regular file: the hub's box key must be one",
                path.display()
            );
        }
        let mode = meta.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            anyhow::bail!(
                "{} is the hub's box key and other users can access it (mode {mode:03o}): run \
                 `chmod 600 {}`",
                path.display(),
                crate::paths::sh_word(&path.to_string_lossy())
            );
        }
        let mut bytes = Vec::new();
        f.take(64 << 10)
            .read_to_end(&mut bytes)
            .with_context(|| format!("reading {}", path.display()))?;
        let pair = Ed25519KeyPair::from_pkcs8(&bytes).map_err(|_| {
            anyhow::anyhow!(
                "{} holds no Ed25519 key (PKCS#8): it is not the hub's box key; move it away \
                 and the hub makes a new one",
                path.display()
            )
        })?;
        Ok(Some(HubKey { pair }))
    }

    /// Creates a new key at `path` unless it exists (then it leaves it).
    fn create(path: &Path) -> anyhow::Result<()> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let doc = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new())
            .map_err(|_| anyhow::anyhow!("the OS gave no random bytes for a key"))?;
        let dir = match path.parent() {
            Some(d) if !d.as_os_str().is_empty() => d,
            _ => Path::new("."),
        };
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        let tmp = dir.join(format!(".{name}.{}.tmp", &new_nonce()?[..16]));
        let written = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC)
            .open(&tmp)
            .and_then(|mut f| {
                f.write_all(doc.as_ref())?;
                f.sync_all()
            });
        if let Err(e) = written {
            let _ = std::fs::remove_file(&tmp);
            return Err(e).with_context(|| format!("creating {}", tmp.display()));
        }
        // A hard link fails when `path` exists: the first creator's key stays.
        let linked = std::fs::hard_link(&tmp, path);
        let _ = std::fs::remove_file(&tmp);
        match linked {
            Ok(()) => std::fs::File::open(dir)
                .and_then(|d| d.sync_all())
                .with_context(|| format!("syncing {}", dir.display())),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
            Err(e) => Err(e).with_context(|| format!("creating {}", path.display())),
        }
    }

    /// The public key (32 bytes).
    pub fn public(&self) -> [u8; 32] {
        let mut out = [0u8; 32];
        out.copy_from_slice(self.pair.public_key().as_ref());
        out
    }

    /// The public key, lowercase hex (64 digits): what the relay and the daemon on a box are
    /// started with.
    pub fn public_hex(&self) -> String {
        hex::encode(self.public())
    }

    /// The key's id ([`hub_id`] of its public key): names the box daemon's directories on a
    /// box, so two hubs sharing a box get separate daemons.
    pub fn id(&self) -> String {
        hub_id(&self.public())
    }

    /// Signs `msg` (Ed25519).
    pub fn sign(&self, msg: &[u8]) -> [u8; 64] {
        let mut out = [0u8; 64];
        out.copy_from_slice(self.pair.sign(msg).as_ref());
        out
    }
}

impl fmt::Debug for HubKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HubKey")
            .field("id", &self.id())
            .field("private", &"<redacted>")
            .finish()
    }
}

/// A hub key's id: the first 16 lowercase hex digits of the SHA-256 of its public key.
pub fn hub_id(public: &[u8; 32]) -> String {
    hex::encode(&ring::digest::digest(&ring::digest::SHA256, public).as_ref()[..8])
}

/// Parses a hub public key given as 64 lowercase hex digits (`--hub-key`).
pub fn parse_hub_public(s: &str) -> anyhow::Result<[u8; 32]> {
    hex32(s).ok_or_else(|| {
        anyhow::anyhow!("{s:?} is not a hub key (64 lowercase hex digits: an Ed25519 public key)")
    })
}

/// `s` as 32 bytes, when it is exactly 64 lowercase hex digits.
fn hex32(s: &str) -> Option<[u8; 32]> {
    if !valid_nonce(s) {
        return None;
    }
    let mut out = [0u8; 32];
    hex::decode_to_slice(s, &mut out).ok()?;
    Some(out)
}

/// The daemon's line ([`HELLO_PREFIX`]), which opens the channel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonHello {
    /// [`CHANNEL_VERSION`] of the daemon.
    pub v: u32,
    /// The box the daemon serves.
    #[serde(rename = "box")]
    pub box_name: String,
    /// The daemon's fresh nonce (64 lowercase hex digits).
    pub nonce: String,
    /// The daemon's fresh X25519 public key (64 lowercase hex digits).
    pub eph: String,
    /// The daemon's build id ([`crate::build_id`]), when it knows it. Not signed: the hub
    /// uses it to tell which build answers, not to trust it.
    pub build: Option<String>,
}

/// The hub's line ([`KEY_PREFIX`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct HubKeyLine {
    /// The hub's fresh nonce (64 lowercase hex digits).
    nonce: String,
    /// The hub's fresh X25519 public key (64 lowercase hex digits).
    eph: String,
    /// The hub key's Ed25519 signature over the transcript (128 lowercase hex digits).
    sig: String,
}

/// What the hub signs ([module docs](self)): the label, then the box, both nonces, both
/// X25519 public keys and the hub's public key, separated by NUL bytes (none of them can
/// hold one: a valid box name and hex).
fn transcript(
    box_name: &str,
    daemon_nonce: &str,
    hub_nonce: &str,
    daemon_eph: &str,
    hub_eph: &str,
    hub_public_hex: &str,
) -> Vec<u8> {
    let mut t = TRANSCRIPT_LABEL.to_vec();
    for (i, part) in [
        box_name,
        daemon_nonce,
        hub_nonce,
        daemon_eph,
        hub_eph,
        hub_public_hex,
    ]
    .into_iter()
    .enumerate()
    {
        if i > 0 {
            t.push(0);
        }
        t.extend_from_slice(part.as_bytes());
    }
    t
}

/// The two direction keys: (hub to daemon, daemon to hub).
struct Keys {
    hub_to_daemon: LessSafeKey,
    daemon_to_hub: LessSafeKey,
}

/// Derives [`Keys`] from the X25519 exchange of `mine` with `peer_eph` (see the module docs).
fn derive_keys(
    mine: EphemeralPrivateKey,
    peer_eph: &[u8; 32],
    daemon_nonce: &[u8; 32],
    hub_nonce: &[u8; 32],
    transcript: &[u8],
) -> anyhow::Result<Keys> {
    let digest = ring::digest::digest(&ring::digest::SHA256, transcript);
    let mut salt = [0u8; 64];
    salt[..32].copy_from_slice(daemon_nonce);
    salt[32..].copy_from_slice(hub_nonce);
    let peer = UnparsedPublicKey::new(&agreement::X25519, peer_eph);
    agreement::agree_ephemeral(
        mine,
        &peer,
        |shared| -> Result<Keys, ring::error::Unspecified> {
            let prk = ring::hkdf::Salt::new(ring::hkdf::HKDF_SHA256, &salt).extract(shared);
            let key = |label: &[u8]| -> Result<LessSafeKey, ring::error::Unspecified> {
                let info = [label, digest.as_ref()];
                let okm = prk.expand(&info, &aead::CHACHA20_POLY1305)?;
                Ok(LessSafeKey::new(UnboundKey::from(okm)))
            };
            Ok(Keys {
                hub_to_daemon: key(HUB_TO_DAEMON)?,
                daemon_to_hub: key(DAEMON_TO_HUB)?,
            })
        },
    )
    .and_then(|r| r)
    .map_err(|_| anyhow::anyhow!("the key exchange failed (an invalid X25519 public key)"))
}

/// A fresh X25519 key pair: the private key and the public key's hex.
fn ephemeral() -> anyhow::Result<(EphemeralPrivateKey, String)> {
    let rng = SystemRandom::new();
    let private = EphemeralPrivateKey::generate(&agreement::X25519, &rng)
        .map_err(|_| anyhow::anyhow!("the OS gave no random bytes for a key"))?;
    let public = private
        .compute_public_key()
        .map_err(|_| anyhow::anyhow!("computing an X25519 public key failed"))?;
    Ok((private, hex::encode(public.as_ref())))
}

/// Reads one `\n`-terminated line, one byte at a time (so nothing after it is consumed),
/// without the `\n`. `Ok(None)`: the stream ended before the line's first byte. An error when
/// the line, `\n` included, would pass `max` bytes (`InvalidData`) or the stream ends inside
/// it (`UnexpectedEof`).
async fn read_line<S: AsyncRead + Unpin>(s: &mut S, max: usize) -> io::Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    loop {
        let mut b = [0u8; 1];
        if s.read(&mut b).await? == 0 {
            if line.is_empty() {
                return Ok(None);
            }
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "the connection ended in the middle of a line",
            ));
        }
        if b[0] == b'\n' {
            return Ok(Some(line));
        }
        if line.len() + 2 > max {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("a line longer than {max} bytes"),
            ));
        }
        line.push(b[0]);
    }
}

/// Writes `prefix`, `msg` as JSON and `\n`, and flushes.
async fn write_line<S: AsyncWrite + Unpin, T: Serialize>(
    s: &mut S,
    prefix: &str,
    msg: &T,
) -> anyhow::Result<()> {
    let mut line = prefix.as_bytes().to_vec();
    serde_json::to_writer(&mut line, msg)?;
    line.push(b'\n');
    anyhow::ensure!(
        line.len() <= MAX_LINE_BYTES,
        "a line of {} bytes: more than {MAX_LINE_BYTES}",
        line.len()
    );
    s.write_all(&line).await?;
    s.flush().await?;
    Ok(())
}

/// The daemon's hello for `box_name`, read from `stream`: lines before it are skipped (at
/// most [`MAX_JUNK_LINES`] lines and [`MAX_JUNK_BYTES`] bytes), as is anything before
/// [`HELLO_PREFIX`] on its own line, and it is checked (version, box, nonce, X25519 key,
/// build id).
async fn read_hello<S: AsyncRead + Unpin>(
    stream: &mut S,
    box_name: &str,
) -> anyhow::Result<DaemonHello> {
    let (mut lines, mut bytes) = (0usize, 0usize);
    let mut last: Option<String> = None;
    let line = loop {
        let line = read_line(stream, MAX_LINE_BYTES)
            .await
            .with_context(|| format!("box {box_name}: reading twr's hello"))?;
        let Some(line) = line else {
            let seen = match &last {
                Some(l) => format!(" (its last line: {l:?})"),
                None => String::new(),
            };
            anyhow::bail!("box {box_name}: the connection ended before twr's hello{seen}");
        };
        // Anywhere in the line: what a login script printed without a newline at its end
        // comes before it.
        if let Some(at) = line
            .windows(HELLO_PREFIX.len())
            .position(|w| w == HELLO_PREFIX.as_bytes())
        {
            break line[at..].to_vec();
        }
        lines += 1;
        bytes += line.len() + 1;
        if lines > MAX_JUNK_LINES || bytes > MAX_JUNK_BYTES {
            anyhow::bail!(
                "box {box_name}: more than {MAX_JUNK_LINES} lines or {} KiB before twr's hello: \
                 a login script on the box prints too much (make it print nothing when there \
                 is no terminal)",
                MAX_JUNK_BYTES >> 10
            );
        }
        let shown: String = String::from_utf8_lossy(&line).chars().take(200).collect();
        last = Some(shown);
    };
    let json = &line[HELLO_PREFIX.len()..];
    let hello: DaemonHello = serde_json::from_slice(json)
        .with_context(|| format!("box {box_name}: twr's hello is not valid"))?;
    if hello.v != CHANNEL_VERSION {
        anyhow::bail!(
            "box {box_name}: its twr opens box channel version {}, this hub {CHANNEL_VERSION}",
            hello.v
        );
    }
    if hello.box_name != box_name {
        anyhow::bail!(
            "box {box_name}: the twr that answered serves box {:?}",
            hello.box_name
        );
    }
    if !valid_nonce(&hello.nonce) {
        anyhow::bail!("box {box_name}: twr's hello has a malformed nonce");
    }
    if hex32(&hello.eph).is_none() {
        anyhow::bail!("box {box_name}: twr's hello has a malformed key");
    }
    if let Some(b) = &hello.build {
        if !crate::valid_build_id(b) {
            anyhow::bail!("box {box_name}: twr's hello has a malformed build id");
        }
    }
    Ok(hello)
}

/// The hub's answer to `hello` (checked by [`read_hello`]): its key line and the keys.
fn hub_answer(key: &HubKey, hello: &DaemonHello) -> anyhow::Result<(HubKeyLine, Keys)> {
    let nonce = new_nonce()?;
    let (eph, eph_hex) = ephemeral()?;
    let t = transcript(
        &hello.box_name,
        &hello.nonce,
        &nonce,
        &hello.eph,
        &eph_hex,
        &key.public_hex(),
    );
    let sig = hex::encode(key.sign(&t));
    let (Some(daemon_nonce), Some(hub_nonce), Some(daemon_eph)) =
        (hex32(&hello.nonce), hex32(&nonce), hex32(&hello.eph))
    else {
        anyhow::bail!(
            "box {}: twr's hello has a malformed nonce or key",
            hello.box_name
        );
    };
    let keys = derive_keys(eph, &daemon_eph, &daemon_nonce, &hub_nonce, &t)?;
    Ok((
        HubKeyLine {
            nonce,
            eph: eph_hex,
            sig,
        },
        keys,
    ))
}

/// Opens the channel on the hub's side of `stream` (the ssh child's stdout and stdin): reads
/// the daemon's hello for `box_name` (skipping what a login script printed before it), sends
/// the key line signed with `key`, and returns the sealed stream and the hello. The hub
/// learns whether the daemon accepted its key from what comes (or does not) through the
/// sealed stream. No timeout: the caller's.
pub async fn hub_side<S: AsyncRead + AsyncWrite + Unpin>(
    mut stream: S,
    key: &HubKey,
    box_name: &str,
) -> anyhow::Result<(SealedStream<S>, DaemonHello)> {
    validate_box_name(box_name)?;
    let hello = read_hello(&mut stream, box_name).await?;
    let (line, keys) = hub_answer(key, &hello)?;
    write_line(&mut stream, KEY_PREFIX, &line)
        .await
        .with_context(|| format!("box {box_name}: sending the hub's key line"))?;
    let sealed = SealedStream::new(stream, keys.hub_to_daemon, keys.daemon_to_hub);
    Ok((sealed, hello))
}

/// Opens the channel on the daemon's side of `stream` (a relay's connection): sends the hello
/// for `box_name` with `build` (a build id), reads the hub's key line (exactly one line, read
/// byte by byte: the sealed records that may follow it in the same write stay unread) and
/// checks its signature with `hub_public`, the key the daemon was started with. Anything
/// else ends the connection with an error, before a byte of the runner protocol. No timeout:
/// the caller's.
pub async fn daemon_side<S: AsyncRead + AsyncWrite + Unpin>(
    mut stream: S,
    hub_public: &[u8; 32],
    box_name: &str,
    build: Option<&str>,
) -> anyhow::Result<SealedStream<S>> {
    validate_box_name(box_name)?;
    if let Some(b) = build {
        anyhow::ensure!(crate::valid_build_id(b), "{b:?} is not a build id");
    }
    let nonce = new_nonce()?;
    let (eph, eph_hex) = ephemeral()?;
    let hello = DaemonHello {
        v: CHANNEL_VERSION,
        box_name: box_name.to_string(),
        nonce: nonce.clone(),
        eph: eph_hex.clone(),
        build: build.map(str::to_string),
    };
    write_line(&mut stream, HELLO_PREFIX, &hello)
        .await
        .context("sending the box daemon's hello")?;
    let line = read_line(&mut stream, MAX_LINE_BYTES)
        .await
        .context("reading the hub's key line")?
        .ok_or_else(|| anyhow::anyhow!("the connection ended before the hub's key line"))?;
    let json = line
        .strip_prefix(KEY_PREFIX.as_bytes())
        .ok_or_else(|| anyhow::anyhow!("the peer sent something other than the hub's key line"))?;
    let reply: HubKeyLine =
        serde_json::from_slice(json).context("the hub's key line is not valid")?;
    let (Some(hub_nonce), Some(hub_eph)) = (hex32(&reply.nonce), hex32(&reply.eph)) else {
        anyhow::bail!("the hub's key line has a malformed nonce or key");
    };
    let mut sig = [0u8; 64];
    if reply.sig.len() != 128
        || !reply
            .sig
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        || hex::decode_to_slice(&reply.sig, &mut sig).is_err()
    {
        anyhow::bail!("the hub's key line has a malformed signature");
    }
    let t = transcript(
        box_name,
        &nonce,
        &reply.nonce,
        &eph_hex,
        &reply.eph,
        &hex::encode(hub_public),
    );
    ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, hub_public)
        .verify(&t, &sig)
        .map_err(|_| {
            anyhow::anyhow!(
                "the hub's key line is not signed by hub key {} (the one this daemon serves): \
                 another hub, or something posing as one",
                hub_id(hub_public)
            )
        })?;
    let daemon_nonce = hex32(&nonce).expect("new_nonce makes 64 hex digits");
    let keys = derive_keys(eph, &hub_eph, &daemon_nonce, &hub_nonce, &t)?;
    Ok(SealedStream::new(
        stream,
        keys.daemon_to_hub,
        keys.hub_to_daemon,
    ))
}

/// One direction's key and record counter.
struct Direction {
    key: LessSafeKey,
    /// The next record's number (its nonce).
    counter: u64,
}

impl Direction {
    fn new(key: LessSafeKey) -> Direction {
        Direction { key, counter: 0 }
    }

    /// The next record's nonce, counting it. The counter never wraps: its last value is
    /// refused rather than used, so no nonce repeats.
    fn next_nonce(&mut self) -> io::Result<Nonce> {
        let n = self.counter;
        self.counter = n.checked_add(1).ok_or_else(|| {
            io::Error::other("the sealed channel used up its record counter: reconnect")
        })?;
        let mut nonce = [0u8; 12];
        nonce[4..].copy_from_slice(&n.to_be_bytes());
        Ok(Nonce::assume_unique_for_key(nonce))
    }
}

/// The reading half's state.
struct Reader {
    dir: Direction,
    /// Bytes from the inner stream: `buf[start..end]` not yet opened, and the plaintext of
    /// the last record opened (in place) at `buf[pstart..pend]`, before `start`.
    buf: Vec<u8>,
    start: usize,
    end: usize,
    pstart: usize,
    pend: usize,
    /// The inner stream ended at a record boundary.
    eof: bool,
    /// A record failed (it did not open, its length was impossible, the stream ended inside
    /// it): the stream fails from then on, with this kind.
    failed: Option<io::ErrorKind>,
}

impl Reader {
    /// Opens the next whole record in `buf[start..end]`, if there is one: `Ok(true)` with its
    /// plaintext at `buf[pstart..pend]`, `Ok(false)` when more bytes are needed (with room
    /// made for them after `end`).
    fn open_next(&mut self) -> io::Result<bool> {
        let avail = self.end - self.start;
        if avail < 4 {
            self.make_room(4);
            return Ok(false);
        }
        let mut len = [0u8; 4];
        len.copy_from_slice(&self.buf[self.start..self.start + 4]);
        let len = u32::from_be_bytes(len) as usize;
        if !(TAG_LEN..=MAX_RECORD_PLAINTEXT + TAG_LEN).contains(&len) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("a sealed record of {len} bytes: not a record of this channel"),
            ));
        }
        if avail < 4 + len {
            self.make_room(4 + len);
            return Ok(false);
        }
        let nonce = self.dir.next_nonce()?;
        let body = &mut self.buf[self.start + 4..self.start + 4 + len];
        let plain = self
            .dir
            .key
            .open_in_place(nonce, Aad::empty(), body)
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "a sealed record did not open: it was altered, replayed, reordered or \
                     follows a dropped one",
                )
            })?
            .len();
        self.pstart = self.start + 4;
        self.pend = self.pstart + plain;
        self.start += 4 + len;
        Ok(true)
    }

    /// Makes room for a record of `need` bytes starting at `start` (called only when the
    /// last record's plaintext was all read, so what lies before `start` is spent).
    fn make_room(&mut self, need: usize) {
        if self.start == self.end {
            self.start = 0;
            self.end = 0;
        } else if self.start + need > self.buf.len() {
            self.buf.copy_within(self.start..self.end, 0);
            self.end -= self.start;
            self.start = 0;
        }
        self.pstart = 0;
        self.pend = 0;
    }
}

/// The writing half's state.
struct Writer {
    dir: Direction,
    /// Sealed records not yet taken by the inner stream: `buf[pos..]`.
    buf: Vec<u8>,
    pos: usize,
    /// An error of the inner stream met while pushing sealed bytes on behalf of a write that
    /// had already succeeded: the next write, flush or shutdown returns it.
    err: Option<io::Error>,
}

impl Writer {
    /// Seals `plain` (at most [`MAX_RECORD_PLAINTEXT`] bytes, not empty) as one record after
    /// what is pending.
    fn seal(&mut self, plain: &[u8]) -> io::Result<()> {
        debug_assert!(!plain.is_empty() && plain.len() <= MAX_RECORD_PLAINTEXT);
        let nonce = self.dir.next_nonce()?;
        if self.pos == self.buf.len() {
            self.buf.clear();
            self.pos = 0;
        }
        let at = self.buf.len();
        let len = u32::try_from(plain.len() + TAG_LEN).expect("a record is under 4 GiB");
        self.buf.extend_from_slice(&len.to_be_bytes());
        self.buf.extend_from_slice(plain);
        let tag = match self.dir.key.seal_in_place_separate_tag(
            nonce,
            Aad::empty(),
            &mut self.buf[at + 4..],
        ) {
            Ok(tag) => tag,
            Err(_) => {
                self.buf.truncate(at);
                return Err(io::Error::other("sealing a record failed"));
            }
        };
        self.buf.extend_from_slice(tag.as_ref());
        Ok(())
    }

    /// Writes the pending sealed bytes to `inner` until it has taken them all.
    fn poll_drain<S: AsyncWrite + Unpin>(
        &mut self,
        inner: &mut S,
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<()>> {
        if let Some(e) = self.err.take() {
            return Poll::Ready(Err(e));
        }
        while self.pos < self.buf.len() {
            let n = ready!(Pin::new(&mut *inner).poll_write(cx, &self.buf[self.pos..]))?;
            if n == 0 {
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "the connection took no more sealed bytes",
                )));
            }
            self.pos += n;
        }
        self.buf.clear();
        self.pos = 0;
        Poll::Ready(Ok(()))
    }
}

/// The byte stream of an open channel ([`hub_side`], [`daemon_side`]) over `S`: what is
/// written goes out as sealed records, what is read is the peer's records opened, in order.
/// Any record that fails to open fails the read with `InvalidData` (and every read after
/// it); the inner stream ending inside a record fails it with `UnexpectedEof`; ending between
/// records is the end of the stream.
///
/// A write seals up to [`MAX_RECORD_PLAINTEXT`] bytes as one record and returns how many it
/// took; the record may still be pending in the stream until a later write or a flush pushes
/// it out, so flush (WebSocket does) to send. A write first finishes pushing the pending
/// record. Needs `S: Unpin` (no unsafe pinning); it is `Unpin` itself.
pub struct SealedStream<S> {
    inner: S,
    r: Reader,
    w: Writer,
}

impl<S> SealedStream<S> {
    /// A sealed stream over `inner` that seals with `seal` and opens with `open`.
    fn new(inner: S, seal: LessSafeKey, open: LessSafeKey) -> SealedStream<S> {
        SealedStream {
            inner,
            r: Reader {
                dir: Direction::new(open),
                buf: vec![0u8; MAX_RECORD_BYTES + (64 << 10)],
                start: 0,
                end: 0,
                pstart: 0,
                pend: 0,
                eof: false,
                failed: None,
            },
            w: Writer {
                dir: Direction::new(seal),
                buf: Vec::new(),
                pos: 0,
                err: None,
            },
        }
    }

    /// The stream underneath (writing to it directly would break the channel).
    pub fn get_ref(&self) -> &S {
        &self.inner
    }
}

impl<S> fmt::Debug for SealedStream<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SealedStream")
            .field("records_sent", &self.w.dir.counter)
            .field("records_received", &self.r.dir.counter)
            .finish_non_exhaustive()
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for SealedStream<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        out: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if out.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        let r = &mut this.r;
        loop {
            if r.pstart < r.pend {
                let n = out.remaining().min(r.pend - r.pstart);
                out.put_slice(&r.buf[r.pstart..r.pstart + n]);
                r.pstart += n;
                return Poll::Ready(Ok(()));
            }
            if let Some(kind) = r.failed {
                return Poll::Ready(Err(io::Error::new(
                    kind,
                    "the sealed channel failed earlier",
                )));
            }
            if r.eof {
                return Poll::Ready(Ok(()));
            }
            match r.open_next() {
                Ok(true) => continue,
                Ok(false) => {}
                Err(e) => {
                    r.failed = Some(e.kind());
                    return Poll::Ready(Err(e));
                }
            }
            let mut rb = ReadBuf::new(&mut r.buf[r.end..]);
            ready!(Pin::new(&mut this.inner).poll_read(cx, &mut rb))?;
            let n = rb.filled().len();
            if n == 0 {
                if r.start == r.end {
                    r.eof = true;
                    continue;
                }
                r.failed = Some(io::ErrorKind::UnexpectedEof);
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "the connection ended in the middle of a sealed record",
                )));
            }
            r.end += n;
        }
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for SealedStream<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        // The previous record first: a write seals new bytes only once nothing is pending,
        // so what is pending never grows past one record.
        ready!(this.w.poll_drain(&mut this.inner, cx))?;
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        let n = buf.len().min(MAX_RECORD_PLAINTEXT);
        this.w.seal(&buf[..n])?;
        // The record is ours now: push what the inner stream takes at once. Pending leaves
        // the rest for the next write or flush; an error waits for it too.
        if let Poll::Ready(Err(e)) = this.w.poll_drain(&mut this.inner, cx) {
            this.w.err = Some(e);
        }
        Poll::Ready(Ok(n))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        ready!(this.w.poll_drain(&mut this.inner, cx))?;
        Pin::new(&mut this.inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        ready!(this.w.poll_drain(&mut this.inner, cx))?;
        Pin::new(&mut this.inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests;

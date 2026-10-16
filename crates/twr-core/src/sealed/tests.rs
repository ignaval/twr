use super::*;

use tokio::io::{duplex, AsyncReadExt, AsyncWriteExt, DuplexStream};

/// The hub and daemon ends of an open channel for box `gpu` over an in-memory pipe of
/// `capacity` bytes each way.
async fn open_pair(
    capacity: usize,
) -> (
    SealedStream<DuplexStream>,
    SealedStream<DuplexStream>,
    DaemonHello,
) {
    let key = HubKey::generate().unwrap();
    let (hub_end, daemon_end) = duplex(capacity);
    let public = key.public();
    let (hub, daemon) = tokio::join!(
        hub_side(hub_end, &key, "gpu"),
        daemon_side(daemon_end, &public, "gpu", Some("0123456789abcdef")),
    );
    let (hub, hello) = hub.unwrap();
    (hub, daemon.unwrap(), hello)
}

/// Byte `i` of a test payload: not periodic in any record or chunk size used here.
fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u32).wrapping_mul(2_654_435_761).rotate_left(7) as u8 ^ seed)
        .collect()
}

#[tokio::test]
async fn the_channel_opens_and_carries_bytes_both_ways() {
    let (mut hub, mut daemon, hello) = open_pair(64 << 10).await;
    assert_eq!(hello.v, CHANNEL_VERSION);
    assert_eq!(hello.box_name, "gpu");
    assert_eq!(hello.build.as_deref(), Some("0123456789abcdef"));

    hub.write_all(b"hello box").await.unwrap();
    hub.flush().await.unwrap();
    let mut buf = [0u8; 9];
    daemon.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"hello box");

    daemon.write_all(b"hello hub").await.unwrap();
    daemon.flush().await.unwrap();
    hub.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"hello hub");

    // Exactly one record's worth, then one byte more (two records).
    for len in [MAX_RECORD_PLAINTEXT, MAX_RECORD_PLAINTEXT + 1] {
        let data = pattern(len, 1);
        let (wrote, read) = tokio::join!(
            async {
                hub.write_all(&data).await.unwrap();
                hub.flush().await.unwrap();
            },
            async {
                let mut got = vec![0u8; len];
                daemon.read_exact(&mut got).await.unwrap();
                got
            }
        );
        let () = wrote;
        assert!(read == data, "{len} bytes");
    }

    // The hub's end shut down: the daemon reads the end of the stream, at a record boundary.
    hub.shutdown().await.unwrap();
    drop(hub);
    let mut rest = Vec::new();
    daemon.read_to_end(&mut rest).await.unwrap();
    assert!(rest.is_empty());
}

/// Megabytes each way at once, written in odd-sized chunks and read in others, through a
/// small pipe (so writes and reads wait on each other all the time).
#[tokio::test]
async fn megabytes_both_ways_at_once_in_odd_chunks() {
    let (hub, daemon, _) = open_pair(8 << 10).await;
    let (mut hub_r, mut hub_w) = tokio::io::split(hub);
    let (mut daemon_r, mut daemon_w) = tokio::io::split(daemon);
    let len = (3 << 20) + 7;
    let down = pattern(len, 0x5a);
    let up = pattern(len, 0xa5);
    async fn send(w: &mut (impl AsyncWrite + Unpin), data: &[u8]) {
        let chunks = [1usize, 7, 4093, 65536, 65537, 100_003];
        let (mut at, mut i) = (0, 0);
        while at < data.len() {
            let n = chunks[i % chunks.len()].min(data.len() - at);
            w.write_all(&data[at..at + n]).await.unwrap();
            if i % 3 == 0 {
                w.flush().await.unwrap();
            }
            at += n;
            i += 1;
        }
        w.flush().await.unwrap();
    }
    async fn receive(r: &mut (impl AsyncRead + Unpin), len: usize) -> Vec<u8> {
        let mut got = Vec::with_capacity(len);
        let mut buf = vec![0u8; 3333];
        while got.len() < len {
            let n = r.read(&mut buf).await.unwrap();
            assert!(n > 0, "the stream ended after {} bytes", got.len());
            got.extend_from_slice(&buf[..n]);
        }
        got
    }
    let ((), (), got_down, got_up) = tokio::join!(
        send(&mut hub_w, &down),
        send(&mut daemon_w, &up),
        receive(&mut daemon_r, len),
        receive(&mut hub_r, len),
    );
    assert!(got_down == down, "hub to daemon");
    assert!(got_up == up, "daemon to hub");
}

/// Messages of the sizes a WebSocket carries (length-prefixed here), up to the runner
/// protocol's largest.
#[tokio::test]
async fn websocket_sized_messages() {
    let (hub, daemon, _) = open_pair(256 << 10).await;
    let (mut hub_r, mut hub_w) = tokio::io::split(hub);
    let (mut daemon_r, mut daemon_w) = tokio::io::split(daemon);
    let sizes = [
        0usize,
        1,
        125,
        126,
        65535,
        65536,
        65537,
        1 << 20,
        crate::runner::MAX_FRAME_BYTES,
    ];
    let send = async {
        for (i, &n) in sizes.iter().enumerate() {
            let msg = pattern(n, i as u8);
            hub_w.write_all(&(n as u32).to_be_bytes()).await.unwrap();
            hub_w.write_all(&msg).await.unwrap();
            hub_w.flush().await.unwrap();
            // The answer comes back before the next message, as a request's would.
            let mut ack = [0u8; 4];
            hub_r.read_exact(&mut ack).await.unwrap();
            assert_eq!(u32::from_be_bytes(ack) as usize, n);
        }
    };
    let echo = async {
        for (i, &n) in sizes.iter().enumerate() {
            let mut len = [0u8; 4];
            daemon_r.read_exact(&mut len).await.unwrap();
            assert_eq!(u32::from_be_bytes(len) as usize, n);
            let mut msg = vec![0u8; n];
            daemon_r.read_exact(&mut msg).await.unwrap();
            assert!(msg == pattern(n, i as u8), "message {i} ({n} bytes)");
            daemon_w.write_all(&len).await.unwrap();
            daemon_w.flush().await.unwrap();
        }
    };
    tokio::join!(send, echo);
}

#[tokio::test]
async fn what_a_login_script_prints_before_the_hello_is_skipped() {
    let key = HubKey::generate().unwrap();
    let (hub_end, mut daemon_end) = duplex(64 << 10);
    // At the limit: 64 lines, 16 KiB.
    let noise = format!("{}\n", "y".repeat(255)).repeat(MAX_JUNK_LINES);
    assert_eq!(noise.len(), MAX_JUNK_BYTES);
    daemon_end.write_all(noise.as_bytes()).await.unwrap();
    // And a last message without its newline, on the hello's line.
    daemon_end.write_all(b"bye").await.unwrap();
    let public = key.public();
    let (hub, daemon) = tokio::join!(
        hub_side(hub_end, &key, "gpu"),
        daemon_side(daemon_end, &public, "gpu", None),
    );
    let (mut hub, hello) = hub.unwrap();
    assert_eq!(hello.build, None);
    let mut daemon = daemon.unwrap();
    daemon.write_all(b"ok").await.unwrap();
    daemon.flush().await.unwrap();
    let mut buf = [0u8; 2];
    hub.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"ok");
}

#[tokio::test]
async fn too_much_before_the_hello_is_refused() {
    let key = HubKey::generate().unwrap();
    let refused = |noise: Vec<u8>| {
        let key = &key;
        async move {
            let (hub_end, mut far) = duplex(64 << 10);
            // The hello itself would follow: the hub must give up before reading it.
            tokio::spawn(async move {
                let _ = far.write_all(&noise).await;
                let _ = far.write_all(b"TWR-BOX-HELLO {}\n").await;
                let _ = far.read_to_end(&mut Vec::new()).await;
            });
            format!("{:#}", hub_side(hub_end, key, "gpu").await.unwrap_err())
        }
    };
    let e = refused(b"x\n".repeat(MAX_JUNK_LINES + 1)).await;
    assert!(e.contains("login script"), "{e}");
    let e = refused(format!("{}\n", "y".repeat(4000)).repeat(5).into_bytes()).await;
    assert!(e.contains("login script"), "{e}");
    let e = refused(format!("{}\n", "y".repeat(MAX_LINE_BYTES)).into_bytes()).await;
    assert!(e.contains("longer than 4096 bytes"), "{e}");

    // The connection ended before the hello: the last line says why.
    let (hub_end, mut far) = duplex(1024);
    far.write_all(b"motd\nsh: 1: twr: not found\n")
        .await
        .unwrap();
    drop(far);
    let e = format!("{:#}", hub_side(hub_end, &key, "gpu").await.unwrap_err());
    assert!(
        e.contains("ended before twr's hello") && e.contains("twr: not found"),
        "{e}"
    );
}

#[tokio::test]
async fn a_daemon_refuses_another_hub_key() {
    let hub_key = HubKey::generate().unwrap();
    let other = HubKey::generate().unwrap();
    let (hub_end, daemon_end) = duplex(64 << 10);
    let public = other.public();
    let (hub, daemon) = tokio::join!(
        hub_side(hub_end, &hub_key, "gpu"),
        daemon_side(daemon_end, &public, "gpu", None),
    );
    let e = format!("{:#}", daemon.unwrap_err());
    assert!(e.contains("not signed by hub key"), "{e}");
    assert!(e.contains(&other.id()), "{e}");
    // The hub learns it from the channel: it ends before anything comes through it.
    let (mut hub, _) = hub.unwrap();
    let mut buf = Vec::new();
    assert_eq!(hub.read_to_end(&mut buf).await.unwrap(), 0);
}

#[tokio::test]
async fn a_hub_refuses_a_hello_for_another_box() {
    let key = HubKey::generate().unwrap();
    let (hub_end, daemon_end) = duplex(64 << 10);
    let public = key.public();
    let (hub, daemon) = tokio::join!(
        hub_side(hub_end, &key, "gpu"),
        daemon_side(daemon_end, &public, "cpu", None),
    );
    let e = format!("{:#}", hub.unwrap_err());
    assert!(e.contains("serves box \"cpu\""), "{e}");
    let e = format!("{:#}", daemon.unwrap_err());
    assert!(e.contains("ended before the hub's key line"), "{e}");
}

#[tokio::test]
async fn a_daemon_refuses_anything_but_a_key_line() {
    let key = HubKey::generate().unwrap();
    let public = key.public();
    for reply in [
        &b"hello\n"[..],
        b"TWR-BOX-KEY {\"nonce\":\"00\",\"eph\":\"00\",\"sig\":\"00\"}\n",
        b"TWR-BOX-KEY not json\n",
    ] {
        let (mut hub_end, daemon_end) = duplex(64 << 10);
        let daemon = daemon_side(daemon_end, &public, "gpu", None);
        let hub = async {
            read_hello(&mut hub_end, "gpu").await.unwrap();
            hub_end.write_all(reply).await.unwrap();
        };
        let (daemon, ()) = tokio::join!(daemon, hub);
        assert!(daemon.is_err(), "{}", String::from_utf8_lossy(reply));
    }
}

/// The hub's key line and its first sealed record arrive in one write: the daemon reads the
/// line only, and the record decodes in the sealed stream.
#[tokio::test]
async fn bytes_after_the_key_line_are_left_for_the_sealed_stream() {
    let key = HubKey::generate().unwrap();
    let public = key.public();
    let (mut hub_end, daemon_end) = duplex(64 << 10);
    let daemon = tokio::spawn(async move {
        let mut s = daemon_side(daemon_end, &public, "gpu", None).await?;
        let mut buf = [0u8; 5];
        s.read_exact(&mut buf).await?;
        anyhow::Ok(buf)
    });
    let hello = read_hello(&mut hub_end, "gpu").await.unwrap();
    let (line, keys) = hub_answer(&key, &hello).unwrap();
    let mut wire = KEY_PREFIX.as_bytes().to_vec();
    serde_json::to_writer(&mut wire, &line).unwrap();
    wire.push(b'\n');
    // One record sealed with the hub's key, as the hub's stream would seal it.
    let (a, mut b) = duplex(1024);
    let mut w = SealedStream::new(a, keys.hub_to_daemon, keys.daemon_to_hub);
    w.write_all(b"first").await.unwrap();
    w.flush().await.unwrap();
    let mut record = vec![0u8; 4 + 5 + TAG_LEN];
    b.read_exact(&mut record).await.unwrap();
    wire.extend_from_slice(&record);
    hub_end.write_all(&wire).await.unwrap();
    assert_eq!(&daemon.await.unwrap().unwrap(), b"first");
}

const K1: [u8; 32] = [1; 32];
const K2: [u8; 32] = [2; 32];

fn key(bytes: &[u8; 32]) -> LessSafeKey {
    LessSafeKey::new(UnboundKey::new(&aead::CHACHA20_POLY1305, bytes).unwrap())
}

/// The records a stream sealing with [`K1`] sends for `msgs` (one write and flush each).
async fn records(msgs: &[&[u8]]) -> Vec<Vec<u8>> {
    let (a, mut b) = duplex(1 << 20);
    let mut w = SealedStream::new(a, key(&K1), key(&K2));
    for m in msgs {
        w.write_all(m).await.unwrap();
        w.flush().await.unwrap();
    }
    drop(w);
    let mut raw = Vec::new();
    b.read_to_end(&mut raw).await.unwrap();
    let mut out = Vec::new();
    let mut at = 0;
    while at < raw.len() {
        let len = u32::from_be_bytes(raw[at..at + 4].try_into().unwrap()) as usize;
        out.push(raw[at..at + 4 + len].to_vec());
        at += 4 + len;
    }
    assert_eq!(out.len(), msgs.len());
    out
}

/// What a stream opening with [`K1`] reads from `wire` followed by the end of the stream:
/// the bytes, and the error that stopped it (`None`: a clean end). A read after the error
/// fails too.
async fn read_wire(wire: Vec<u8>) -> (Vec<u8>, Option<io::ErrorKind>) {
    let (mut a, b) = duplex(1 << 20);
    a.write_all(&wire).await.unwrap();
    drop(a);
    let mut r = SealedStream::new(b, key(&K2), key(&K1));
    let mut got = Vec::new();
    let mut buf = [0u8; 1024];
    loop {
        match r.read(&mut buf).await {
            Ok(0) => return (got, None),
            Ok(n) => got.extend_from_slice(&buf[..n]),
            Err(e) => {
                let again = r.read(&mut buf).await.unwrap_err();
                assert_eq!(again.kind(), e.kind(), "the stream stays failed");
                return (got, Some(e.kind()));
            }
        }
    }
}

#[tokio::test]
async fn altered_replayed_dropped_and_cut_records_fail() {
    let r = records(&[b"one", b"two", b"three"]).await;
    let cat = |parts: &[&Vec<u8>]| parts.iter().flat_map(|p| p.iter().copied()).collect();

    // As sent.
    assert_eq!(
        read_wire(cat(&[&r[0], &r[1], &r[2]])).await,
        (b"onetwothree".to_vec(), None)
    );
    // A flipped bit in the ciphertext, then in the tag.
    for at in [4, r[1].len() - 1] {
        let mut flipped = r[1].clone();
        flipped[at] ^= 0x01;
        assert_eq!(
            read_wire(cat(&[&r[0], &flipped, &r[2]])).await,
            (b"one".to_vec(), Some(io::ErrorKind::InvalidData)),
            "bit flipped at {at}"
        );
    }
    // Replayed.
    assert_eq!(
        read_wire(cat(&[&r[0], &r[0]])).await,
        (b"one".to_vec(), Some(io::ErrorKind::InvalidData))
    );
    // Dropped.
    assert_eq!(
        read_wire(cat(&[&r[0], &r[2]])).await,
        (b"one".to_vec(), Some(io::ErrorKind::InvalidData))
    );
    // Reordered.
    assert_eq!(
        read_wire(cat(&[&r[1], &r[0]])).await,
        (Vec::new(), Some(io::ErrorKind::InvalidData))
    );
    // Cut short, in the body and in the length.
    for cut in [r[1].len() - 1, 2] {
        let mut wire: Vec<u8> = cat(&[&r[0]]);
        wire.extend_from_slice(&r[1][..cut]);
        assert_eq!(
            read_wire(wire).await,
            (b"one".to_vec(), Some(io::ErrorKind::UnexpectedEof)),
            "cut at {cut}"
        );
    }
    // A length no record has.
    for len in [
        0u32,
        TAG_LEN as u32 - 1,
        (MAX_RECORD_PLAINTEXT + TAG_LEN + 1) as u32,
    ] {
        let mut wire: Vec<u8> = cat(&[&r[0]]);
        wire.extend_from_slice(&len.to_be_bytes());
        wire.extend_from_slice(&[0u8; 64]);
        assert_eq!(
            read_wire(wire).await,
            (b"one".to_vec(), Some(io::ErrorKind::InvalidData)),
            "length {len}"
        );
    }
    // Sealed with the other direction's key (a record reflected to its sender).
    let (a, mut b) = duplex(1024);
    let mut w = SealedStream::new(a, key(&K2), key(&K1));
    w.write_all(b"one").await.unwrap();
    w.flush().await.unwrap();
    drop(w);
    let mut wire = Vec::new();
    b.read_to_end(&mut wire).await.unwrap();
    assert_eq!(
        read_wire(wire).await,
        (Vec::new(), Some(io::ErrorKind::InvalidData))
    );
}

/// A write takes at most one record's plaintext, and seals it once.
#[tokio::test]
async fn a_write_seals_at_most_one_record() {
    let (a, mut b) = duplex(1 << 20);
    let mut w = SealedStream::new(a, key(&K1), key(&K2));
    let data = pattern(MAX_RECORD_PLAINTEXT * 2 + 5, 9);
    let n = w.write(&data).await.unwrap();
    assert_eq!(n, MAX_RECORD_PLAINTEXT);
    assert_eq!(w.write(&[]).await.unwrap(), 0);
    w.write_all(&data[n..]).await.unwrap();
    w.shutdown().await.unwrap();
    drop(w);
    let mut raw = Vec::new();
    b.read_to_end(&mut raw).await.unwrap();
    assert_eq!(raw.len(), data.len() + 3 * (4 + TAG_LEN), "three records");
    assert_eq!(read_wire(raw).await, (data, None));
}

#[test]
fn hub_keys_are_created_private_and_kept() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("box-key");
    let key = HubKey::load_or_create(&path).unwrap();
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    let again = HubKey::load_or_create(&path).unwrap();
    assert_eq!(
        key.public(),
        again.public(),
        "the same key on the next start"
    );
    assert_eq!(key.public_hex().len(), 64);
    assert_eq!(parse_hub_public(&key.public_hex()).unwrap(), key.public());
    assert!(parse_hub_public(&key.public_hex().to_uppercase()).is_err());
    assert!(parse_hub_public("abcd").is_err());
    let id = key.id();
    assert!(crate::valid_build_id(&id), "16 hex digits: {id}");
    assert_eq!(id, hub_id(&key.public()));
    assert_ne!(id, HubKey::generate().unwrap().id());
    let sig = key.sign(b"msg");
    ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, key.public())
        .verify(b"msg", &sig)
        .unwrap();
    // Only the key is left in the directory (no temporary file).
    let names: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names, ["box-key"]);

    // Hubs starting at once all end up with the one key.
    let path = dir.path().join("race-key");
    let publics: Vec<[u8; 32]> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..8)
            .map(|_| s.spawn(|| HubKey::load_or_create(&path).unwrap().public()))
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    assert!(publics.iter().all(|p| *p == publics[0]));

    // A missing directory is not created.
    assert!(HubKey::load_or_create(&dir.path().join("none").join("box-key")).is_err());
}

#[test]
fn hub_keys_others_can_reach_are_refused() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("box-key");
    HubKey::load_or_create(&path).unwrap();
    for mode in [0o640, 0o604, 0o660, 0o644] {
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        let e = format!("{:#}", HubKey::load_or_create(&path).unwrap_err());
        assert!(e.contains("chmod 600"), "{mode:o}: {e}");
        assert!(e.contains(&format!("mode {mode:03o}")), "{e}");
    }
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    HubKey::load_or_create(&path).unwrap();

    // A symbolic link, even to a good key.
    let link = dir.path().join("link-key");
    std::os::unix::fs::symlink(&path, &link).unwrap();
    let e = format!("{:#}", HubKey::load_or_create(&link).unwrap_err());
    assert!(e.contains("symbolic link"), "{e}");
    // A directory.
    let sub = dir.path().join("dir-key");
    std::fs::create_dir(&sub).unwrap();
    std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o700)).unwrap();
    let e = format!("{:#}", HubKey::load_or_create(&sub).unwrap_err());
    assert!(e.contains("not a regular file"), "{e}");
    // Not a key.
    let junk = dir.path().join("junk-key");
    std::fs::write(&junk, b"not a key").unwrap();
    std::fs::set_permissions(&junk, std::fs::Permissions::from_mode(0o600)).unwrap();
    let e = format!("{:#}", HubKey::load_or_create(&junk).unwrap_err());
    assert!(e.contains("no Ed25519 key"), "{e}");
}

#[test]
fn the_private_key_never_shows_in_debug() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("box-key");
    let key = HubKey::load_or_create(&path).unwrap();
    let pkcs8 = std::fs::read(&path).unwrap();
    // ring's Ed25519 PKCS#8 v2: a 16-byte header, then the 32-byte seed.
    let seed = &pkcs8[16..48];
    let dbg = format!("{key:?} {key:#?}");
    for shown in [
        hex::encode(seed),
        hex::encode_upper(seed),
        format!("{seed:?}"),
    ] {
        assert!(!dbg.contains(&shown), "{dbg}");
    }
    assert!(
        dbg.contains(&key.id()) && dbg.contains("<redacted>"),
        "{dbg}"
    );
}

#[test]
fn transcripts_separate_their_parts() {
    let a = transcript("gpu", "n1", "n2", "e1", "e2", "p");
    assert!(a.starts_with(b"twr box channel v1\0gpu\0n1\0n2\0e1\0e2\0p"));
    assert_eq!(
        a.len(),
        TRANSCRIPT_LABEL.len() + "gpu\0n1\0n2\0e1\0e2\0p".len()
    );
    assert_ne!(a, transcript("gp", "un1", "n2", "e1", "e2", "p"));
}

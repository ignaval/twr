//! End-to-end: certificate-free phone pairing (docs/design/phone-pairing.md) against a real
//! `twr hub` and `twr gateway` in pinned mode, on loopback, in a temporary `TWR_HOME`. A
//! rustls client stands in for a phone: it trusts only the gateway's pinned key, pairs by
//! typed code (proof first), and opens the device socket.

use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::SocketAddr;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const TWR: &str = env!("CARGO_BIN_EXE_twr");

fn command(home: &Path, args: &[&str]) -> Command {
    let mut c = Command::new(TWR);
    c.args(args)
        .env("TWR_HOME", home)
        .env_remove("TWR_WORKSPACE")
        .env_remove("RUST_LOG")
        .env("TWR_TEST_USER_TOKEN_FILE", test_token())
        .env("TWR_AGENT_PROBE", "off")
        .env("TWR_BOX_PROBE", "off")
        .env("TWR_GATEWAY_AUTOSTART", "off");
    c
}

/// A user token this test's hub and clients share (`TWR_TEST_USER_TOKEN_FILE`), as in the
/// other CLI tests: run from inside a twr thread, they would otherwise count as its agent.
fn test_token() -> &'static Path {
    static TOKEN: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    TOKEN.get_or_init(|| {
        use std::os::unix::fs::OpenOptionsExt;
        let dir = tempfile::tempdir().unwrap().keep();
        let path = dir.join("user.token");
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .unwrap()
            .write_all(format!("phone{:x}", std::process::id()).as_bytes())
            .unwrap();
        path
    })
}

fn twr(home: &Path, args: &[&str]) -> Output {
    command(home, args).output().expect("running twr")
}

fn poll<T>(what: &str, mut f: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

struct Killed(Child);
impl Drop for Killed {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Trusts only a leaf whose SPKI hashes to `expected`, when given; else any, recording the pin
/// it saw (typed pairing's first step). The handshake's signature is checked either way.
#[derive(Debug)]
struct Pinned {
    expected: Option<[u8; 32]>,
    seen: Mutex<Option<[u8; 32]>>,
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl rustls::client::danger::ServerCertVerifier for Pinned {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _name: &rustls::pki_types::ServerName<'_>,
        _ocsp: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        let cert = webpki::EndEntityCert::try_from(end_entity)
            .map_err(|_| rustls::Error::General("bad certificate".into()))?;
        let pin = twr_core::phone_pairing::pin_bytes(cert.subject_public_key_info().as_ref());
        *self.seen.lock().unwrap() = Some(pin);
        match self.expected {
            Some(expected) if expected != pin => Err(rustls::Error::General("PIN_MISMATCH".into())),
            _ => Ok(rustls::client::danger::ServerCertVerified::assertion()),
        }
    }
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// A TLS connection to `address` that trusts what `verifier` does: the connection and the
/// TLS version, or the handshake's error.
fn connect(
    address: SocketAddr,
    verifier: Arc<Pinned>,
) -> Result<rustls::StreamOwned<rustls::ClientConnection, std::net::TcpStream>, String> {
    let config = rustls::ClientConfig::builder_with_provider(verifier.provider.clone())
        .with_safe_default_protocol_versions()
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    let mut tcp = std::net::TcpStream::connect(address).unwrap();
    tcp.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    // A pinned client sends nothing but the handshake until the key is checked.
    let name = "twr-hub.invalid".try_into().unwrap();
    let mut tls = rustls::ClientConnection::new(Arc::new(config), name).unwrap();
    while tls.is_handshaking() {
        tls.complete_io(&mut tcp).map_err(|e| e.to_string())?;
    }
    assert_eq!(
        tls.protocol_version(),
        Some(rustls::ProtocolVersion::TLSv1_3)
    );
    Ok(rustls::StreamOwned::new(tls, tcp))
}

/// One HTTP/1.1 request on its own connection: the status and the JSON body.
fn request(
    address: SocketAddr,
    verifier: Arc<Pinned>,
    method: &str,
    path: &str,
    body: Option<&Value>,
) -> (u16, Value) {
    let mut stream = connect(address, verifier).unwrap();
    let body = body.map(Value::to_string).unwrap_or_default();
    let head = format!(
        "{method} {path} HTTP/1.1\r\nHost: twr-hub.invalid\r\nConnection: close\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).unwrap();
    stream.write_all(body.as_bytes()).unwrap();
    let mut response = Vec::new();
    let _ = stream.read_to_end(&mut response);
    let response = String::from_utf8_lossy(&response).into_owned();
    let status = response
        .split(' ')
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let body = response.split("\r\n\r\n").nth(1).unwrap_or("");
    (status, serde_json::from_str(body).unwrap_or(Value::Null))
}

/// The first text frame the gateway sends on an upgraded device socket.
fn socket_hello(address: SocketAddr, verifier: Arc<Pinned>, token: &str) -> Value {
    let mut stream = connect(address, verifier).unwrap();
    let head = format!(
        "GET /v1/socket HTTP/1.1\r\nHost: twr-hub.invalid\r\nConnection: Upgrade\r\n\
         Upgrade: websocket\r\nSec-WebSocket-Version: 13\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nAuthorization: Bearer {token}\r\n\r\n"
    );
    stream.write_all(head.as_bytes()).unwrap();
    let mut seen = Vec::new();
    let mut byte = [0u8; 1];
    while !seen.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).unwrap();
        seen.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&seen);
    assert!(head.starts_with("HTTP/1.1 101"), "{head}");
    let mut frame = [0u8; 2];
    stream.read_exact(&mut frame).unwrap();
    assert_eq!(frame[0], 0x81, "one unfragmented text frame");
    let len = match frame[1] & 0x7f {
        126 => {
            let mut n = [0u8; 2];
            stream.read_exact(&mut n).unwrap();
            u16::from_be_bytes(n) as usize
        }
        127 => {
            let mut n = [0u8; 8];
            stream.read_exact(&mut n).unwrap();
            u64::from_be_bytes(n) as usize
        }
        n => n as usize,
    };
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload).unwrap();
    serde_json::from_slice(&payload).unwrap()
}

#[test]
fn a_phone_pairs_by_typed_code_with_a_pinned_gateway() {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    let log = std::fs::File::create(home.join("hub.log")).unwrap();
    let _hub = Killed(
        command(home, &["hub"])
            .stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap(),
    );
    poll("the hub", || {
        twr(home, &["thread", "list"])
            .status
            .success()
            .then_some(())
    });
    // Pinned mode: no --tls-cert. On loopback, at a port the system picks.
    let log = std::fs::File::create(home.join("gateway.log")).unwrap();
    let mut gateway = Killed(
        command(home, &["gateway", "--listen", "127.0.0.1:0"])
            .stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap(),
    );
    let info: Value = poll("the gateway", || {
        let out = twr(home, &["gateway", "--gateway-info"]);
        let info: Value = serde_json::from_slice(&out.stdout).ok()?;
        (info["running"] == true).then_some(info)
    });
    assert_eq!(info["mode"], "pinned");
    let address: SocketAddr = info["listen"].as_str().unwrap().parse().unwrap();
    assert_eq!(info["listens"], json!([address.to_string()]));
    let settings = home.join("mobile").join("gateway-settings.json");
    assert!(
        !settings.exists(),
        "a gateway started without `twr phone` turns no autostart on"
    );
    let pin = twr_core::phone_pairing::decode_pin(info["pin"].as_str().unwrap()).unwrap();
    let route = format!("tls:{address}");
    assert_eq!(info["routes"], json!([route]));
    for name in ["identity-key.pem", "identity-cert.pem", "iroh-key"] {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(home.join("mobile").join(name))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "{name}");
    }

    // The invitation as the desktop asks for one.
    let out = twr(home, &["gateway", "--pair", "--pair-only", "--json"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let line: Value = serde_json::from_slice(&out.stdout).unwrap();
    let code = line["code"].as_str().unwrap().to_owned();
    assert_eq!(code.len(), 12);
    let invitation =
        twr_core::phone_pairing::Invitation::parse(line["link"].as_str().unwrap()).unwrap();
    assert_eq!(
        serde_json::to_value(&invitation).unwrap(),
        line["invitation"]
    );
    assert_eq!(invitation.pin, info["pin"].as_str().unwrap());
    assert_eq!(invitation.routes, std::slice::from_ref(&route));
    assert_eq!(invitation.id, info["gateway_id"].as_str().unwrap());
    assert_eq!(
        invitation.expires_at,
        line["expires_at"].as_u64().unwrap() * 1000
    );

    // Another key: refused at the handshake, twice. That costs no pairing attempt.
    for _ in 0..2 {
        let other = Arc::new(Pinned {
            expected: Some([7u8; 32]),
            seen: Mutex::new(None),
            provider: provider.clone(),
        });
        let refused = connect(address, other.clone()).err().unwrap();
        assert!(refused.contains("PIN_MISMATCH"), "{refused}");
        assert_eq!(*other.seen.lock().unwrap(), Some(pin));
    }

    // Typed pairing: capture the key unchecked, ask for a proof, check it, then pair.
    let capture = Arc::new(Pinned {
        expected: None,
        seen: Mutex::new(None),
        provider: provider.clone(),
    });
    let (status, health) = request(address, capture.clone(), "GET", "/health", None);
    assert_eq!(status, 200);
    let captured = capture.seen.lock().unwrap().unwrap();
    assert_eq!(captured, pin);
    assert_eq!(health["protocol"], 2);
    assert_eq!(health["pin"], info["pin"]);
    let gateway_id = health["gateway_id"].as_str().unwrap().to_owned();
    let nonce: [u8; 32] = std::array::from_fn(|i| i as u8 * 3);
    let query = format!(
        "/v1/pair/proof?nonce={}",
        twr_core::phone_pairing::b64url_encode(&nonce)
    );
    let pinned = Arc::new(Pinned {
        expected: Some(captured),
        seen: Mutex::new(None),
        provider: provider.clone(),
    });
    let (status, body) = request(address, pinned.clone(), "GET", &query, None);
    assert_eq!(status, 200, "{body}");
    let proof = twr_core::phone_pairing::b64url_decode(body["proof"].as_str().unwrap()).unwrap();
    let typed = twr_core::phone_pairing::display_code(&code).to_ascii_lowercase();
    let key = twr_core::phone_pairing::proof_key(&typed).unwrap();
    assert!(twr_core::phone_pairing::verify_pin_proof(
        &key,
        &captured,
        &nonce,
        &gateway_id,
        &proof
    ));
    let (status, _) = request(
        address,
        pinned.clone(),
        "GET",
        "/v1/pair/proof?nonce=AAAA",
        None,
    );
    assert_eq!(status, 400);
    let pairing = json!({"code": typed, "name": "Test phone"});
    let (status, paired) = request(address, pinned.clone(), "POST", "/v1/pair", Some(&pairing));
    assert_eq!(status, 200, "{paired}");
    assert_eq!(paired["gateway_id"], gateway_id.as_str());
    assert_eq!(paired["scopes"], json!(["read", "operate"]));
    let token = paired["token"].as_str().unwrap().to_owned();
    // The invitation is gone: no more proofs.
    let (status, _) = request(address, pinned.clone(), "GET", &query, None);
    assert_eq!(status, 404);

    // The device socket, on a pinned connection: its hello names the routes.
    let hello = socket_hello(address, pinned, &token);
    assert_eq!(hello["type"], "authenticated", "{hello}");
    assert_eq!(hello["device"], paired["id"]);
    assert_eq!(hello["routes"], json!([route]));

    let devices = twr(home, &["phone", "devices", "--json"]);
    let devices: Value = serde_json::from_slice(&devices.stdout).unwrap();
    assert_eq!(devices[0]["name"], "Test phone");

    // `twr phone` through the gateway that runs: it turns autostart on and saves where that
    // gateway listens. Told to stop, it cancels its invitation.
    let mut phone = Killed(
        command(home, &["phone", "--json"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let mut lines = std::io::BufReader::new(phone.0.stdout.take().unwrap());
    let mut read_line = || {
        let mut line = String::new();
        std::io::BufRead::read_line(&mut lines, &mut line).unwrap();
        serde_json::from_str::<Value>(&line).unwrap_or(Value::Null)
    };
    let opened = read_line();
    assert_eq!(opened["code"].as_str().map(str::len), Some(12), "{opened}");
    let saved: Value = serde_json::from_slice(&std::fs::read(&settings).unwrap()).unwrap();
    assert_eq!(saved["autostart"], true);
    assert_eq!(saved["listen"], json!([address.to_string()]));
    // SAFETY: a signal to our own, unreaped child.
    unsafe { libc::kill(phone.0.id() as i32, libc::SIGTERM) };
    assert_eq!(read_line(), json!({"cancelled": true}));

    // The desktop's "Stop gateway": it stays stopped, a hub that starts won't start it.
    let instance = info["instance"].as_str().unwrap();
    let out = twr(
        home,
        &["gateway", "--stop-gateway", "--expected-instance", instance],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    poll("the gateway to stop", || {
        gateway.0.try_wait().ok().flatten().map(|_| ())
    });
    let saved: Value = serde_json::from_slice(&std::fs::read(&settings).unwrap()).unwrap();
    assert_eq!(saved["autostart"], false);
    assert_eq!(saved["listen"], json!([address.to_string()]));
    let _ = twr(home, &["hub", "stop"]);
}

/// Asking about the gateway, as the desktop app does with no certificate named, writes no
/// settings: only `twr phone` or a pinned gateway that serves turns autostart on. `twr phone
/// off` keeps it off. Neither needs a hub.
#[test]
fn administration_alone_never_turns_autostart_on() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    let out = twr(home, &["gateway", "--gateway-info"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let info: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(info["running"], false);
    assert!(info.get("pin").is_none(), "no identity yet");
    let settings = home.join("mobile").join("gateway-settings.json");
    assert!(!settings.exists());
    let out = twr(home, &["phone", "off"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let saved: Value = serde_json::from_slice(&std::fs::read(&settings).unwrap()).unwrap();
    assert_eq!(saved["autostart"], false);
    let out = twr(home, &["phone"]);
    assert!(!out.status.success(), "no terminal and no --json");
    assert!(String::from_utf8_lossy(&out.stderr).contains("terminal"));
}

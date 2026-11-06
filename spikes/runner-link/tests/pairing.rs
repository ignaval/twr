use std::{
    sync::OnceLock,
    time::{Duration, Instant},
};

use twr_runner_link_spike::pairing::{Code, Confirmed, Context, Derived, Exchange, Role, Window};

fn derived() -> &'static Derived {
    static KEY: OnceLock<Derived> = OnceLock::new();
    KEY.get_or_init(|| Code::parse("4HK7-M2QP-9XTR").unwrap().derive().unwrap())
}

fn context() -> Context {
    Context {
        hub: [1; 32],
        runner: [2; 32],
        initiator: Role::Hub,
        session: [4; 16],
        expires_unix: 12345,
        identity_payload_hash: [5; 32],
    }
}

fn confirmed() -> Confirmed {
    let context = context();
    let (hub, h) = Exchange::start(Role::Hub, derived(), &context);
    let (runner, r) = Exchange::start(Role::Runner, derived(), &context);
    let hub = hub.finish(&r).unwrap();
    let runner = runner.finish(&h).unwrap();
    hub.confirm(&runner.confirmation()).unwrap()
}

#[test]
fn same_code_confirms_both_directions_but_not_its_own_proof() {
    let context = context();
    let (hub, h) = Exchange::start(Role::Hub, derived(), &context);
    let (runner, r) = Exchange::start(Role::Runner, derived(), &context);
    let hub = hub.finish(&r).unwrap();
    let runner = runner.finish(&h).unwrap();
    let h = hub.confirmation();
    let r = runner.confirmation();
    assert_ne!(h, r);
    hub.confirm(&r).unwrap();
    runner.confirm(&h).unwrap();
}

#[test]
fn wrong_code_and_modified_identity_context_cannot_confirm() {
    let wrong = Code::parse("7KQ4-M9TX-3PWD").unwrap().derive().unwrap();
    for modified in [false, true] {
        let mut peer_context = context();
        if modified {
            peer_context.identity_payload_hash[0] ^= 1;
        }
        let (hub, h) = Exchange::start(Role::Hub, derived(), &context());
        let (runner, r) = Exchange::start(
            Role::Runner,
            if modified { derived() } else { &wrong },
            &peer_context,
        );
        let hub = hub.finish(&r).unwrap();
        let runner = runner.finish(&h).unwrap();
        assert!(hub.confirm(&runner.confirmation()).is_err());
    }
}

#[test]
fn role_reflection_replay_and_oversized_messages_fail() {
    let (hub, h) = Exchange::start(Role::Hub, derived(), &context());
    assert!(hub.finish(&h).is_err());
    let (hub, _) = Exchange::start(Role::Hub, derived(), &context());
    assert!(hub.finish(&[0; 8192]).is_err());
    let (old, h) = Exchange::start(Role::Hub, derived(), &context());
    let (peer, r) = Exchange::start(Role::Runner, derived(), &context());
    let old = old.finish(&r).unwrap();
    let peer = peer.finish(&h).unwrap();
    let proof = peer.confirmation();
    old.confirm(&proof).unwrap();
    let (new, _) = Exchange::start(Role::Hub, derived(), &context());
    let (_, r) = Exchange::start(Role::Runner, derived(), &context());
    assert!(new.finish(&r).unwrap().confirm(&proof).is_err());
    // Replaying an old PAKE message into a fresh exchange also fails confirmation.
    let (new, _) = Exchange::start(Role::Hub, derived(), &context());
    let (peer, r) = Exchange::start(Role::Runner, derived(), &context());
    let new = new.finish(&r).unwrap();
    let replayed = peer.finish(&h).unwrap();
    assert!(new.confirm(&replayed.confirmation()).is_err());
}

#[test]
fn initiation_direction_and_confirmation_direction_are_bound() {
    let mut other = context();
    other.initiator = Role::Runner;
    let (hub, h) = Exchange::start(Role::Hub, derived(), &context());
    let (runner, r) = Exchange::start(Role::Runner, derived(), &other);
    assert!(hub
        .finish(&r)
        .unwrap()
        .confirm(&runner.finish(&h).unwrap().confirmation())
        .is_err());
    let (hub, _) = Exchange::start(Role::Hub, derived(), &context());
    let (_, r) = Exchange::start(Role::Runner, derived(), &context());
    let hub = hub.finish(&r).unwrap();
    let reflected = hub.confirmation();
    assert!(hub.confirm(&reflected).is_err());
    // Both initiation directions work when both peers agree.
    let (hub, h2) = Exchange::start(Role::Hub, derived(), &other);
    let (runner, r2) = Exchange::start(Role::Runner, derived(), &other);
    hub.finish(&r2)
        .unwrap()
        .confirm(&runner.finish(&h2).unwrap().confirmation())
        .unwrap();
}

#[test]
fn failed_admission_does_not_burn_attempts_and_expiry_needs_no_tombstone() {
    let now = Instant::now();
    let mut window = Window::new(
        derived(),
        &context(),
        [7; 16],
        now + Duration::from_secs(900),
    );
    for _ in 0..100 {
        assert!(window.failed(&[8; 16], now).is_err());
    }
    for _ in 0..3 {
        window.failed(&[7; 16], now).unwrap();
    }
    assert!(window.admit(&[7; 16], now).is_err());
    let window = Window::new(derived(), &context(), [7; 16], now);
    assert!(window.admit(&[7; 16], now).is_err());
}

#[test]
fn concurrent_confirmations_have_only_one_winner() {
    let now = Instant::now();
    let window = std::sync::Arc::new(std::sync::Mutex::new(Window::new(
        derived(),
        &context(),
        [7; 16],
        now + Duration::from_secs(900),
    )));
    let proofs = [confirmed(), confirmed()];
    let results: Vec<_> = proofs
        .into_iter()
        .map(|proof| {
            let window = window.clone();
            std::thread::spawn(move || window.lock().unwrap().consume(&[7; 16], now, proof).is_ok())
        })
        .collect();
    assert_eq!(
        results
            .into_iter()
            .map(|t| t.join().unwrap())
            .filter(|won| *won)
            .count(),
        1
    );
}

#[test]
fn proof_from_a_different_session_cannot_consume_a_code() {
    let now = Instant::now();
    let mut other = context();
    other.session[0] ^= 1;
    let mut window = Window::new(derived(), &other, [7; 16], now + Duration::from_secs(900));
    assert!(window.consume(&[7; 16], now, confirmed()).is_err());
    assert!(window.admit(&[7; 16], now).is_ok());
}

#[test]
fn code_normalization_and_redaction() {
    assert!(Code::parse("4hk7-m2qp-9xtr").is_ok());
    assert!(Code::parse("4HK7_M2QP_9XTR").is_err());
    assert!(Code::parse(&"X".repeat(10000)).is_err());
    assert_eq!(format!("{:?}", Code::generate()), "Code(<redacted>)");
}

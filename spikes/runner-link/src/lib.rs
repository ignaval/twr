//! Non-shipping experiments for the linked-runner design.
//!
//! No automatic startup, public discovery, service installation or agent execution.
//! PAKE implementation acceptance and real-machine validation remain release gates.

pub mod locator;
pub mod pairing;
pub mod transfer;
pub mod transport;

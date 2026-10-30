//! [`twr_runner::Shutdown`] sees a SIGTERM that came while the thread was busy with something
//! synchronous (opening the box), before the runtime delivered it to its stream: the signal
//! handler itself latched it.
//!
//! One test in its own binary (its own process): it sends SIGTERM to the process, which every
//! signal listener in that process would see.

use futures::FutureExt;

#[test]
fn a_sigterm_during_synchronous_startup_is_seen_before_the_runtime_delivers_it() {
    // A current-thread runtime: its signal driver runs only when the future below yields,
    // which it never does, so Tokio's stream cannot have the signal.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let mut shutdown = twr_runner::Shutdown::install();
        assert!(!shutdown.fired(), "no signal yet");
        // SAFETY: raise(3) sends SIGTERM to this thread, which runs the handlers installed
        // above (Tokio's and the latch) before it returns: the process is not terminated.
        assert_eq!(unsafe { libc::raise(libc::SIGTERM) }, 0);
        // Still in the same poll, as during a synchronous opening.
        assert!(
            shutdown.fired(),
            "a SIGTERM the runtime has not delivered yet went unseen"
        );
        assert!(
            shutdown.wait().now_or_never().is_some(),
            "waiting for a signal that already came did not end at once"
        );
    });
}

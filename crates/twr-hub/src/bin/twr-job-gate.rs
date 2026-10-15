//! A job's start gate (`twr_hub::job_gate`) as a program of its own, for this crate's tests:
//! their hubs run it in place of `twr __job-gate` (their own executable is the test harness).
//! The hub that ships is `twr`'s, which runs `twr __job-gate`.

fn main() {
    twr_hub::job_gate_main(std::env::args_os().skip(1).collect())
}

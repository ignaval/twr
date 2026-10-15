//! The runner token read from the environment leaves it: nothing the runner starts inherits it.
//! A test binary of its own (one test), since it changes the process environment.

use twr_runner::client::{take_token_env, TOKEN_ENV};

#[test]
fn the_token_is_taken_out_of_the_environment() {
    std::env::set_var(TOKEN_ENV, "tok-123");
    assert_eq!(take_token_env().unwrap().expose(), "tok-123");
    assert!(std::env::var_os(TOKEN_ENV).is_none());
    // A child started afterwards does not see it.
    let out = std::process::Command::new("sh")
        .args(["-c", &format!("printf %s \"${{{TOKEN_ENV}-unset}}\"")])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "unset");
    // Empty counts as unset, and is removed too.
    std::env::set_var(TOKEN_ENV, "");
    assert!(take_token_env().is_none());
    assert!(std::env::var_os(TOKEN_ENV).is_none());
}

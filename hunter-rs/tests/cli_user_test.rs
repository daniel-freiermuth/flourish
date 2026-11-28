#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! `hunter user add|passwd|disable`, run as the operator runs them: the
//! real binary against a scratch root, the password piped on stdin or
//! typed (twice) on a terminal -- a pseudo-terminal the test holds the
//! other end of.
//!
//! Nothing else ran these commands, so mutation testing could not tell
//! `run_user` from `Ok(())` -- every command exiting 0 having done
//! nothing -- nor `read_password` or `prompt_hidden` from one that
//! ignores what was typed.

mod support;

use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use support::TempDir;

/// Run `hunter --root <root> user <args>` with `stdin` piped in.
fn hunter_user(root: &Path, args: &[&str], stdin: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_hunter"))
        .arg("--root")
        .arg(root)
        .arg("user")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

/// Run `hunter --root <root> user <args>` with a terminal for stdin, and
/// `typed` typed on it.
///
/// Typed up front, before the command switches echo off: the terminal
/// queues it either way (`prompt_hidden` changes the mode with
/// `TCSANOW`, which keeps queued input), and the echo only goes back to
/// the end this test holds and never reads.
fn hunter_user_on_terminal(root: &Path, args: &[&str], typed: &str) -> Output {
    let pty = nix::pty::openpty(None, None).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_hunter"))
        .arg("--root")
        .arg(root)
        .arg("user")
        .args(args)
        .stdin(Stdio::from(pty.slave))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Held open until the command exits: closing it hangs the terminal up.
    let mut keyboard = std::fs::File::from(pty.master);
    keyboard.write_all(typed.as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    drop(keyboard);
    out
}

fn said(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// The account each command leaves behind: its password hash, or `None`
/// for one that may not log in.
async fn login_hash(db: &Path, name: &str) -> Option<String> {
    let store = hunter::store::Store::connect(db).await.unwrap();
    store
        .login_candidate(name)
        .await
        .unwrap()
        .map(|(_, hash)| hash)
}

/// `add` creates the account with the piped password, `passwd` replaces
/// it, `disable` stops it logging in -- each exiting 0 only having done so.
#[tokio::test]
async fn user_commands_add_change_and_disable_the_account_they_name() {
    let root = TempDir::new("cli-user");
    let cfg = support::load_config(root.path());
    std::fs::create_dir_all(cfg.db_path.parent().unwrap()).unwrap();

    let add = hunter_user(root.path(), &["add", "alice"], "first-password-1\n");
    assert!(add.status.success(), "add failed: {}", said(&add));
    let hash = login_hash(&cfg.db_path, "alice").await.expect("no alice");
    assert!(hunter::auth::verify_password("first-password-1", &hash));

    let passwd = hunter_user(root.path(), &["passwd", "alice"], "second-password-2\n");
    assert!(passwd.status.success(), "passwd failed: {}", said(&passwd));
    let hash = login_hash(&cfg.db_path, "alice").await.expect("no alice");
    assert!(hunter::auth::verify_password("second-password-2", &hash));
    assert!(!hunter::auth::verify_password("first-password-1", &hash));

    let disable = hunter_user(root.path(), &["disable", "alice"], "");
    assert!(
        disable.status.success(),
        "disable failed: {}",
        said(&disable)
    );
    assert_eq!(login_hash(&cfg.db_path, "alice").await, None);
}

/// A command that cannot do what it was asked exits non-zero: a password
/// too short to accept, and an account that does not exist.
#[tokio::test]
async fn user_commands_that_fail_exit_non_zero() {
    let root = TempDir::new("cli-user-fail");
    let cfg = support::load_config(root.path());
    std::fs::create_dir_all(cfg.db_path.parent().unwrap()).unwrap();

    let short = hunter_user(root.path(), &["add", "bob"], "short\n");
    assert!(!short.status.success(), "{}", said(&short));
    assert_eq!(login_hash(&cfg.db_path, "bob").await, None);

    let missing = hunter_user(root.path(), &["disable", "carol"], "");
    assert!(!missing.status.success(), "{}", said(&missing));
}

/// On a terminal the password is typed twice: the account gets what was
/// typed when both agree, and nothing when they differ.
#[tokio::test]
async fn a_password_typed_on_a_terminal_is_set_only_when_typed_the_same_twice() {
    let root = TempDir::new("cli-user-tty");
    let cfg = support::load_config(root.path());
    std::fs::create_dir_all(cfg.db_path.parent().unwrap()).unwrap();

    let differ = hunter_user_on_terminal(
        root.path(),
        &["add", "dave"],
        "typed-password-1\ntyped-passw0rd-1\n",
    );
    assert!(!differ.status.success(), "{}", said(&differ));
    assert!(
        said(&differ).contains("the passwords differ"),
        "{}",
        said(&differ)
    );
    assert_eq!(login_hash(&cfg.db_path, "dave").await, None);

    let add = hunter_user_on_terminal(
        root.path(),
        &["add", "dave"],
        "typed-password-1\ntyped-password-1\n",
    );
    assert!(add.status.success(), "add failed: {}", said(&add));
    let hash = login_hash(&cfg.db_path, "dave").await.expect("no dave");
    assert!(hunter::auth::verify_password("typed-password-1", &hash));
}

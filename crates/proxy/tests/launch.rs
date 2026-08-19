//! `docs/api.md` §2.3 — starting a client with this proxy's configuration.
//!
//! The environment half is set on the child. The policy half has no environment
//! variable at all, so it rides on the client's own settings flag — and that
//! flag is the one place this collides with what the caller typed.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::indexing_slicing)]

use codex_cc_proxy::launch;
use pretty_assertions::assert_eq;

const POLICY: &str = r#"{"permissions":{"deny":["Skill(claude-api)"]}}"#;

fn command(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|part| (*part).to_owned()).collect()
}

/// The policy leads, so the caller's own flags read in the order they typed
/// them. Position is otherwise free: settings layers union, measured — a rule
/// in a project file and a rule on the command line were both enforced in one
/// session, and a control skill denied by neither still launched.
#[test]
fn the_policy_leads_the_forwarded_arguments() {
    let plan = launch::plan(&command(&["claude", "--resume", "abc"]), Some(POLICY)).unwrap();

    assert_eq!(plan.program, "claude");
    assert_eq!(
        plan.arguments,
        command(&["--settings", POLICY, "--resume", "abc"])
    );
}

/// Refused rather than merged or overridden.
///
/// Measured: two `--settings` on one argument list and the client keeps the
/// last, drops the first, exits 0, and writes nothing to stderr. Leading with
/// this proxy's document loses the policy; trailing loses the caller's. Both
/// are silent, and a permission rule that disappears without a word is the
/// failure this proxy exists to avoid.
#[test]
fn a_settings_flag_from_the_caller_is_refused_rather_than_dropped() {
    for given in [
        command(&["claude", "--settings", "mine.json"]),
        command(&["claude", "--settings=mine.json"]),
        command(&[
            "/usr/local/bin/claude",
            "-p",
            "hi",
            "--settings",
            "mine.json",
        ]),
    ] {
        let error = launch::plan(&given, Some(POLICY))
            .expect_err("a collision that would drop one of the two must be visible");
        assert!(
            error.message.contains("--settings"),
            "the message has to name the collision: {}",
            error.message
        );
        assert!(
            error.message.contains("codex-cc-proxy settings"),
            "and the way out of it: {}",
            error.message
        );
    }
}

/// Everything after the program is opaque and forwarded in order. A launcher
/// that reorders or swallows a flag makes the thing it wraps undebuggable.
#[test]
fn arguments_reach_the_child_untouched_and_in_order() {
    let given = command(&["claude", "-p", "--verbose", "--", "trailing", "-x"]);
    let plan = launch::plan(&given, None).unwrap();

    assert_eq!(
        plan.arguments,
        command(&["-p", "--verbose", "--", "trailing", "-x"])
    );
}

/// A program that does not read the flag gets the environment and nothing
/// spliced. Its own `--settings` is its own business, so it is not a collision
/// and must not be refused.
#[test]
fn a_program_that_does_not_take_the_flag_gets_nothing_spliced() {
    let plan = launch::plan(
        &command(&["some-tool", "--settings", "theirs"]),
        Some(POLICY),
    )
    .expect("another program's flag is not this proxy's collision");

    assert_eq!(plan.program, "some-tool");
    assert_eq!(plan.arguments, command(&["--settings", "theirs"]));
}

/// No policy, no flag. With `[client]` switched off there is nothing to
/// deliver, and passing an empty document would be inventing one.
#[test]
fn no_policy_means_no_flag() {
    let plan = launch::plan(&command(&["claude", "-p", "hi"]), None).unwrap();

    assert_eq!(plan.arguments, command(&["-p", "hi"]));
}

#[test]
fn an_empty_command_is_refused() {
    let error = launch::plan(&[], Some(POLICY)).expect_err("there is nothing to start");
    assert!(error.message.contains("exec"), "{}", error.message);
}

/// It refuses before starting anything.
///
/// A client launched against a daemon that is not there fails with a connection
/// refused it cannot explain. Naming the daemon, and how to start it, is the
/// whole difference. `TMPDIR` moves the socket path, so this never reaches a
/// daemon the developer happens to be running.
#[test]
fn exec_refuses_before_starting_anything_when_the_daemon_is_not_answering() {
    let dir = tempfile::tempdir().unwrap();

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_codex-cc-proxy"))
        .args(["exec", "claude", "--resume", "x"])
        .env("TMPDIR", dir.path())
        .output()
        .expect("the binary should run");

    assert!(
        !output.status.success(),
        "a launch that cannot be configured is not a launch"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("codex-cc-proxy run"),
        "the refusal has to say how to fix it: {stderr}"
    );
}

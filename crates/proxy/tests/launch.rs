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

/// The assembly, not the parts.
///
/// Features in this project have shipped inert while every unit test passed,
/// each time because nothing exercised the wiring between a value handed in at
/// startup and the thing that was supposed to read it. So this starts the
/// shipping binary as a daemon and drives a launch all the way through it: a
/// policy that never reaches the child fails here rather than in someone's
/// session.
///
/// **It contacts nothing.** Without credentials the daemon short-circuits to
/// the fallback model list before any request is built, which the log states in
/// as many words. `TMPDIR` moves the control socket and `CODEX_CC_PROXY_HOME`
/// moves the configuration, so neither this developer's daemon nor their
/// configuration is touched.
#[cfg(unix)]
#[test]
fn a_launched_child_is_given_the_policy_and_the_environment() {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let bin = dir.path().join("bin");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&bin).unwrap();

    // A stand-in that reports what it was actually given, which is the only
    // evidence that distinguishes a delivered policy from a plausible one.
    let stub = bin.join("claude");
    let mut file = std::fs::File::create(&stub).unwrap();
    file.write_all(b"#!/bin/sh\necho \"ARGV: $*\"\necho \"BASE: $ANTHROPIC_BASE_URL\"\n")
        .unwrap();
    drop(file);
    std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();

    let binary = env!("CARGO_BIN_EXE_codex-cc-proxy");
    let mut daemon = std::process::Command::new(binary)
        .args(["run", "--port", "0"])
        .env("CODEX_CC_PROXY_HOME", &home)
        .env("TMPDIR", dir.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("the daemon should start");

    let socket = dir.path().join("codex-cc-proxy.sock");
    for _ in 0..200 {
        if socket.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    let launched = std::process::Command::new(binary)
        .args(["exec", "claude", "--resume", "abc"])
        .env("CODEX_CC_PROXY_HOME", &home)
        .env("TMPDIR", dir.path())
        .env(
            "PATH",
            format!(
                "{}:{}",
                bin.display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        )
        .output()
        .expect("the launcher should run");

    let _ = daemon.kill();
    let _ = daemon.wait();

    let stdout = String::from_utf8_lossy(&launched.stdout);
    let stderr = String::from_utf8_lossy(&launched.stderr);
    assert!(
        stdout.contains("Skill(claude-api)"),
        "the policy has to reach the child, not merely exist: {stdout}{stderr}"
    );
    assert!(
        stdout.contains("--resume abc"),
        "and the caller's own arguments with it: {stdout}{stderr}"
    );
    assert!(
        stdout.contains("BASE: http://127.0.0.1:"),
        "and the environment half: {stdout}{stderr}"
    );
}

/// The verbs that would lie must stop, and the one that would not must not.
///
/// One binary is both the daemon and the CLI, and replacing the file on disk
/// does not restart what is already running — so a newer CLI against an older
/// daemon is what an upgrade leaves behind. Against such a daemon `settings`
/// would print a document that looks complete and lacks a permission rule, and
/// `exec` would start a session with that rule missing and nothing saying so.
/// Both refuse. `env` continues, because routing is all it ever carried.
///
/// The stand-in answers the way this daemon did before client policy existed:
/// a payload with `variables` and no `settings` at all.
#[cfg(unix)]
#[test]
fn settings_and_exec_refuse_a_daemon_that_predates_client_policy() {
    use std::io::BufRead;
    use std::io::BufReader;
    use std::io::Write;

    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("codex-cc-proxy.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();

    let server = std::thread::spawn(move || {
        // Three callers: `settings`, `exec`, and `env`.
        for stream in listener.incoming().take(3) {
            let Ok(mut stream) = stream else { continue };
            let mut request = String::new();
            let _ = BufReader::new(stream.try_clone().unwrap()).read_line(&mut request);
            let _ = writeln!(
                stream,
                r#"{{"jsonrpc":"2.0","id":1,"result":{{"variables":[["ANTHROPIC_BASE_URL","http://127.0.0.1:8787"]]}}}}"#
            );
            let _ = stream.flush();
        }
    });

    let binary = env!("CARGO_BIN_EXE_codex-cc-proxy");
    let run = |args: &[&str]| {
        std::process::Command::new(binary)
            .args(args)
            .env("TMPDIR", dir.path())
            .output()
            .expect("the binary should run")
    };

    for verb in [vec!["settings"], vec!["exec", "claude"]] {
        let output = run(&verb);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !output.status.success(),
            "`{}` produced something rather than refusing: {}",
            verb.join(" "),
            String::from_utf8_lossy(&output.stdout)
        );
        assert!(
            stderr.to_lowercase().contains("restart the daemon"),
            "`{}` has to say what to do: {stderr}",
            verb.join(" ")
        );
    }

    let output = run(&["env"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "routing is unaffected and this path stays open"
    );
    assert!(
        stdout.contains("export ANTHROPIC_BASE_URL=http://127.0.0.1:8787"),
        "{stdout}"
    );
    assert!(
        stdout.to_lowercase().contains("restart the daemon"),
        "and it names why the policy is not there: {stdout}"
    );

    let _ = server.join();
}

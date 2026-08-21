//! `docs/api.md` §2.3 — starting a client with this proxy's configuration.
//!
//! The environment half is set on the child. The policy half has no environment
//! variable at all, so it rides on the client's own settings flag — and that
//! flag is the one place this collides with what the caller typed.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::indexing_slicing)]

use pretty_assertions::assert_eq;
use proxenos::launch;

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
            error.message.contains("proxenos settings"),
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

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_proxenos"))
        .args(["exec", "claude", "--resume", "x"])
        .env("PROXENOS_HOME", dir.path())
        .env("TMPDIR", dir.path())
        .output()
        .expect("the binary should run");

    assert!(
        !output.status.success(),
        "a launch that cannot be configured is not a launch"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("proxenos run"),
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
/// as many words. `TMPDIR` moves the control socket and `PROXENOS_HOME`
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

    let binary = env!("CARGO_BIN_EXE_proxenos");
    let mut daemon = std::process::Command::new(binary)
        .args(["run", "--port", "0"])
        .env("PROXENOS_HOME", &home)
        .env("TMPDIR", dir.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("the daemon should start");

    let socket = dir.path().join("proxenos.sock");
    for _ in 0..200 {
        if socket.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    let launched = std::process::Command::new(binary)
        .args(["exec", "claude", "--resume", "abc"])
        .env("PROXENOS_HOME", &home)
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
    let socket = dir.path().join("proxenos.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();

    let server = std::thread::spawn(move || {
        // Four callers: `settings`, `exec`, `env --json`, and `env`.
        for stream in listener.incoming().take(4) {
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

    let binary = env!("CARGO_BIN_EXE_proxenos");
    let run = |args: &[&str]| {
        std::process::Command::new(binary)
            .args(args)
            .env("PROXENOS_HOME", dir.path())
            .env("TMPDIR", dir.path())
            .output()
            .expect("the binary should run")
    };

    // `env --json` is the same document under the older name, so it has to
    // refuse for the same reason. Leaving it out is how one of two names for
    // one thing quietly keeps the behaviour the other one dropped.
    for verb in [
        vec!["settings"],
        vec!["exec", "claude"],
        vec!["env", "--json"],
    ] {
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

/// A stop over the socket actually stops the process.
///
/// The unit tests establish that the signal is armed after the answer is
/// written; nothing there proves the run loop is listening to it. This starts
/// the shipping binary as a daemon, asks it to stop through the shipping CLI,
/// and waits for the process to be gone. The wiring is the whole assertion.
///
/// It contacts nothing: without credentials the daemon short-circuits to the
/// fallback model list before any request is built. `TMPDIR` moves the control
/// socket and `PROXENOS_HOME` the configuration, so a developer's own
/// daemon is never involved.
#[cfg(unix)]
#[test]
fn a_stop_asked_for_over_the_socket_ends_the_process() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();

    let binary = env!("CARGO_BIN_EXE_proxenos");
    let mut daemon = std::process::Command::new(binary)
        .args(["run", "--port", "0"])
        .env("PROXENOS_HOME", &home)
        .env("TMPDIR", dir.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("the daemon should start");

    let socket = dir.path().join("proxenos.sock");
    for _ in 0..200 {
        if socket.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(socket.exists(), "the daemon never came up");

    let stopped = std::process::Command::new(binary)
        .arg("stop")
        .env("PROXENOS_HOME", &home)
        .env("TMPDIR", dir.path())
        .output()
        .expect("the stop verb should run");

    let said = String::from_utf8_lossy(&stopped.stdout);
    assert!(
        stopped.status.success(),
        "stop failed: {}",
        String::from_utf8_lossy(&stopped.stderr)
    );
    assert!(
        said.contains("stopped"),
        "it has to say what it observed: {said}"
    );
    assert!(
        said.contains("nothing started it again"),
        "nothing supervises this one, and saying so is the useful half: {said}"
    );

    // The process itself, not the socket: a daemon that stopped answering while
    // still running would be the worse of the two failures.
    let mut gone = false;
    for _ in 0..100 {
        match daemon.try_wait() {
            Ok(Some(_)) => {
                gone = true;
                break;
            }
            _ => std::thread::sleep(std::time::Duration::from_millis(50)),
        }
    }
    if !gone {
        let _ = daemon.kill();
    }
    assert!(
        gone,
        "the run loop is not listening to the stop it answered"
    );
}

/// The verb that replaces an old daemon cannot replace one older than itself.
///
/// `stop` exists so a running daemon can be swapped for the build on disk. A
/// daemon that predates `stop` has no method to ask, so this cannot work and
/// nothing here can make it — what it can do is say which situation this is,
/// instead of surfacing `unknown method` and leaving the reader to work out
/// that the raw protocol error is really an upgrade problem.
///
/// The stand-in answers the way an older daemon does: method-not-found.
#[cfg(unix)]
#[test]
fn stop_names_the_upgrade_problem_when_the_daemon_predates_it() {
    use std::io::BufRead;
    use std::io::BufReader;
    use std::io::Write;

    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("proxenos.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();

    // Faithful to what an older daemon does: it answers `status` perfectly well
    // and has never heard of `shutdown`.
    // Exactly two: `stop` reads what is answering, then asks it to stop, then
    // gives up on the refusal. A larger count would leave this thread waiting
    // for a connection that never comes.
    let server = std::thread::spawn(move || {
        for stream in listener.incoming().take(2) {
            let Ok(mut stream) = stream else { continue };
            let mut request = String::new();
            let _ = BufReader::new(stream.try_clone().unwrap()).read_line(&mut request);
            let reply = if request.contains("\"shutdown\"") {
                r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"unknown method `shutdown`"}}"#
            } else {
                r#"{"jsonrpc":"2.0","id":1,"result":{"version":"0.0.1-from-before"}}"#
            };
            let _ = writeln!(stream, "{reply}");
            let _ = stream.flush();
        }
    });

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_proxenos"))
        .arg("stop")
        .env("PROXENOS_HOME", dir.path())
        .env("TMPDIR", dir.path())
        .output()
        .expect("the binary should run");

    let _ = server.join();

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("older build") && stderr.contains("no `stop`"),
        "it has to name the situation rather than echo the protocol: {stderr}"
    );
    assert!(
        !stderr.contains("unknown method"),
        "the raw error is what this replaces: {stderr}"
    );
}

/// `run --detach` hands the terminal back and leaves a daemon behind.
///
/// The command's own exit is the observable: after it returns, the daemon it
/// started still answers the control socket, its output lands in a log file
/// rather than a terminal that no longer exists, and `stop` still ends it.
#[cfg(unix)]
#[test]
fn a_detached_run_outlives_the_command_that_started_it() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let binary = env!("CARGO_BIN_EXE_proxenos");

    let started = std::process::Command::new(binary)
        .args(["run", "--detach", "--port", "0"])
        .env("PROXENOS_HOME", &home)
        .env("TMPDIR", dir.path())
        .output()
        .expect("the detach command should run");

    assert!(
        started.status.success(),
        "detach failed: {}{}",
        String::from_utf8_lossy(&started.stdout),
        String::from_utf8_lossy(&started.stderr)
    );
    let said = String::from_utf8_lossy(&started.stdout);
    assert!(
        said.contains("proxenos stop"),
        "it should say how to stop what it started: {said}"
    );

    // The command has exited; what it left behind is answering, and its
    // output went to the log.
    let log = home.join("daemon.log");
    assert!(log.exists(), "the daemon's output needs somewhere to go");
    assert!(
        std::fs::read_to_string(&log).unwrap().contains("listening"),
        "the log should carry what the terminal no longer can"
    );

    let stopped = std::process::Command::new(binary)
        .arg("stop")
        .env("PROXENOS_HOME", &home)
        .env("TMPDIR", dir.path())
        .output()
        .expect("the stop verb should run");
    assert!(
        stopped.status.success(),
        "the detached daemon should have been answering: {}",
        String::from_utf8_lossy(&stopped.stderr)
    );
}

/// A child that dies at startup is reported from its own log, not summarized.
///
/// The port is held by a plain listener, so the spawned daemon fails at bind.
/// The command must exit nonzero and quote the reason the daemon itself gave.
#[cfg(unix)]
#[test]
fn a_detached_daemon_that_dies_at_startup_is_reported_from_its_log() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let binary = env!("CARGO_BIN_EXE_proxenos");

    let holder = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = holder.local_addr().unwrap().port().to_string();

    // The log survives across starts. A line from an earlier run must not be
    // quoted as the reason this one died.
    std::fs::write(home.join("daemon.log"), "STALE LINE FROM AN EARLIER RUN\n").unwrap();

    let started = std::process::Command::new(binary)
        .args(["run", "--detach", "--port", &port])
        .env("PROXENOS_HOME", &home)
        .env("TMPDIR", dir.path())
        .output()
        .expect("the detach command should run");

    assert!(
        !started.status.success(),
        "a daemon that never came up must not be reported as running"
    );
    let said = String::from_utf8_lossy(&started.stderr);
    assert!(
        said.contains("already in use"),
        "the daemon's own reason should be quoted: {said}"
    );
    assert!(
        !said.contains("STALE LINE"),
        "only this start's writes may be quoted: {said}"
    );
}

/// One daemon per control socket. A second detach is refused before it can
/// spawn a child that would steal the first one's socket file.
#[cfg(unix)]
#[test]
fn a_second_detach_is_refused_while_the_first_still_answers() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let binary = env!("CARGO_BIN_EXE_proxenos");

    let first = std::process::Command::new(binary)
        .args(["run", "--detach", "--port", "0"])
        .env("PROXENOS_HOME", &home)
        .env("TMPDIR", dir.path())
        .output()
        .expect("the first detach should run");
    assert!(
        first.status.success(),
        "the first detach should have worked: {}",
        String::from_utf8_lossy(&first.stderr)
    );

    let second = std::process::Command::new(binary)
        .args(["run", "--detach", "--port", "0"])
        .env("PROXENOS_HOME", &home)
        .env("TMPDIR", dir.path())
        .output()
        .expect("the second detach should run");
    assert!(
        !second.status.success(),
        "the second detach must be refused while the first answers"
    );
    assert!(
        String::from_utf8_lossy(&second.stderr).contains("already answering"),
        "it should say why: {}",
        String::from_utf8_lossy(&second.stderr)
    );

    let _ = std::process::Command::new(binary)
        .arg("stop")
        .env("PROXENOS_HOME", &home)
        .env("TMPDIR", dir.path())
        .output();
}

/// A launch that cannot carry the policy says so instead of dropping it quietly.
///
/// Only programs that take `--settings` are given the policy document. That is
/// by design — but a silent by-design is indistinguishable from a bug to the
/// person whose deny rule just vanished, so the launcher names what this launch
/// does not carry.
#[cfg(unix)]
#[test]
fn a_launch_that_cannot_carry_the_policy_names_the_loss() {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let bin = dir.path().join("bin");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&bin).unwrap();

    let stub = bin.join("tool");
    let mut file = std::fs::File::create(&stub).unwrap();
    file.write_all(b"#!/bin/sh\necho \"ARGV: $*\"\n").unwrap();
    drop(file);
    std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();

    let binary = env!("CARGO_BIN_EXE_proxenos");
    let mut daemon = std::process::Command::new(binary)
        .args(["run", "--port", "0"])
        .env("PROXENOS_HOME", &home)
        .env("TMPDIR", dir.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("the daemon should start");

    let socket = dir.path().join("proxenos.sock");
    for _ in 0..200 {
        if socket.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    let launched = std::process::Command::new(binary)
        .args(["exec", "tool"])
        .env("PROXENOS_HOME", &home)
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
        !stdout.contains("--settings"),
        "a program that does not take the flag must not be handed it: {stdout}"
    );
    assert!(
        stderr.contains("policy"),
        "the loss has to be named, not silent: {stderr}"
    );
}

/// The project was renamed at v0.5.0, and an environment still exporting the
/// old home variable is an operator who has not heard. Reading nothing and
/// starting with an empty store would look like every credential vanished;
/// the only honest answer is a refusal that names the new variable.
#[test]
fn the_old_home_variable_is_refused_by_name() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_proxenos"))
        .args(["status"])
        .env("CODEX_CC_PROXY_HOME", "/tmp/anywhere")
        .env_remove("PROXENOS_HOME")
        .output()
        .expect("the binary runs");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("CODEX_CC_PROXY_HOME") && stderr.contains("PROXENOS_HOME"),
        "the refusal names both the old variable and its replacement: {stderr}"
    );
}

/// `record ingress` runs a daemon, so the port controls the daemon controls
/// apply to it — including `PROXENOS_PORT`, which `run` documents. It used to
/// be dropped on this verb: the arguments were assembled by hand rather than
/// parsed, so the declared environment binding never ran, and the capture
/// daemon collided with whatever held the configured port.
#[test]
fn record_ingress_honours_the_port_variable() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).expect("the home");

    // A port that is free right now. Freed before the spawn, so a race is
    // possible but vanishingly unlikely inside one test process.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .expect("a free port")
        .local_addr()
        .expect("its address")
        .port();

    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_proxenos"))
        .args(["record", "ingress"])
        .env("PROXENOS_HOME", &home)
        .env("PROXENOS_PORT", port.to_string())
        .env("TMPDIR", dir.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("the record verb should start");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut answered = false;
    while std::time::Instant::now() < deadline {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            answered = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    let _ = child.kill();
    let _ = child.wait();
    assert!(
        answered,
        "the capture daemon should be listening on the port the variable named"
    );
}

/// A default home written by the old binary, with nothing yet at the new
/// path, is the same operator in the same situation without the variable.
/// The refusal names both directories and the move, and nothing is migrated
/// automatically: an old daemon may still be running against that directory.
#[test]
fn a_store_under_the_old_default_home_is_refused_with_the_move() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let old = dir.path().join("codex-cc-proxy");
    std::fs::create_dir_all(&old).expect("the old home");
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_proxenos"))
        .args(["status"])
        .env("XDG_CONFIG_HOME", dir.path())
        .env_remove("PROXENOS_HOME")
        .env_remove("CODEX_CC_PROXY_HOME")
        .output()
        .expect("the binary runs");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(old.to_string_lossy().as_ref())
            && stderr.contains(dir.path().join("proxenos").to_string_lossy().as_ref()),
        "the refusal names where the store is and where it moved: {stderr}"
    );
}

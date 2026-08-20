//! `docs/api.md` §2 — the account verbs, driven through the shipping binary.
//!
//! The assembly, not the parts. Features in this project have shipped inert
//! while every unit test passed, each time because nothing exercised the wiring
//! between a value handed in at startup and the thing meant to read it. These
//! start the real binary as a daemon and drive the real CLI against it, so a
//! selection that never reaches the store fails here rather than in someone's
//! session.
//!
//! Nothing here reaches the network. The catalog endpoint is pointed at a
//! loopback port with nothing on it, so the startup fetch fails immediately and
//! falls back; the stored grants carry expiries far enough out that no refresh
//! is due.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::indexing_slicing)]

use pretty_assertions::assert_eq;
use serde_json::json;

/// An unsigned JWT carrying the given claims. Nothing here verifies one.
fn id_token(account: &str) -> String {
    use base64::Engine;
    let encode = |value: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(value);
    let claims = json!({
        "email": format!("{account}@example.test"),
        "https://api.openai.com/auth": {
            "chatgpt_account_id": account,
            "chatgpt_plan_type": "plus",
        },
    });
    format!(
        "{}.{}.{}",
        encode(br#"{"alg":"none"}"#),
        encode(claims.to_string().as_bytes()),
        encode(b"signature")
    )
}

fn grant(account: &str) -> serde_json::Value {
    json!({
        "access_token": format!("access-{account}"),
        "refresh_token": format!("refresh-{account}"),
        "id_token": id_token(account),
        "account_id": account,
        // Far enough out that nothing is due, so no refresh is attempted and
        // no authorization server is contacted.
        "expires_at": 4_000_000_000_u64,
    })
}

/// A daemon of the shipping binary, on a home of its own.
struct Daemon {
    dir: tempfile::TempDir,
    process: std::process::Child,
}

impl Daemon {
    fn start(credentials: &serde_json::Value) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            home.join("credentials.json"),
            serde_json::to_string_pretty(credentials).unwrap(),
        )
        .unwrap();
        // A catalog endpoint with nothing behind it: the fetch fails at once
        // and the daemon falls back, which is the documented behaviour and
        // keeps this offline.
        std::fs::write(
            home.join("config.toml"),
            "[upstream]\ncatalog = \"http://127.0.0.1:1/models\"\n",
        )
        .unwrap();

        let process = std::process::Command::new(env!("CARGO_BIN_EXE_codex-cc-proxy"))
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
        assert!(socket.exists(), "the daemon never answered its socket");

        Self { dir, process }
    }

    /// One CLI verb, through the socket this daemon is serving.
    fn run(&self, args: &[&str]) -> String {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_codex-cc-proxy"))
            .args(args)
            .env("CODEX_CC_PROXY_HOME", self.dir.path().join("home"))
            .env("TMPDIR", self.dir.path())
            .output()
            .expect("the binary should run");
        assert!(
            output.status.success(),
            "`{}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

/// §8.1 — a credential file written before the store held more than one
/// account is read as the one account it describes, by the shipping binary,
/// with no re-login.
#[test]
fn the_binary_reads_a_credential_file_from_before_accounts() {
    let daemon = Daemon::start(&grant("acct_legacy"));

    let listed = daemon.run(&["accounts"]);

    assert!(
        listed.contains("acct_legacy"),
        "the migrated account should be listed: {listed}"
    );
    assert!(
        listed.starts_with('*'),
        "the only account serves turns: {listed}"
    );
    let status = daemon.run(&["status"]);
    assert!(
        status.contains("acct_legacy@example.test"),
        "status should name the account it is connected as: {status}"
    );
}

/// §2 — `accounts` lists, `accounts --use` switches, and the switch is what
/// the next turn would authenticate with.
#[test]
fn the_binary_lists_and_switches_accounts() {
    let daemon = Daemon::start(&json!({
        // The name, not the account id: `spare` is what this store calls the
        // account the backend knows as `acct_two`.
        "selected": "spare",
        "accounts": [
            { "name": "acct_one", "access_token": "access-acct_one",
              "refresh_token": "refresh-acct_one", "id_token": id_token("acct_one"),
              "account_id": "acct_one", "expires_at": 4_000_000_000_u64 },
            { "name": "spare", "access_token": "access-acct_two",
              "refresh_token": "refresh-acct_two", "id_token": id_token("acct_two"),
              "account_id": "acct_two", "expires_at": 4_000_000_000_u64 },
        ],
    }));

    let listed = daemon.run(&["accounts"]);
    let marked: Vec<&str> = listed
        .lines()
        .filter(|line| line.starts_with('*'))
        .collect();
    assert_eq!(
        marked.len(),
        1,
        "exactly one account serves turns: {listed}"
    );
    assert!(marked[0].contains("spare"), "{listed}");
    assert!(listed.contains("acct_one@example.test"), "{listed}");

    let switched = daemon.run(&["accounts", "--use", "acct_one"]);
    assert!(switched.contains("acct_one"), "{switched}");

    // The store every request authenticates through, as the daemon now reads
    // it: the account named is the one `status` reports being connected as.
    let status = daemon.run(&["status"]);
    assert!(
        status.contains("acct_one@example.test"),
        "the switch did not reach what serves turns: {status}"
    );
    let listed = daemon.run(&["accounts"]);
    let marked: Vec<&str> = listed
        .lines()
        .filter(|line| line.starts_with('*'))
        .collect();
    assert!(marked[0].contains("acct_one"), "{listed}");

    // And a name nobody holds is refused rather than silently ignored.
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_codex-cc-proxy"))
        .args(["accounts", "--use", "nobody"])
        .env("CODEX_CC_PROXY_HOME", daemon.dir.path().join("home"))
        .env("TMPDIR", daemon.dir.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("nobody"), "{stderr}");
    assert!(stderr.contains("acct_one"), "{stderr}");
}

/// §2 — `accounts --forget` through the shipping binary. The named account
/// goes, the rest stay usable, and something still serves turns.
#[test]
fn the_binary_forgets_one_account_and_keeps_the_rest() {
    let daemon = Daemon::start(&json!({
        "selected": "spare",
        "accounts": [
            { "name": "acct_one", "access_token": "access-acct_one",
              "refresh_token": "refresh-acct_one", "id_token": id_token("acct_one"),
              "account_id": "acct_one", "expires_at": 4_000_000_000_u64 },
            { "name": "spare", "access_token": "access-acct_two",
              "refresh_token": "refresh-acct_two", "id_token": id_token("acct_two"),
              "account_id": "acct_two", "expires_at": 4_000_000_000_u64 },
        ],
    }));

    let forgotten = daemon.run(&["accounts", "--forget", "spare"]);

    assert!(forgotten.contains("spare"), "{forgotten}");
    assert!(
        forgotten.contains("acct_one"),
        "it should say who serves turns now: {forgotten}"
    );

    let listed = daemon.run(&["accounts"]);
    assert!(!listed.contains("spare"), "{listed}");
    assert!(listed.starts_with('*'), "{listed}");
    assert!(listed.contains("acct_one"), "{listed}");

    // The last one can go too, and what is left says what to do about it.
    daemon.run(&["accounts", "--forget", "acct_one"]);
    assert!(daemon.run(&["accounts"]).contains("login"));
}

/// §2 — `accounts --rename` through the shipping binary. What changes is the
/// name `--use` takes; the grant and the account id stay where they are.
#[test]
fn the_binary_renames_an_account_without_touching_its_grant() {
    let daemon = Daemon::start(&grant("acct_legacy"));

    let renamed = daemon.run(&["accounts", "--rename", "acct_legacy", "work"]);
    assert!(renamed.contains("work"), "{renamed}");

    let listed = daemon.run(&["accounts"]);
    assert!(listed.contains("work"), "{listed}");
    assert!(listed.starts_with('*'), "it still serves turns: {listed}");
    // The address comes from the grant, so seeing it here is what says the
    // grant survived the rename.
    assert!(listed.contains("acct_legacy@example.test"), "{listed}");

    // The new name is what selects it now.
    daemon.run(&["accounts", "--use", "work"]);

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_codex-cc-proxy"))
        .args(["accounts", "--use", "acct_legacy"])
        .env("CODEX_CC_PROXY_HOME", daemon.dir.path().join("home"))
        .env("TMPDIR", daemon.dir.path())
        .output()
        .unwrap();
    assert!(!output.status.success(), "the old name should be gone");
}

/// §8 — a key is stored without a browser flow, and never through argv.
///
/// The secret arrives on stdin because non-negotiable #7 puts credentials out
/// of process arguments: an argument is visible to every process on the
/// machine and lands in shell history. The name is required, because a key
/// carries no id to be named by and the name is what `--use` takes.
#[test]
fn the_binary_stores_a_key_from_stdin_and_serves_turns_as_it() {
    use std::io::Write;

    let daemon = Daemon::start(&grant("acct_legacy"));
    let home = daemon.dir.path().join("home");

    let mut login = std::process::Command::new(env!("CARGO_BIN_EXE_codex-cc-proxy"))
        .args(["login", "--key", "--as", "billing"])
        .env("CODEX_CC_PROXY_HOME", &home)
        .env("TMPDIR", daemon.dir.path())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("the binary should run");
    login
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"sk-probe-9c21-not-in-argv\n")
        .unwrap();
    let stored = login.wait_with_output().unwrap();
    assert!(
        stored.status.success(),
        "{}",
        String::from_utf8_lossy(&stored.stderr)
    );
    assert!(
        String::from_utf8_lossy(&stored.stdout).contains("billing"),
        "it should say what it stored: {}",
        String::from_utf8_lossy(&stored.stdout)
    );

    // Both accounts are there, and the key is the one serving turns.
    let listed = daemon.run(&["accounts"]);
    assert!(listed.contains("acct_legacy"), "{listed}");
    assert!(
        listed
            .lines()
            .any(|line| line.starts_with('*') && line.contains("billing")),
        "the key should be serving turns: {listed}"
    );
    assert!(
        !listed.contains("sk-probe"),
        "the key reached a listing: {listed}"
    );
    assert!(daemon.run(&["status"]).contains("billing"));

    // Switching back and forth is switching accounts, nothing more.
    daemon.run(&["accounts", "--use", "acct_legacy"]);
    assert!(
        daemon
            .run(&["accounts"])
            .lines()
            .any(|line| { line.starts_with('*') && line.contains("acct_legacy") })
    );
    daemon.run(&["accounts", "--use", "billing"]);

    // And the secret is not in the file's listing of names either.
    let stored = std::fs::read_to_string(home.join("credentials.json")).unwrap();
    assert!(
        stored.contains("sk-probe-9c21-not-in-argv"),
        "it has to be stored somewhere"
    );
    assert!(stored.contains("\"kind\": \"key\""), "{stored}");
}

/// A login through the CLI hands over to a running daemon.
///
/// The daemon reads the credential file on every request, so the account moves
/// either way. What does not move on its own is what a switch carries with it:
/// the conversations bound to the previous account keep the endpoint they
/// dialed, and after a change of kind that endpoint refuses every turn.
#[test]
fn a_cli_login_tells_a_running_daemon_to_hand_over() {
    use std::io::Write;

    let daemon = Daemon::start(&grant("acct_legacy"));
    let home = daemon.dir.path().join("home");

    let mut login = std::process::Command::new(env!("CARGO_BIN_EXE_codex-cc-proxy"))
        .args(["login", "--key", "--as", "billing"])
        .env("CODEX_CC_PROXY_HOME", &home)
        .env("TMPDIR", daemon.dir.path())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("the binary should run");
    login
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"sk-handover-probe\n")
        .unwrap();
    let stored = login.wait_with_output().unwrap();
    let said = String::from_utf8_lossy(&stored.stdout);

    assert!(
        said.contains("running daemon"),
        "it should say it told the daemon: {said}"
    );
    assert!(daemon.run(&["status"]).contains("billing"));
}

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

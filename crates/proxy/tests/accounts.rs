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
        Self::start_with_config(credentials, "")
    }

    /// The same daemon, with more written into its configuration file.
    ///
    /// A catalog endpoint with nothing behind it: the fetch fails at once and
    /// the daemon falls back, which is the documented behaviour and keeps this
    /// offline.
    fn start_with_config(credentials: &serde_json::Value, extra: &str) -> Self {
        Self::start_with_file(
            credentials,
            &format!("{extra}\n[upstream]\ncatalog = \"http://127.0.0.1:1/models\"\n"),
        )
    }

    /// The same daemon, on a configuration file written exactly as given —
    /// for the cases that need the catalog section to say something else.
    fn start_with_file(credentials: &serde_json::Value, config: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            home.join("credentials.json"),
            serde_json::to_string_pretty(credentials).unwrap(),
        )
        .unwrap();
        std::fs::write(home.join("config.toml"), config).unwrap();

        let process = std::process::Command::new(env!("CARGO_BIN_EXE_proxenos"))
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
        assert!(socket.exists(), "the daemon never answered its socket");

        Self { dir, process }
    }

    /// One CLI verb, through the socket this daemon is serving.
    fn run(&self, args: &[&str]) -> String {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_proxenos"))
            .args(args)
            .env("PROXENOS_HOME", self.dir.path().join("home"))
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
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_proxenos"))
        .args(["accounts", "--use", "nobody"])
        .env("PROXENOS_HOME", daemon.dir.path().join("home"))
        .env("TMPDIR", daemon.dir.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("nobody"), "{stderr}");
    assert!(stderr.contains("acct_one"), "{stderr}");
}

/// An account states its provider, and everything that reports the account
/// says so — the roadmap's first rule for a second provider. The default is
/// the provider this project started with, so every credential file written
/// before the field existed reads unchanged, and the word appears only where
/// it is not that default: a listing that stamped the same provider on every
/// line would say nothing.
#[test]
fn an_account_states_its_provider_and_the_reports_name_it() {
    let daemon = Daemon::start(&json!({
        "selected": "work",
        "accounts": [
            { "name": "work", "access_token": "access-acct_one",
              "refresh_token": "refresh-acct_one", "id_token": id_token("acct_one"),
              "account_id": "acct_one", "expires_at": 4_000_000_000_u64 },
            { "name": "claude", "kind": "key", "api_key": "sk-test-not-a-real-key",
              "provider": "anthropic" },
        ],
    }));

    let listed = daemon.run(&["accounts"]);
    let line_of = |name: &str| {
        listed
            .lines()
            .find(|line| line.contains(name))
            .unwrap_or_else(|| panic!("{name} should be listed: {listed}"))
            .to_owned()
    };
    assert!(
        line_of("claude").contains("anthropic"),
        "the account of the other provider must say so: {listed}"
    );
    assert!(
        !line_of("work").contains("codex") && !line_of("work").contains("anthropic"),
        "the default provider is not stamped on every line: {listed}"
    );

    // The switch rewrites the credential file, so this also holds the field
    // through a round-trip: a write that dropped it would strand the account
    // back on the default provider silently.
    let _ = daemon.run(&["accounts", "--use", "claude"]);
    let status = daemon.run(&["status"]);
    assert!(
        status.contains("anthropic"),
        "status reports the provider of the account serving turns: {status}"
    );
    let relisted = daemon.run(&["accounts"]);
    assert!(
        relisted
            .lines()
            .find(|line| line.contains("claude"))
            .is_some_and(|line| line.contains("anthropic")),
        "the provider must survive the file being rewritten: {relisted}"
    );
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

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_proxenos"))
        .args(["accounts", "--use", "acct_legacy"])
        .env("PROXENOS_HOME", daemon.dir.path().join("home"))
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

    let mut login = std::process::Command::new(env!("CARGO_BIN_EXE_proxenos"))
        .args(["login", "--key", "--as", "billing"])
        .env("PROXENOS_HOME", &home)
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

    let mut login = std::process::Command::new(env!("CARGO_BIN_EXE_proxenos"))
        .args(["login", "--key", "--as", "billing"])
        .env("PROXENOS_HOME", &home)
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

/// §2 — a live probe run that cannot authenticate says so, once.
///
/// It used to answer with the whole capability matrix, every row failed and
/// the header claiming the backend answered and was billed. Nothing had been
/// sent and nothing had been billed: the transport never got a token. A matrix
/// that reports seven capabilities as broken when the credential is what is
/// missing sends whoever reads it to the wrong place entirely — and it is the
/// same failure the probes exist to prevent, printed the other way round.
#[test]
fn a_live_probe_run_without_a_credential_refuses_rather_than_reporting_failures() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_proxenos"))
        .args(["doctor", "--live"])
        .env("PROXENOS_HOME", &home)
        .env("TMPDIR", dir.path())
        .output()
        .expect("the binary should run");

    assert!(!output.status.success(), "it cannot have succeeded");
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        said.contains("login"),
        "it should say what is missing: {said}"
    );
    assert!(
        !said.contains("Capability matrix"),
        "nothing was probed, so nothing may be reported about the backend: {said}"
    );
    assert!(
        !said.contains("billed"),
        "nothing was sent, so nothing was billed: {said}"
    );
}

/// §4 — the mapping the daemon serves is the selected account's, through the
/// shipping binary.
///
/// A catalog is one account's menu, so a mapping written for one account can
/// name a model another is not offered. This is the wiring half of that: the
/// override is in the file, the account it names is the one selected, and what
/// `status` reports has to be the override rather than the shared table.
#[test]
fn the_binary_serves_the_selected_accounts_mapping() {
    let daemon = Daemon::start_with_config(
        &json!({
            "selected": "acct_two",
            "accounts": [
                { "name": "acct_one", "access_token": "access-acct_one",
                  "refresh_token": "refresh-acct_one", "id_token": id_token("acct_one"),
                  "account_id": "acct_one", "expires_at": 4_000_000_000_u64 },
                { "name": "acct_two", "access_token": "access-acct_two",
                  "refresh_token": "refresh-acct_two", "id_token": id_token("acct_two"),
                  "account_id": "acct_two", "expires_at": 4_000_000_000_u64 },
            ],
        }),
        "[tiers]\nopus = \"shared-opus\"\n\n[accounts.acct_two.tiers]\nopus = \"only-for-two\"\n",
    );

    let status = daemon.run(&["status"]);

    assert!(
        status.contains("only-for-two"),
        "the selected account's override should be what serves turns: {status}"
    );
    assert!(
        !status.contains("shared-opus"),
        "the shared mapping should have been replaced: {status}"
    );
}

/// §7.1 — a pinned tier names another account's menu, so the catalog fetched
/// for the serving account does not decide whether it is valid.
///
/// The daemon's start and `tiers.set` are two doors onto one rule, and only one
/// of them held it: `tiers.set` excluded a pinned entry and startup measured it
/// against the serving account's list anyway. So a mapping written through the
/// socket and then persisted refused the daemon at the next start — the exact
/// silent-until-restart failure the write-time check exists to prevent.
///
/// Driven against an authoritative catalog, because the fallback list validates
/// nothing and would pass this whatever the rule was.
#[tokio::test(flavor = "multi_thread")]
async fn a_pinned_tier_does_not_have_to_be_on_the_serving_accounts_catalog() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let app = axum::Router::new().route(
            "/models",
            axum::routing::get(|| async {
                r#"{"data":[{"id":"gpt-5.6-terra","context_window":272000}]}"#
            }),
        );
        let _ = axum::serve(listener, app).await;
    });

    let daemon = Daemon::start_with_file(
        &json!({
            "selected": "work",
            "accounts": [
                { "name": "work", "access_token": "access-acct_one",
                  "refresh_token": "refresh-acct_one", "id_token": id_token("acct_one"),
                  "account_id": "acct_one", "expires_at": 4_000_000_000_u64 },
                { "name": "spare", "access_token": "access-acct_two",
                  "refresh_token": "refresh-acct_two", "id_token": id_token("acct_two"),
                  "account_id": "acct_two", "expires_at": 4_000_000_000_u64 },
            ],
        }),
        &format!(
            "cross_account_tiers = true\n\
             [tiers]\n\
             opus = \"gpt-5.6-terra\"\n\
             sonnet = \"gpt-5.6-terra\"\n\
             fable = \"gpt-5.6-terra\"\n\
             haiku = {{ account = \"spare\", model = \"gpt-5.5\" }}\n\
             [upstream]\n\
             catalog = \"http://{addr}/models\"\n"
        ),
    );

    // The daemon answered at all, which is the assertion: `gpt-5.5` is not on
    // the list this catalog served, and it is not that list's to refuse.
    let status = daemon.run(&["status"]);
    assert!(
        status.contains("gpt-5.5"),
        "the pinned mapping should be in force: {status}"
    );
}

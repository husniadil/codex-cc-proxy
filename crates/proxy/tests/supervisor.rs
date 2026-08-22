//! `docs/api.md` §2.6 — the supervisor unit.
//!
//! Everything here is a pure function over data: the rendering of the unit, the
//! path decisions, and the refusal on a platform this cannot supervise. No test
//! writes into the real `~/Library/LaunchAgents`, contacts the network, or
//! touches a running daemon.

use proxenos::supervisor::Origin;
use proxenos::supervisor::Platform;
use proxenos::supervisor::plan;
use proxenos::supervisor::render;
use std::path::PathBuf;

fn origin() -> Origin {
    Origin {
        program: PathBuf::from("/Users/someone/.local/bin/proxenos"),
        log: PathBuf::from("/Users/someone/.config/proxenos/daemon.log"),
        proxenos_home: None,
        tmpdir: Some("/var/folders/j2/abcdef/T/".into()),
    }
}

/// A platform with no launchd refuses and says what it would take, rather than
/// writing a file that supervises nothing. A half-installed unit that silently
/// never runs is worse than no verb at all, and the operator cannot tell the
/// two apart from the outside.
#[test]
fn a_platform_this_cannot_supervise_is_refused_by_name() {
    let error = plan(&Platform::Other("linux"), &origin()).unwrap_err();
    let message = error.to_string();

    assert!(message.contains("linux"), "{message}");
    assert!(
        message.contains("launchd"),
        "the refusal names the one supervisor this implements: {message}"
    );
    assert!(
        message.contains("systemd"),
        "and names what supervising this platform would take: {message}"
    );
    assert!(
        message.contains("run --detach"),
        "and names what to do meanwhile: {message}"
    );
}

/// The hazard this verb exists to avoid: a supervised daemon binding one socket
/// path while the operator's CLI dials another. Both halves derive the path from
/// the same function, and the unit carries the inputs explicitly so a launchd
/// process cannot be handed a different `TMPDIR` than the shell that installed
/// it.
#[test]
fn the_socket_the_unit_binds_is_the_one_the_cli_dials() {
    for home in [None, Some("/Users/someone/px".into())] {
        let origin = Origin {
            proxenos_home: home,
            ..origin()
        };
        let unit = plan(&Platform::MacOs, &origin).unwrap();

        let carried: Vec<(String, String)> = unit.environment.clone();
        let tmpdir = carried
            .iter()
            .find(|(key, _)| key == "TMPDIR")
            .map(|(_, value)| PathBuf::from(value));
        let named_home = carried
            .iter()
            .find(|(key, _)| key == "PROXENOS_HOME")
            .map(|(_, value)| PathBuf::from(value));

        // What the supervised daemon will derive, from what the unit hands it.
        let bound = proxenos::control::path_for(named_home.as_deref(), tmpdir.as_deref());

        assert_eq!(
            unit.socket, bound,
            "the unit's own socket and the one its environment produces must agree"
        );
        assert_eq!(
            bound,
            proxenos::control::path_for(
                origin.proxenos_home.as_ref().map(PathBuf::from).as_deref(),
                origin.tmpdir.as_ref().map(PathBuf::from).as_deref(),
            ),
            "and both must equal what the installing shell's CLI dials"
        );
    }
}

/// A `TMPDIR` deep enough to push the socket past the platform's address limit
/// fails at bind, after the HTTP listener is already up — the daemon looks
/// healthy and every CLI verb gets connection refused. It is refused here, where
/// the path is chosen, rather than there.
#[test]
fn a_socket_path_the_platform_cannot_address_is_refused() {
    let deep = format!("/var/folders/{}/T/", "a".repeat(120));
    let origin = Origin {
        tmpdir: Some(deep.into()),
        ..origin()
    };

    let message = plan(&Platform::MacOs, &origin).unwrap_err().to_string();
    assert!(message.contains("unix socket address"), "{message}");
}

/// launchd resolves nothing: a relative program in a plist is a unit that never
/// starts, reported nowhere.
#[test]
fn a_program_launchd_cannot_resolve_is_refused() {
    let origin = Origin {
        program: PathBuf::from("target/release/proxenos"),
        ..origin()
    };

    let message = plan(&Platform::MacOs, &origin).unwrap_err().to_string();
    assert!(message.contains("absolute"), "{message}");
}

/// The unit runs `run` in the foreground and keeps it alive. Not `--detach`:
/// a process that forks away leaves launchd supervising something that has
/// already exited, and the respawn loop then fights the daemon it cannot see.
#[test]
fn the_unit_runs_the_daemon_in_the_foreground_and_keeps_it_alive() {
    let unit = plan(&Platform::MacOs, &origin()).unwrap();
    let document = render(&unit);

    assert_eq!(unit.arguments, vec!["run".to_owned()]);
    assert!(!document.contains("--detach"), "{document}");
    assert!(
        document.contains("<key>KeepAlive</key>\n    <true/>"),
        "{document}"
    );
    assert!(
        document.contains("<key>RunAtLoad</key>\n    <true/>"),
        "{document}"
    );
    assert!(
        document.contains("/Users/someone/.local/bin/proxenos"),
        "{document}"
    );
}

/// It logs where the daemon already logs, so a supervised start and a detached
/// one leave one file to read rather than two.
#[test]
fn the_unit_logs_where_the_daemon_already_logs() {
    let unit = plan(&Platform::MacOs, &origin()).unwrap();
    let document = render(&unit);

    assert_eq!(
        unit.log,
        PathBuf::from("/Users/someone/.config/proxenos/daemon.log")
    );
    assert!(
        document.contains("<key>StandardOutPath</key>\n    <string>/Users/someone/.config/proxenos/daemon.log</string>"),
        "{document}"
    );
    assert!(
        document.contains("<key>StandardErrorPath</key>\n    <string>/Users/someone/.config/proxenos/daemon.log</string>"),
        "{document}"
    );
}

/// A plist in the user's home is a world-readable file. The store holds
/// credentials; this holds none, and the set it may carry is closed rather than
/// filtered, so a later addition has to be made deliberately.
#[test]
fn the_unit_carries_no_credential() {
    let origin = Origin {
        proxenos_home: Some("/Users/someone/px".into()),
        ..origin()
    };
    let unit = plan(&Platform::MacOs, &origin).unwrap();

    let keys: Vec<&str> = unit
        .environment
        .iter()
        .map(|(key, _)| key.as_str())
        .collect();
    assert_eq!(keys, vec!["PROXENOS_HOME", "TMPDIR"]);

    let document = render(&unit).to_ascii_lowercase();
    for forbidden in ["token", "secret", "api_key", "apikey", "password", "auth"] {
        assert!(
            !document.contains(forbidden),
            "the unit must carry no credential, found {forbidden}"
        );
    }
}

/// A path with an XML metacharacter in it renders as data rather than as markup.
#[test]
fn a_path_carrying_markup_is_escaped() {
    let origin = Origin {
        log: PathBuf::from("/Users/a&b/<x>/daemon.log"),
        ..origin()
    };
    let document = render(&plan(&Platform::MacOs, &origin).unwrap());

    assert!(
        document.contains("/Users/a&amp;b/&lt;x&gt;/daemon.log"),
        "{document}"
    );
    assert!(!document.contains("/Users/a&b/"), "{document}");
}

/// The installed unit is compared against the one this environment would write,
/// as text. A `TMPDIR` that has moved since install renders a different
/// document, and that is precisely the case where the daemon binds one socket
/// path and the operator's CLI dials another — healthy on the port, connection
/// refused on every verb. Saying so is the whole point of the `status` verb.
#[test]
fn a_unit_installed_from_a_different_environment_is_reported_as_divergent() {
    let wanted = plan(&Platform::MacOs, &origin()).unwrap();

    let elsewhere = Origin {
        tmpdir: Some("/var/folders/zz/999999/T/".into()),
        ..origin()
    };
    let stale = render(&plan(&Platform::MacOs, &elsewhere).unwrap());

    assert_eq!(
        proxenos::supervisor::compare(None, &wanted),
        proxenos::supervisor::Installed::Absent
    );
    assert_eq!(
        proxenos::supervisor::compare(Some(&render(&wanted)), &wanted),
        proxenos::supervisor::Installed::Current
    );
    assert_eq!(
        proxenos::supervisor::compare(Some(&stale), &wanted),
        proxenos::supervisor::Installed::Divergent
    );
}

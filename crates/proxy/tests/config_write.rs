//! `docs/api.md` §3 — writing a runtime change back to the configuration file.
//!
//! The file is not a serialized struct. It is a document whose comments explain
//! why each key is what it is, and most of those comments exist because the
//! obvious value is wrong in a way that does not fail loudly. Rewriting it by
//! re-serializing the parsed configuration would throw all of that away and the
//! loss would be invisible — the file would still parse, still work, and never
//! again explain itself.
//!
//! So the edit is surgical: one value on one line, everything else byte for
//! byte.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::indexing_slicing)]

use pretty_assertions::assert_eq;
use proxenos::config::edit;

const DOCUMENT: &str = r#"# Every key here has a default.

port = 8787

# Capped again by what the model accepts.
# effort = "low"

# WebFetch runs on the haiku tier, so that one matters more than it looks.
[tiers]
opus   = "gpt-5.6-terra"
sonnet = "gpt-5.6-luna"
haiku  = "gpt-5.6-luna"

[transport]
websocket = true
"#;

#[test]
fn setting_a_tier_changes_one_value_and_nothing_else() {
    let written = edit::set_tier(DOCUMENT, None, "sonnet", "gpt-5.4-mini", None).unwrap();

    assert!(written.contains(r#"sonnet = "gpt-5.4-mini""#));
    // Every comment survives, including the one explaining the tier that was
    // not touched.
    assert!(written.contains("# WebFetch runs on the haiku tier"));
    assert!(written.contains("# Every key here has a default."));
    assert!(written.contains("# Capped again by what the model accepts."));
    // And so does every other value.
    assert!(written.contains(r#"opus   = "gpt-5.6-terra""#));
    assert!(written.contains(r#"haiku  = "gpt-5.6-luna""#));
    assert!(written.contains("websocket = true"));
    assert!(written.contains("port = 8787"));
}

/// A tier the file does not state takes its default, so there is no line to
/// edit — it has to be added, and added *inside* `[tiers]`.
///
/// In TOML a bare key belongs to the table above it, so appending at the end of
/// the file would write `transport.fable`, which is a different setting that
/// nothing reads. The file would still parse and the tier would still be
/// defaulted.
#[test]
fn a_tier_the_file_never_stated_is_added_inside_its_table() {
    let written = edit::set_tier(DOCUMENT, None, "fable", "gpt-5.6-sol", None).unwrap();

    let tiers = written.find("[tiers]").unwrap();
    let transport = written.find("[transport]").unwrap();
    let fable = written.find("fable").unwrap();

    assert!(
        fable > tiers && fable < transport,
        "a new tier must land inside [tiers], not under the table that follows it:\n{written}"
    );
    assert_eq!(
        toml::from_str::<toml::Value>(&written).unwrap()["tiers"]["fable"].as_str(),
        Some("gpt-5.6-sol")
    );
}

/// A pinned tier is written in the table form the file reads back:
/// `haiku = { account = "spare", model = "..." }` — replacing a bare-string
/// line where one exists, so the file cannot end up with both forms live.
#[test]
fn a_pinned_tier_is_written_in_table_form() {
    let written =
        edit::set_tier(DOCUMENT, None, "haiku", "claude-haiku-4-5", Some("spare")).unwrap();

    let parsed: toml::Value = toml::from_str(&written).unwrap();
    assert_eq!(
        parsed["tiers"]["haiku"]["account"].as_str(),
        Some("spare"),
        "{written}"
    );
    assert_eq!(
        parsed["tiers"]["haiku"]["model"].as_str(),
        Some("claude-haiku-4-5"),
        "{written}"
    );
    // The bare form it replaced is gone, and everything else survives.
    assert!(!written.contains(r#"haiku  = "gpt-5.6-luna""#), "{written}");
    assert!(written.contains("# WebFetch runs on the haiku tier"));
}

/// A file with no `[tiers]` table at all gains one.
#[test]
fn a_file_without_the_table_gains_it() {
    let written = edit::set_tier("port = 8787\n", None, "opus", "gpt-5.6-terra", None).unwrap();

    assert_eq!(
        toml::from_str::<toml::Value>(&written).unwrap()["tiers"]["opus"].as_str(),
        Some("gpt-5.6-terra")
    );
    assert!(written.contains("port = 8787"));
}

/// The effort ceiling is a bare key above every table, and it is commented out
/// in the shipped example. Setting it must produce a *live* key, not a second
/// line that leaves the commented one looking authoritative.
#[test]
fn setting_the_effort_ceiling_uncomments_rather_than_duplicates() {
    let written = edit::set_effort(DOCUMENT, None, Some("high")).unwrap();

    assert!(written.contains(r#"effort = "high""#));
    assert_eq!(
        written.matches("effort =").count(),
        1,
        "one effort line, not a live one beside a commented one:\n{written}"
    );
    assert_eq!(
        toml::from_str::<toml::Value>(&written).unwrap()["effort"].as_str(),
        Some("high")
    );
}

/// Removing the ceiling comments the key out rather than deleting the line, so
/// the explanation above it still has something to explain.
#[test]
fn removing_the_effort_ceiling_leaves_the_key_commented() {
    let set = edit::set_effort(DOCUMENT, None, Some("high")).unwrap();
    let removed = edit::set_effort(&set, None, None).unwrap();

    assert!(removed.contains("# effort = "));
    assert!(
        toml::from_str::<toml::Value>(&removed)
            .unwrap()
            .get("effort")
            .is_none()
    );
    assert!(removed.contains("# Capped again by what the model accepts."));
}

/// A bare key must never be written below a table header.
///
/// This is the one mistake that produces a file which parses, loads, and means
/// something else entirely — `tiers.effort` rather than `effort` — and nothing
/// about it looks wrong.
#[test]
fn the_effort_key_is_never_written_under_a_table() {
    let written =
        edit::set_effort("[tiers]\nopus = \"gpt-5.6-terra\"\n", None, Some("low")).unwrap();

    let parsed: toml::Value = toml::from_str(&written).unwrap();
    assert_eq!(parsed["effort"].as_str(), Some("low"));
    assert!(
        parsed["tiers"].get("effort").is_none(),
        "the ceiling landed inside [tiers]:\n{written}"
    );
}

/// A commented-out `effort` line that sits below a table header is not the key
/// this setting means, and must not be rewritten into a live one.
///
/// In TOML a bare key belongs to the table above it, so replacing that line
/// writes `tiers.effort` — which parses, which nothing reads, and which leaves
/// the daemon starting with no ceiling at all after the operator asked for one.
/// The `None` branch already guards the placement; this is the same rule for
/// the branch that replaces an existing line.
#[test]
fn an_effort_key_below_a_table_header_is_not_the_one_that_gets_set() {
    let document = "port = 8787\n\n[tiers]\nopus = \"gpt-5.6-terra\"\n# effort = \"low\"\n";

    let written = edit::set_effort(document, None, Some("high")).unwrap();

    let parsed: toml::Value = toml::from_str(&written).unwrap();
    assert_eq!(
        parsed["effort"].as_str(),
        Some("high"),
        "the ceiling must land at the top level:\n{written}"
    );
    assert!(
        parsed["tiers"].get("effort").is_none(),
        "the ceiling landed inside [tiers]:\n{written}"
    );
}

/// Removing it has the same hazard in reverse: commenting out a live key is
/// fine wherever it is, but a file whose only `effort` line is under a table
/// must not gain a second one there.
#[test]
fn removing_a_ceiling_from_a_file_that_never_had_one_adds_nothing_under_a_table() {
    let written = edit::set_effort("[tiers]\nopus = \"gpt-5.6-terra\"\n", None, None).unwrap();

    let parsed: toml::Value = toml::from_str(&written).unwrap();
    assert!(parsed.get("effort").is_none());
    assert!(
        parsed["tiers"].get("effort").is_none(),
        "a commented key was written under [tiers]:\n{written}"
    );
}

/// A quoted account header is the same table as a bare one, and is edited
/// rather than duplicated.
///
/// TOML allows both spellings for one table, so appending a second is a table
/// defined twice — a file that no longer parses, from an edit that reported
/// success.
#[test]
fn a_quoted_account_header_is_the_same_table() {
    let document = "[accounts.\"spare\".tiers]\nopus = \"old\"\n";

    let written = edit::set_tier(document, Some("spare"), "opus", "new", None).unwrap();

    let parsed: toml::Value = toml::from_str(&written).unwrap();
    assert_eq!(
        parsed["accounts"]["spare"]["tiers"]["opus"].as_str(),
        Some("new")
    );
    assert_eq!(
        written.matches("tiers]").count(),
        1,
        "a second table was appended: {written}"
    );
}

/// The same, for the account's own table.
#[test]
fn a_quoted_account_header_is_the_same_table_for_the_ceiling() {
    let document = "[accounts.\"spare\"]\neffort = \"high\"\n";

    let written = edit::set_effort(document, Some("spare"), Some("low")).unwrap();

    let parsed: toml::Value = toml::from_str(&written).unwrap();
    assert_eq!(parsed["accounts"]["spare"]["effort"].as_str(), Some("low"));
}

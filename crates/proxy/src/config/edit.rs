//! Changing one value in the configuration file without rewriting the file.
//!
//! **The file is a document, not a serialized struct.** Its comments explain why
//! each key is what it is, and most of them exist because the obvious value is
//! wrong in a way that does not fail loudly. Re-serializing the parsed
//! configuration would discard every one of them, and the loss would be
//! invisible: the file would still parse, still work, and never again explain
//! itself.
//!
//! So these functions edit text. One value on one line; everything else survives
//! byte for byte.
//!
//! The one mistake worth naming, because it produces a file that parses and
//! means something else: in TOML a bare key belongs to the table above it. An
//! `effort` appended at the end of a file that ends in `[transport]` is
//! `transport.effort`, which nothing reads, and a tier appended after `[tiers]`
//! has ended lands in whatever table follows. Both look right.

use crate::error::ProxyError;

/// Point a tier at a model, in the text of the file.
///
/// `account` names whose mapping it is: `None` for the shared `[tiers]` table,
/// a name for that account's own `[accounts.<name>.tiers]`. Written where the
/// value will be read from — an account section shadows the shared table for
/// the tiers it names (§4), so writing the shared one there would leave the
/// change applied on a running daemon and gone at the next start.
///
/// `pin` is a different account entirely: the one the tier's *turns* are served
/// as. It writes the table form — `haiku = { account = "…", model = "…" }` —
/// replacing whatever form the line had, so the file never carries both.
pub fn set_tier(
    document: &str,
    account: Option<&str>,
    tier: &str,
    model: &str,
    pin: Option<&str>,
) -> Result<String, ProxyError> {
    if !crate::config::TIER_NAMES.contains(&tier) {
        return Err(ProxyError::invalid_request(format!(
            "unknown tier `{tier}`; expected one of: {}",
            crate::config::TIER_NAMES.join(", ")
        )));
    }

    let rendered = match pin {
        Some(pin) => format!("{{ account = \"{pin}\", model = \"{model}\" }}"),
        None => format!("\"{model}\""),
    };

    let header = match account {
        Some(account) => format!("[accounts.{}.tiers]", key(account)),
        None => "[tiers]".to_owned(),
    };

    let mut lines: Vec<String> = document.lines().map(str::to_owned).collect();

    let table = lines
        .iter()
        .position(|line| opens_table(line, account, ".tiers"));

    let Some(start) = table else {
        // No table at all. Appending one at the end is safe in a way appending
        // a bare key never is: a table header ends whatever table preceded it.
        let mut written = document.trim_end().to_owned();
        written.push_str(&format!("\n\n{header}\n{tier} = {rendered}\n"));
        return Ok(written);
    };

    // Where this table ends: the next table header, or the end of the file.
    let end = lines
        .iter()
        .skip(start + 1)
        .position(|line| line.trim_start().starts_with('['))
        .map_or(lines.len(), |offset| start + 1 + offset);

    let body = lines.get(start + 1..end).unwrap_or_default();
    let existing = body
        .iter()
        .position(|line| is_assignment_to(line, tier))
        .map(|offset| start + 1 + offset);

    match existing {
        Some(index) => {
            // The original spacing is kept — these keys are aligned in the
            // shipped file, and a rewrite that broke the alignment would be a
            // diff about nothing.
            let padding = lines
                .get(index)
                .map_or_else(|| " ".to_owned(), |line: &String| alignment(line, tier));
            if let Some(line) = lines.get_mut(index) {
                *line = format!("{tier}{padding}= {rendered}");
            }
        }
        None => {
            // Inserted at the end of *this* table, after its last non-blank
            // line, so it cannot drift under the next header.
            let insert = body
                .iter()
                .rposition(|line| !line.trim().is_empty())
                .map_or(start + 1, |offset| start + 1 + offset + 1);
            lines.insert(insert, format!("{tier} = {rendered}"));
        }
    }

    Ok(with_trailing_newline(lines.join("\n")))
}

/// Set or remove the effort ceiling, in the text of the file.
///
/// `None` comments the key out rather than deleting the line: the explanation
/// above it is worth keeping, and a commented key still shows what the setting
/// is called.
pub fn set_effort(
    document: &str,
    account: Option<&str>,
    effort: Option<&str>,
) -> Result<String, ProxyError> {
    if let Some(effort) = effort {
        // Rejected here rather than written and refused at the next startup,
        // which would leave a daemon that will not start and a file that looks
        // like someone meant it.
        crate::config::parse_effort(effort)?;
    }

    // An account's ceiling is a key inside its own table, so it is written the
    // way a tier is rather than the way the shared one is. The shared one is
    // the special case: it is a bare key, and a bare key belongs to the table
    // above it.
    if let Some(account) = account {
        return set_in_table(document, account, "effort", effort);
    }

    let mut lines: Vec<String> = document.lines().map(str::to_owned).collect();

    // Only the region above the first table header is this setting's. A bare
    // key belongs to the table above it, so an `effort` line below one is
    // `tiers.effort` — a different key that nothing reads. Rewriting it would
    // produce a file that parses, loads, and leaves the daemon with no ceiling
    // at all after the operator asked for one.
    let first_table = lines
        .iter()
        .position(|line| line.trim_start().starts_with('['))
        .unwrap_or(lines.len());

    // A commented-out key counts: the shipped file ships it that way, and
    // writing a second line would leave the commented one looking like the
    // setting while a live one below it actually decides.
    let existing = lines
        .get(..first_table)
        .unwrap_or_default()
        .iter()
        .position(|line| {
            let bare = line.trim_start().trim_start_matches('#').trim_start();
            is_assignment_to(bare, "effort")
        });

    let line = match effort {
        Some(effort) => format!("effort = \"{effort}\""),
        None => "# effort = \"low\"".to_owned(),
    };

    match existing {
        Some(index) => {
            if let Some(existing) = lines.get_mut(index) {
                *existing = line;
            }
        }
        None => {
            // Above every table header, for the same reason the search above is
            // bounded by it.
            lines.insert(first_table, line);
            lines.insert(first_table + 1, String::new());
        }
    }

    Ok(with_trailing_newline(lines.join("\n")))
}

/// Set or withdraw the cross-account consent key, in the text of the file.
///
/// A top-level bare key with the same placement hazard as the shared `effort`:
/// written below a table header it becomes that table's key, which parses,
/// which nothing reads, and which leaves the daemon refusing a mapping the
/// operator explicitly permitted. Withdrawing comments the line out rather
/// than deleting it, so the warning above it still has something to warn about.
pub fn set_cross_account_tiers(document: &str, enabled: bool) -> Result<String, ProxyError> {
    let mut lines: Vec<String> = document.lines().map(str::to_owned).collect();

    let first_table = lines
        .iter()
        .position(|line| line.trim_start().starts_with('['))
        .unwrap_or(lines.len());

    // A commented-out key counts — the shipped file ships it that way, and a
    // second live line would leave the commented one looking like the setting.
    let existing = lines
        .get(..first_table)
        .unwrap_or_default()
        .iter()
        .position(|line| {
            let bare = line.trim_start().trim_start_matches('#').trim_start();
            is_assignment_to(bare, "cross_account_tiers")
        });

    let line = if enabled {
        "cross_account_tiers = true".to_owned()
    } else {
        "# cross_account_tiers = true".to_owned()
    };

    match existing {
        Some(index) => {
            if let Some(existing) = lines.get_mut(index) {
                *existing = line;
            }
        }
        None => {
            lines.insert(first_table, line);
            lines.insert(first_table + 1, String::new());
        }
    }

    Ok(with_trailing_newline(lines.join("\n")))
}

/// Whether a line assigns to this key, ignoring how it is spaced.
fn is_assignment_to(line: &str, key: &str) -> bool {
    let trimmed = line.trim_start();
    let Some(rest) = trimmed.strip_prefix(key) else {
        return false;
    };
    rest.trim_start().starts_with('=')
}

/// The spacing between a key and its `=`, so an aligned table stays aligned.
fn alignment(line: &str, key: &str) -> String {
    line.trim_start()
        .strip_prefix(key)
        .map(|rest| {
            let spaces = rest.len() - rest.trim_start().len();
            " ".repeat(spaces.max(1))
        })
        .unwrap_or_else(|| " ".to_owned())
}

fn with_trailing_newline(mut text: String) -> String {
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text
}

/// Move every table belonging to one account to another name.
///
/// `None` where the file has none, so a rename that has nothing to move does
/// not write the file at all — and does not create one out of the shipped
/// example for an account that never had a section.
///
/// Only the header lines change. The body of each table, and every comment in
/// and around it, survives byte for byte: what an operator wrote about why a
/// tier is what it is stays true after the account it belongs to is renamed.
pub fn rename_account(document: &str, from: &str, to: &str) -> Result<Option<String>, ProxyError> {
    let mut lines: Vec<String> = document.lines().map(str::to_owned).collect();

    // A section under the destination name blocks this. Forgetting an account
    // leaves its section behind, so the name can be free in the store and taken
    // in the file — and moving onto it would define one table twice, which TOML
    // refuses. The daemon would then fail to start on a file the operator never
    // edited, with a parse error as the only clue.
    if let Some(occupied) = lines.iter().find(|line| account_header(line, to).is_some()) {
        return Err(ProxyError::invalid_request(format!(
            "the configuration file already has `{}` for `{to}`, left behind by an \
             account of that name. Remove it or merge it into `[accounts.{}]` before \
             renaming.",
            occupied.trim(),
            key(from)
        )));
    }

    let mut moved = false;
    for line in &mut lines {
        let Some(rest) = account_header(line, from) else {
            continue;
        };
        *line = format!("[accounts.{}{rest}", key(to));
        moved = true;
    }

    Ok(moved.then(|| with_trailing_newline(lines.join("\n"))))
}

/// The remainder of a header line that opens a table belonging to this
/// account — `]` for the account's own table, `.tiers]` for one below it.
///
/// Both spellings of the name are recognized, because TOML allows a bare key
/// and a quoted one for the same table and an operator may have written either.
fn account_header(line: &str, account: &str) -> Option<String> {
    let trimmed = line.trim();
    let rest = trimmed.strip_prefix("[accounts.")?;
    let rest = rest
        .strip_prefix(&format!("\"{account}\""))
        .or_else(|| rest.strip_prefix(account))?;
    (rest == "]" || rest.starts_with('.')).then(|| rest.to_owned())
}

/// A name as it has to be written in a table header: bare where TOML allows a
/// bare key, quoted otherwise.
fn key(name: &str) -> String {
    if !name.is_empty()
        && name.chars().all(|character| {
            character.is_ascii_alphanumeric() || character == '_' || character == '-'
        })
    {
        return name.to_owned();
    }
    format!("\"{}\"", name.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Set or comment out one key inside a named table, creating the table at the
/// end of the file if it has none.
///
/// Appending a table header is safe in a way appending a bare key never is: a
/// header ends whatever table preceded it.
fn set_in_table(
    document: &str,
    account: &str,
    name: &str,
    value: Option<&str>,
) -> Result<String, ProxyError> {
    let header = format!("[accounts.{}]", key(account));
    let mut lines: Vec<String> = document.lines().map(str::to_owned).collect();
    let line = match value {
        Some(value) => format!("{name} = \"{value}\""),
        None => format!("# {name} = \"low\""),
    };

    let Some(start) = lines
        .iter()
        .position(|line| opens_table(line, Some(account), ""))
    else {
        let mut written = document.trim_end().to_owned();
        written.push_str(&format!("\n\n{header}\n{line}\n"));
        return Ok(written);
    };

    let end = lines
        .iter()
        .skip(start + 1)
        .position(|line| line.trim_start().starts_with('['))
        .map_or(lines.len(), |offset| start + 1 + offset);

    let body = lines.get(start + 1..end).unwrap_or_default();
    // A commented-out key counts, so a second live line cannot end up below one
    // that still looks like the setting.
    let existing = body
        .iter()
        .position(|line| {
            let bare = line.trim_start().trim_start_matches('#').trim_start();
            is_assignment_to(bare, name)
        })
        .map(|offset| start + 1 + offset);

    match existing {
        Some(index) => {
            if let Some(existing) = lines.get_mut(index) {
                *existing = line;
            }
        }
        None => {
            let insert = body
                .iter()
                .rposition(|line| !line.trim().is_empty())
                .map_or(start + 1, |offset| start + 1 + offset + 1);
            lines.insert(insert, line);
        }
    }

    Ok(with_trailing_newline(lines.join("\n")))
}

/// Whether this line opens the table this edit belongs to.
///
/// `suffix` is what follows the account name: `""` for the account's own table,
/// `".tiers"` for the one below it. With no account it is the shared table of
/// that name.
///
/// Both spellings of an account name are recognized, because TOML allows a bare
/// key and a quoted one for the same table. Matching only the bare one meant an
/// operator who wrote `[accounts."spare"]` got a second table appended, and a
/// table defined twice is a file that no longer parses.
fn opens_table(line: &str, account: Option<&str>, suffix: &str) -> bool {
    let trimmed = line.trim();
    let Some(account) = account else {
        return trimmed == format!("[{}]", suffix.trim_start_matches('.'));
    };
    let Some(rest) = trimmed.strip_prefix("[accounts.") else {
        return false;
    };
    let Some(rest) = rest
        .strip_prefix(&format!("\"{account}\""))
        .or_else(|| rest.strip_prefix(account))
    else {
        return false;
    };
    rest == format!("{suffix}]")
}

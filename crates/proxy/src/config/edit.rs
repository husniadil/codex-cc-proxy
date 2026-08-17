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
pub fn set_tier(document: &str, tier: &str, model: &str) -> Result<String, ProxyError> {
    if !crate::config::TIER_NAMES.contains(&tier) {
        return Err(ProxyError::invalid_request(format!(
            "unknown tier `{tier}`; expected one of: {}",
            crate::config::TIER_NAMES.join(", ")
        )));
    }

    let mut lines: Vec<String> = document.lines().map(str::to_owned).collect();

    let table = lines.iter().position(|line| line.trim() == "[tiers]");

    let Some(start) = table else {
        // No table at all. Appending one at the end is safe in a way appending
        // a bare key never is: a table header ends whatever table preceded it.
        let mut written = document.trim_end().to_owned();
        written.push_str(&format!("\n\n[tiers]\n{tier} = \"{model}\"\n"));
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
                *line = format!("{tier}{padding}= \"{model}\"");
            }
        }
        None => {
            // Inserted at the end of *this* table, after its last non-blank
            // line, so it cannot drift under the next header.
            let insert = body
                .iter()
                .rposition(|line| !line.trim().is_empty())
                .map_or(start + 1, |offset| start + 1 + offset + 1);
            lines.insert(insert, format!("{tier} = \"{model}\""));
        }
    }

    Ok(with_trailing_newline(lines.join("\n")))
}

/// Set or remove the effort ceiling, in the text of the file.
///
/// `None` comments the key out rather than deleting the line: the explanation
/// above it is worth keeping, and a commented key still shows what the setting
/// is called.
pub fn set_effort(document: &str, effort: Option<&str>) -> Result<String, ProxyError> {
    if let Some(effort) = effort {
        // Rejected here rather than written and refused at the next startup,
        // which would leave a daemon that will not start and a file that looks
        // like someone meant it.
        crate::config::parse_effort(effort)?;
    }

    let mut lines: Vec<String> = document.lines().map(str::to_owned).collect();

    // A commented-out key counts: the shipped file ships it that way, and
    // writing a second line would leave the commented one looking like the
    // setting while a live one below it actually decides.
    let existing = lines.iter().position(|line| {
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
            // Above every table header, because a bare key written below one
            // belongs to it. Inserting before the first header is the only
            // placement that is right whatever the file already contains.
            let insert = lines
                .iter()
                .position(|line| line.trim_start().starts_with('['))
                .unwrap_or(lines.len());
            lines.insert(insert, line);
            lines.insert(insert + 1, String::new());
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

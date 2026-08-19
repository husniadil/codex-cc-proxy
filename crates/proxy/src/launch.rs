//! `docs/api.md` §2.3 — starting a client with this proxy's configuration.
//!
//! The environment half is set on the child. The policy half has no environment
//! variable at all (`proxy-behavior.md` §7.3), so it rides on the client's own
//! settings flag, and that flag is the one place this can collide with what the
//! caller typed.

use crate::error::ProxyError;

/// The flag the client reads an extra settings document from.
const SETTINGS_FLAG: &str = "--settings";

/// What to run, once the caller's arguments and this proxy's policy have been
/// put together.
#[derive(Debug, PartialEq, Eq)]
pub struct Launch {
    pub program: String,
    pub arguments: Vec<String>,
}

/// Whether this program reads the client's settings flag.
///
/// A table of one. The payload has always been client-shaped — `env` emits
/// `ANTHROPIC_DEFAULT_*_MODEL` and `CLAUDE_CODE_*` and nothing else would want
/// them — so recognizing the client it serves is not a new coupling. The naming
/// rule applies to the verb, which says what it does rather than what it
/// launches, and that is what keeps a second client a row rather than a
/// rewrite.
///
/// Everything else gets the environment and no injected flag. Splicing a flag
/// into a program that does not take one would break it for no gain.
fn accepts_settings_flag(program: &str) -> bool {
    let name = std::path::Path::new(program)
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or(program);
    matches!(name, "claude" | "claude.exe")
}

/// Whether an argument list already carries the settings flag, in either form.
fn carries_settings_flag(arguments: &[String]) -> bool {
    arguments.iter().any(|argument| {
        argument == SETTINGS_FLAG || argument.starts_with(&format!("{SETTINGS_FLAG}="))
    })
}

/// Put the caller's command together with the policy this daemon published.
///
/// **A settings flag from the caller is refused rather than merged or
/// overridden.** Measured: given two `--settings` on one argument list the
/// client keeps the last and drops the first, with exit 0 and an empty stderr.
/// So placing this proxy's document first loses the policy the moment a caller
/// brings their own, and placing it last loses theirs. Both are silent, and a
/// silent loss of a permission rule is the failure this proxy exists to avoid.
/// Refusing is the only outcome the caller can see.
///
/// The check is a token match, not a parse: this forwards the client's grammar
/// rather than interpreting it, and detecting one flag needs no more than that.
pub fn plan(command: &[String], policy: Option<&str>) -> Result<Launch, ProxyError> {
    let Some((program, forwarded)) = command.split_first() else {
        return Err(ProxyError::invalid_request(
            "no command to run; name the program to start, as in `codex-cc-proxy exec claude`",
        ));
    };

    // Only a program that would actually be given the flag can collide with it.
    // Another program's own `--settings` means nothing here and is forwarded
    // like any other argument.
    let injecting = policy.is_some() && accepts_settings_flag(program);

    if injecting && carries_settings_flag(forwarded) {
        return Err(ProxyError::invalid_request(format!(
            "`{program}` was given {SETTINGS_FLAG} and this proxy needs the same flag to deliver \
             its client policy. The client keeps only the last one, so one of the two would be \
             dropped without a word. Merge them into a single document — `codex-cc-proxy \
             settings` prints this proxy's half — or switch the policy off in `[client]`."
        )));
    }

    let mut arguments = Vec::with_capacity(forwarded.len() + 2);
    if injecting && let Some(document) = policy {
        // First, so the caller's own flags read in the order they typed them.
        arguments.push(SETTINGS_FLAG.to_owned());
        arguments.push(document.to_owned());
    }
    arguments.extend(forwarded.iter().cloned());

    Ok(Launch {
        program: program.clone(),
        arguments,
    })
}

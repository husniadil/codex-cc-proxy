//! The guided front door for storing a subscription setup token.
//!
//! Nothing here is a new credential kind: what it produces is the same keyed
//! account `login --key` produces, filed by the same store. What it adds is the
//! part a person needs and a pipe does not — where the token comes from, an
//! entry that does not echo it, and a refusal before a mistyped credential is
//! written under a name that will later be spent against the wrong endpoint.
//!
//! The decisions live in `run`, which takes its I/O as a trait. A prompt is
//! bound to a terminal and a terminal is not available to a test; the rules
//! about what a token must look like are not, and are covered without one.

use std::io;
use std::io::IsTerminal;
use std::io::Write;

use crate::auth::store::AccountStore;
use crate::auth::store::Provider;

/// What `claude setup-token` mints. An API key carries a different prefix and
/// is a different credential, spent against a different endpoint.
///
/// The stem, not a whole prefix. A real minted token begins `sk-ant-oat01-`,
/// and the version digit belongs to the issuer: guarding on one value refused
/// the credential this flow exists to store. What the guard is for is telling a
/// setup token apart from an API key, and the stem does that.
pub const SETUP_TOKEN_PREFIX: &str = "sk-ant-oat";

/// Why a guided login refused before anything was stored.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SetupTokenError {
    #[error(
        "no token was entered. Run `claude setup-token` in another terminal and paste what it prints"
    )]
    EmptyToken,
    /// Refused rather than stored. A credential of the wrong kind authenticates
    /// nowhere, and the failure it produces later names the account rather than
    /// the paste that was wrong.
    #[error(
        "that does not look like a setup token: it must begin with `{SETUP_TOKEN_PREFIX}`. \
         `claude setup-token` prints one. An API key is a different credential — store it with \
         `proxenos login --key --provider anthropic --as NAME`"
    )]
    NotASetupToken,
    #[error(
        "name the account with `--as NAME`: a token carries no id to be named by, \
         and the name is what `accounts --use` takes"
    )]
    MissingName,
    #[error("an account name cannot be empty")]
    EmptyName,
}

/// A token, as it will be stored.
///
/// Trimmed, because a paste carries a trailing newline and the store would file
/// it verbatim.
pub fn validate_token(raw: &str) -> Result<&str, SetupTokenError> {
    let token = raw.trim();
    if token.is_empty() {
        return Err(SetupTokenError::EmptyToken);
    }
    if !token.starts_with(SETUP_TOKEN_PREFIX) {
        return Err(SetupTokenError::NotASetupToken);
    }
    Ok(token)
}

/// The name the account is filed and selected under.
pub fn validate_name(raw: &str) -> Result<&str, SetupTokenError> {
    let name = raw.trim();
    if name.is_empty() {
        return Err(SetupTokenError::EmptyName);
    }
    Ok(name)
}

/// The I/O half: everything the flow says and everything it reads.
pub trait Guide {
    /// Where the token comes from, said before it is asked for.
    fn explain(&mut self) -> io::Result<()>;
    /// Read the token. A terminal implementation must not echo it.
    fn token(&mut self) -> io::Result<String>;
    /// What to call the account, or `None` when there is nobody to ask.
    fn name(&mut self) -> io::Result<Option<String>>;
    /// Confirm what was stored. Never the token.
    fn stored(&mut self, name: &str) -> io::Result<()>;
}

/// Collect a setup token and store it as a named Anthropic account.
///
/// Returns the name it was filed under, which is the string `accounts --use`
/// takes.
pub fn run(
    store: &dyn AccountStore,
    guide: &mut dyn Guide,
    label: Option<&str>,
) -> anyhow::Result<String> {
    guide.explain()?;

    // The token is checked before a name is asked for: a refusal is worth
    // reaching quickly, and there is nothing to name if there is no token.
    let raw = guide.token()?;
    let token = validate_token(&raw)?;

    let named = match label {
        Some(label) => label.to_owned(),
        None => guide.name()?.ok_or(SetupTokenError::MissingName)?,
    };
    let name = validate_name(&named)?;

    store.add_key(name, token, Provider::Anthropic)?;
    guide.stored(name)?;
    Ok(name.to_owned())
}

/// The terminal implementation: hidden entry where there is a tty, plain stdin
/// where there is not.
///
/// The non-tty path is what keeps `proxenos login --setup-token --as NAME <
/// token` working, so nothing scripted regresses.
pub struct Terminal;

impl Terminal {
    fn interactive() -> bool {
        io::stdin().is_terminal()
    }
}

impl Guide for Terminal {
    fn explain(&mut self) -> io::Result<()> {
        if !Self::interactive() {
            return Ok(());
        }
        println!(
            "This stores a Claude subscription token.\n\n\
             1. In another terminal, run:  claude setup-token\n\
             2. Copy the token it prints (it begins with {SETUP_TOKEN_PREFIX}).\n\
             3. Paste it below. Nothing is echoed, and it is never written anywhere\n   \
                but the credential file.\n"
        );
        Ok(())
    }

    fn token(&mut self) -> io::Result<String> {
        if Self::interactive() {
            rpassword::prompt_password("Token: ")
        } else {
            let mut token = String::new();
            io::Read::read_to_string(&mut io::stdin(), &mut token)?;
            Ok(token)
        }
    }

    fn name(&mut self) -> io::Result<Option<String>> {
        if !Self::interactive() {
            return Ok(None);
        }
        print!("Name this account: ");
        io::stdout().flush()?;
        let mut name = String::new();
        io::stdin().read_line(&mut name)?;
        let name = name.trim();
        if name.is_empty() {
            return Ok(None);
        }
        Ok(Some(name.to_owned()))
    }

    fn stored(&mut self, name: &str) -> io::Result<()> {
        println!("Stored an anthropic key as {name}. It serves turns from now on.");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::store::FileStore;
    use std::sync::Arc;

    /// A guide with no terminal behind it: the flow's decisions, without its
    /// I/O.
    struct Scripted {
        token: String,
        name: Option<String>,
        stored_as: Option<String>,
    }

    impl Scripted {
        fn new(token: &str, name: Option<&str>) -> Self {
            Self {
                token: token.to_owned(),
                name: name.map(str::to_owned),
                stored_as: None,
            }
        }
    }

    impl Guide for Scripted {
        fn explain(&mut self) -> io::Result<()> {
            Ok(())
        }
        fn token(&mut self) -> io::Result<String> {
            Ok(self.token.clone())
        }
        fn name(&mut self) -> io::Result<Option<String>> {
            Ok(self.name.clone())
        }
        fn stored(&mut self, name: &str) -> io::Result<()> {
            self.stored_as = Some(name.to_owned());
            Ok(())
        }
    }

    fn temp_store() -> (tempfile::TempDir, Arc<FileStore>) {
        let home = tempfile::tempdir().expect("a temp home");
        let store = Arc::new(FileStore::new(home.path().join("credentials.json")));
        (home, store)
    }

    /// The whole reason the guided flow validates at all: a credential of the
    /// wrong kind stores cleanly and fails later, naming the account rather
    /// than the paste.
    #[test]
    fn a_token_without_the_setup_token_prefix_is_refused() {
        let error = validate_token("sk-ant-api03-notasetuptoken").unwrap_err();
        assert_eq!(error, SetupTokenError::NotASetupToken);

        let (_home, store) = temp_store();
        let mut guide = Scripted::new("sk-ant-api03-notasetuptoken", Some("sub"));
        let refused = run(store.as_ref(), &mut guide, None).unwrap_err();
        assert!(
            refused.to_string().contains(SETUP_TOKEN_PREFIX),
            "the refusal names the prefix it wanted: {refused}"
        );
        assert!(
            guide.stored_as.is_none(),
            "nothing is stored when the token is refused"
        );
        assert!(
            store.accounts().expect("a readable store").is_empty(),
            "the store is left untouched"
        );
    }

    /// The prefix a real minted token actually carries. Measured against a
    /// live one rather than transcribed from prose: the token in use here
    /// begins `sk-ant-oat01-`, and a guard written for `sk-ant-oat1-` refused
    /// the very credential this flow exists to store.
    #[test]
    fn the_prefix_a_real_minted_token_carries_is_accepted() {
        assert_eq!(
            validate_token("sk-ant-oat01-realish-token").expect("a setup token"),
            "sk-ant-oat01-realish-token"
        );
    }

    #[test]
    fn an_empty_paste_is_refused_before_anything_is_stored() {
        assert_eq!(
            validate_token("   \n").unwrap_err(),
            SetupTokenError::EmptyToken
        );
    }

    #[test]
    fn a_pasted_token_is_trimmed_of_the_newline_it_arrives_with() {
        assert_eq!(
            validate_token("sk-ant-oat1-abc\n").expect("a setup token"),
            "sk-ant-oat1-abc"
        );
    }

    /// The scripted path — no tty, a name supplied by `--as` — still stores a
    /// key, which is what keeps machine use from regressing.
    #[test]
    fn the_non_tty_path_stores_a_key_under_the_supplied_name() {
        let (_home, store) = temp_store();
        let mut guide = Scripted::new("sk-ant-oat1-realish-token\n", None);

        let name = run(store.as_ref(), &mut guide, Some("sub")).expect("the token is stored");

        assert_eq!(name, "sub");
        assert_eq!(guide.stored_as.as_deref(), Some("sub"));
        let accounts = store.accounts().expect("a readable store");
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].name, "sub");
        assert_eq!(accounts[0].provider, Provider::Anthropic.as_str());
    }

    /// Nothing to ask and nothing supplied: refused, rather than filed under a
    /// name nobody chose.
    #[test]
    fn a_flow_with_no_name_and_nobody_to_ask_is_refused() {
        let (_home, store) = temp_store();
        let mut guide = Scripted::new("sk-ant-oat1-realish-token", None);
        let refused = run(store.as_ref(), &mut guide, None).unwrap_err();
        assert!(refused.to_string().contains("--as NAME"), "{refused}");
        assert!(store.accounts().expect("a readable store").is_empty());
    }

    #[test]
    fn a_name_answered_at_the_prompt_is_used_when_no_label_was_given() {
        let (_home, store) = temp_store();
        let mut guide = Scripted::new("sk-ant-oat1-realish-token", Some(" typed \n"));
        let name = run(store.as_ref(), &mut guide, None).expect("the token is stored");
        assert_eq!(name, "typed");
    }
}

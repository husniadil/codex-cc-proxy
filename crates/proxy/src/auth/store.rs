//! `docs/proxy-behavior.md` §8 — where credentials live.
//!
//! Behind a trait, so a platform keychain satisfies the same contract as the
//! default file. Credentials never appear in process arguments, logs, or the
//! configuration file.

use crate::error::ProxyError;
use serde::Deserialize;
use serde::Serialize;
use std::path::Path;
use std::path::PathBuf;

/// One grant. `Debug` is implemented by hand: the derived one would print the
/// tokens, and a `Debug` line in a log is exactly the leak §8 forbids.
#[derive(Clone, Deserialize, Serialize)]
pub struct Credentials {
    pub access_token: String,
    pub refresh_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    /// Unix seconds. Absolute rather than a duration, because a duration is
    /// only meaningful next to the instant it was issued.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("access_token", &Redacted)
            .field("refresh_token", &Redacted)
            .field("id_token", &self.id_token.as_ref().map(|_| Redacted))
            .field("account_id", &self.account_id)
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

struct Redacted;

impl std::fmt::Debug for Redacted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}

impl Credentials {
    /// Whether the access token is at or past the point where it should be
    /// replaced.
    ///
    /// Refresh begins ahead of expiry (§8): a token that expires during a
    /// request fails the request, and the margin is what stops that being
    /// routine.
    pub fn needs_refresh(&self, now: u64, margin_seconds: u64) -> bool {
        match self.expires_at {
            Some(expires_at) => now.saturating_add(margin_seconds) >= expires_at,
            // An unknown expiry is treated as expired. Refreshing needlessly
            // costs one request; using a dead token fails the turn.
            None => true,
        }
    }
}

/// One account as it is reported, never as it is stored.
///
/// This is the shape that leaves the process — `status` renders it — so it
/// carries what tells two accounts apart and nothing that would authenticate
/// as either. There is no token in it, and there must never be one.
#[derive(Clone, Debug, Serialize)]
pub struct Account {
    /// What this store calls the account: an operator's label, else the id the
    /// backend knows it by, else an assigned name.
    pub name: String,
    /// The id the backend knows it by, where the grant carried one.
    pub account_id: Option<String>,
    /// Read from the stored id token, so two accounts are distinguishable by
    /// something a person recognizes.
    pub email: Option<String>,
    /// The plan as of the last login, which is the only thing a stored grant
    /// can say about it. The backend's own figure outranks it wherever a turn
    /// has been made.
    pub plan: Option<String>,
    pub expires_at: Option<u64>,
    /// Whether this is the account serving turns.
    pub selected: bool,
}

/// A store that holds more than one grant.
///
/// `CredentialStore` is about *the* grant — the one serving turns — and every
/// caller that only needs to authenticate a request stays on it. This is the
/// second half: which grants exist, and which of them is that one.
pub trait AccountStore: CredentialStore {
    /// Every stored account, in the order they were added.
    fn accounts(&self) -> Result<Vec<Account>, ProxyError>;

    /// Store a grant as an account and select it, returning the name it got.
    ///
    /// This is what a login does. `save` writes the grant of the account
    /// already selected — a refresh — and the two are deliberately different
    /// verbs: a login that overwrote whichever account happened to be selected
    /// would silently retire a working grant.
    fn add(&self, credentials: &Credentials, label: Option<&str>) -> Result<String, ProxyError>;

    /// Choose the account that serves turns from now on.
    fn select(&self, name: &str) -> Result<(), ProxyError>;

    /// Forget one account, leaving the rest usable.
    fn remove(&self, name: &str) -> Result<(), ProxyError>;
}

/// The credential file: several accounts, one of them selected.
///
/// A file written before this store held more than one is a bare grant, and is
/// read as the single account it describes. Refusing it would cost a re-login
/// for a grant that is present and still valid.
#[derive(Debug, Default, Deserialize, Serialize)]
struct StoredFile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    selected: Option<String>,
    #[serde(default)]
    accounts: Vec<Entry>,
}

#[derive(Debug, Deserialize, Serialize)]
struct Entry {
    name: String,
    #[serde(flatten)]
    grant: Credentials,
}

impl StoredFile {
    /// Which account serves turns.
    ///
    /// A selection naming an account that is not stored falls back to the
    /// first one. The file still holds usable grants, and answering "not
    /// authenticated" there sends an operator to re-login for nothing.
    fn selected_index(&self) -> Option<usize> {
        if self.accounts.is_empty() {
            return None;
        }
        self.selected
            .as_deref()
            .and_then(|name| self.accounts.iter().position(|entry| entry.name == name))
            .or(Some(0))
    }

    fn index_of(&self, name: &str) -> Option<usize> {
        self.accounts.iter().position(|entry| entry.name == name)
    }

    /// Which entry belongs to this account, by the id the backend knows it by.
    ///
    /// Identity, as distinct from the name it is filed under. An account
    /// authorized again under a different label is the same account, and
    /// storing it twice would leave two entries holding one refresh-token
    /// family — the arrangement §8.1 exists to keep out of the store.
    fn index_by_account(&self, account_id: Option<&str>) -> Option<usize> {
        let account_id = account_id?;
        self.accounts
            .iter()
            .position(|entry| entry.grant.account_id.as_deref() == Some(account_id))
    }

    /// How a refusal describes what is here. With nothing stored, what the
    /// reader needs is not an empty list.
    fn unknown(&self, name: &str) -> ProxyError {
        if self.accounts.is_empty() {
            return ProxyError::invalid_request(format!(
                "no account named `{name}`; none are stored — run `login`"
            ));
        }
        let stored = self
            .accounts
            .iter()
            .map(|entry| entry.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        ProxyError::invalid_request(format!("no account named `{name}`; stored: {stored}"))
    }

    /// The name a grant gets when nothing else names it.
    ///
    /// The account id where there is one. Where there is not, an assigned name
    /// rather than anything derived from the grant: nothing inside it is an
    /// account id, and treating a token as one would be a fabricated fact
    /// about an account.
    fn name_for(&self, credentials: &Credentials) -> String {
        if let Some(account_id) = credentials.account_id.as_deref() {
            return account_id.to_owned();
        }
        (1..)
            .map(|n| format!("account-{n}"))
            .find(|name| self.index_of(name).is_none())
            .unwrap_or_else(|| "account".to_owned())
    }

    /// Drop one account by position, leaving something selected behind it.
    fn remove_at(&mut self, index: usize) {
        if index >= self.accounts.len() {
            return;
        }
        let removed = self.accounts.remove(index);
        if self.selected.as_deref() == Some(removed.name.as_str()) {
            self.selected = self.accounts.first().map(|entry| entry.name.clone());
        }
    }

    /// Put a grant under a name, replacing whatever was there.
    fn put(&mut self, name: String, credentials: &Credentials) {
        match self
            .index_by_account(credentials.account_id.as_deref())
            .or_else(|| self.index_of(&name))
            .and_then(|index| self.accounts.get_mut(index))
        {
            Some(entry) => {
                // A label renames the account it was given for; it never
                // creates a second entry for one already stored.
                entry.name = name.clone();
                entry.grant = credentials.clone();
            }
            None => self.accounts.push(Entry {
                name: name.clone(),
                grant: credentials.clone(),
            }),
        }
        self.selected = Some(name);
    }
}

pub trait CredentialStore: Send + Sync {
    fn load(&self) -> Result<Option<Credentials>, ProxyError>;
    fn save(&self, credentials: &Credentials) -> Result<(), ProxyError>;
    fn clear(&self) -> Result<(), ProxyError>;
}

/// The default implementation: one JSON file, created `0600`.
///
/// The file holds every account and the name of the one serving turns. A file
/// written before it held more than one is a bare grant, and migrates on the
/// next write rather than on read: reading credentials is not a reason to
/// rewrite them.
pub struct FileStore {
    path: PathBuf,
    /// Fired between a write's read and its replacement, so a test can make
    /// the file change underneath one. Nothing outside a test sets it.
    #[allow(clippy::type_complexity)]
    on_write: std::sync::Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
}

/// How many times a write will start over when it finds the file changed.
///
/// Each attempt is a read, a change and a replacement, with nothing slow in
/// between: losing five in a row is not contention, it is something writing the
/// file in a loop, and answering that with an error beats spinning.
const WRITE_ATTEMPTS: usize = 5;

impl FileStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            on_write: std::sync::Mutex::new(None),
        }
    }

    /// Test seam: run this between reading the file and replacing it.
    pub fn on_write_for_test(&self, hook: impl Fn() + Send + Sync + 'static) {
        if let Ok(mut on_write) = self.on_write.lock() {
            *on_write = Some(Box::new(hook));
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn read(&self) -> Result<StoredFile, ProxyError> {
        Ok(self.read_raw()?.1)
    }

    /// The file as it is on disk, and as this store understands it.
    ///
    /// The bytes come back too: a write compares them against what is there
    /// when it lands, and starting over is what keeps two writers from
    /// discarding each other's accounts.
    fn read_raw(&self) -> Result<(Option<String>, StoredFile), ProxyError> {
        let raw = match std::fs::read_to_string(&self.path) {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok((None, StoredFile::default()));
            }
            Err(error) => {
                return Err(ProxyError::authentication(format!(
                    "could not read credentials: {error}"
                )));
            }
        };

        // The error names the parse failure, never the content.
        let unreadable = |error: serde_json::Error| {
            ProxyError::authentication(format!("stored credentials are unreadable: {error}"))
        };
        let value: serde_json::Value = serde_json::from_str(&raw).map_err(unreadable)?;

        // `accounts` is the key the current shape is built around, so its
        // absence is what identifies the older one. Reading a bare grant as
        // the single account it describes is what keeps an upgrade from
        // costing a re-login.
        if value.get("accounts").is_some() {
            let file = serde_json::from_value(value).map_err(unreadable)?;
            return Ok((Some(raw), file));
        }

        let grant: Credentials = serde_json::from_value(value).map_err(unreadable)?;
        let mut file = StoredFile::default();
        let name = file.name_for(&grant);
        file.put(name, &grant);
        Ok((Some(raw), file))
    }

    /// Read, change, replace — starting over if the file moved underneath.
    ///
    /// Every write here rewrites the whole file, so two overlapping writers
    /// used to mean one discarded whatever the other had just done. That is a
    /// whole account now, not one stale token, and the pair that overlaps in
    /// practice is real: `login` in the CLI writes this file directly while
    /// the daemon may be persisting a refresh.
    ///
    /// **This narrows the window rather than closing it.** The comparison and
    /// the replacement are two operations, and a writer that lands between
    /// them is still lost. Closing it needs a lock the filesystem enforces.
    fn update<T>(
        &self,
        mutate: impl Fn(&mut StoredFile) -> Result<T, ProxyError>,
    ) -> Result<T, ProxyError> {
        for _ in 0..WRITE_ATTEMPTS {
            let (raw, mut file) = self.read_raw()?;
            // Before the write, never after: an error here is the caller's
            // answer, and retrying it would only produce the same one.
            let outcome = mutate(&mut file)?;

            if let Ok(hook) = self.on_write.lock()
                && let Some(hook) = hook.as_ref()
            {
                hook();
            }

            if self.replace_if_unchanged(&file, raw.as_deref())? {
                return Ok(outcome);
            }
        }

        Err(ProxyError::authentication(
            "the credential file kept changing while it was being written; try again",
        ))
    }

    /// Replace the file, unless it is no longer what was read.
    fn replace_if_unchanged(
        &self,
        file: &StoredFile,
        expected: Option<&str>,
    ) -> Result<bool, ProxyError> {
        let current = match std::fs::read_to_string(&self.path) {
            Ok(current) => Some(current),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(ProxyError::authentication(format!(
                    "could not read credentials: {error}"
                )));
            }
        };
        if current.as_deref() != expected {
            return Ok(false);
        }

        self.write(file)?;
        Ok(true)
    }

    fn write(&self, file: &StoredFile) -> Result<(), ProxyError> {
        // Nothing left to hold. The file goes rather than staying behind as an
        // empty list, so `load` answers "not authenticated" from its absence
        // the same way it always has.
        if file.accounts.is_empty() {
            return self.remove_file();
        }

        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                ProxyError::authentication(format!(
                    "could not create credential directory: {error}"
                ))
            })?;
        }

        let body = serde_json::to_string_pretty(file).map_err(|error| {
            ProxyError::authentication(format!("could not serialize credentials: {error}"))
        })?;

        // Written beside the file and moved over it. The store holds every
        // account now, so a write interrupted partway — no space, a crash —
        // would take all of them for one account's rotated token. The
        // replacement carries the process id because two daemons writing one
        // temporary path would interleave into a file that is neither.
        //
        // Created with restrictive permissions from the outset. Writing first
        // and tightening afterwards leaves a window in which the file is
        // world-readable, and that window is enough.
        let mut pending = self.path.clone().into_os_string();
        pending.push(format!(".{}.pending", std::process::id()));
        let pending = PathBuf::from(pending);
        write_private(&pending, &body)?;
        std::fs::rename(&pending, &self.path).map_err(|error| {
            let _ = std::fs::remove_file(&pending);
            ProxyError::authentication(format!("could not replace the credential file: {error}"))
        })
    }

    fn remove_file(&self) -> Result<(), ProxyError> {
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(ProxyError::authentication(format!(
                "could not clear credentials: {error}"
            ))),
        }
    }
}

impl CredentialStore for FileStore {
    fn load(&self) -> Result<Option<Credentials>, ProxyError> {
        let file = self.read()?;
        Ok(file
            .selected_index()
            .and_then(|index| file.accounts.get(index))
            .map(|entry| entry.grant.clone()))
    }

    /// Write the selected account's grant — what a refresh does.
    ///
    /// On an empty store this creates the account, so a caller holding nothing
    /// but this trait still works. It never creates a *second* one: adding an
    /// account is `AccountStore::add`, and a refresh that appended would leave
    /// two entries sharing one refresh-token family.
    fn save(&self, credentials: &Credentials) -> Result<(), ProxyError> {
        self.update(|file| {
            // The account the grant belongs to, and only failing that the
            // selected one. A refresh is a read, a network round trip, and a
            // write; between the read and the write the selection can move,
            // and resolving the target by selection would drop one account's
            // rotated grant into another's entry — destroying a refresh token
            // only a re-login replaces, and leaving that account
            // authenticating as somebody else.
            match file
                .index_by_account(credentials.account_id.as_deref())
                .or_else(|| file.selected_index())
                .and_then(|index| file.accounts.get_mut(index))
            {
                Some(entry) => entry.grant = credentials.clone(),
                None => {
                    let name = file.name_for(credentials);
                    file.put(name, credentials);
                }
            }
            Ok(())
        })
    }

    /// Forget the account serving turns, leaving the rest usable.
    ///
    /// Clearing what is already gone is not an error: `disconnect` must be
    /// safe to run twice.
    fn clear(&self) -> Result<(), ProxyError> {
        self.update(|file| {
            if let Some(index) = file.selected_index() {
                file.remove_at(index);
            }
            Ok(())
        })
    }
}

impl AccountStore for FileStore {
    fn accounts(&self) -> Result<Vec<Account>, ProxyError> {
        let file = self.read()?;
        let selected = file.selected_index();
        Ok(file
            .accounts
            .iter()
            .enumerate()
            .map(|(index, entry)| {
                let id_token = entry.grant.id_token.as_deref();
                Account {
                    name: entry.name.clone(),
                    account_id: entry.grant.account_id.clone(),
                    email: super::jwt::email(id_token),
                    plan: super::jwt::plan(id_token),
                    expires_at: entry.grant.expires_at,
                    selected: selected == Some(index),
                }
            })
            .collect())
    }

    fn add(&self, credentials: &Credentials, label: Option<&str>) -> Result<String, ProxyError> {
        self.update(|file| {
            // A label that already names a different account. Honouring it would
            // write this grant over that one, retiring a working grant with
            // nothing said — the failure the add/save split exists to prevent.
            // Refusing costs the authorization just spent, which one more login
            // replaces; the other way costs a grant that may not be.
            if let Some(label) = label
                && let Some(entry) = file.index_of(label).and_then(|i| file.accounts.get(i))
                && entry.grant.account_id.is_some()
                && entry.grant.account_id != credentials.account_id
            {
                return Err(ProxyError::invalid_request(format!(
                    "`{label}` already names account {}; log in again with another label",
                    entry.grant.account_id.as_deref().unwrap_or("unknown")
                )));
            }

            let name = match label {
                Some(label) => label.to_owned(),
                // Already stored, under whatever it is already called: a login
                // carrying no label is not a request to rename anything.
                None => match file
                    .index_by_account(credentials.account_id.as_deref())
                    .and_then(|index| file.accounts.get(index))
                {
                    Some(entry) => entry.name.clone(),
                    None => file.name_for(credentials),
                },
            };
            file.put(name.clone(), credentials);
            Ok(name)
        })
    }

    fn select(&self, name: &str) -> Result<(), ProxyError> {
        self.update(|file| {
            if file.index_of(name).is_none() {
                return Err(file.unknown(name));
            }
            file.selected = Some(name.to_owned());
            Ok(())
        })
    }

    fn remove(&self, name: &str) -> Result<(), ProxyError> {
        self.update(|file| {
            let Some(index) = file.index_of(name) else {
                return Err(file.unknown(name));
            };
            file.remove_at(index);
            Ok(())
        })
    }
}

#[cfg(unix)]
fn write_private(path: &Path, body: &str) -> Result<(), ProxyError> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(|error| {
            ProxyError::authentication(format!("could not open credential file: {error}"))
        })?;

    file.write_all(body.as_bytes()).map_err(|error| {
        ProxyError::authentication(format!("could not write credentials: {error}"))
    })
}

#[cfg(not(unix))]
fn write_private(path: &Path, body: &str) -> Result<(), ProxyError> {
    // Windows has no mode bits. The file inherits the directory's ACL, and the
    // configuration directory is per-user.
    std::fs::write(path, body).map_err(|error| {
        ProxyError::authentication(format!("could not write credentials: {error}"))
    })
}

//! Account configuration: a serde/TOML file in the platform config dir,
//! replacing the old 3-line `esmail_config.txt`. Passwords never live here —
//! see [`crate::secrets`] for those.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// How to secure a connection to a mail server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TlsMode {
    /// Implicit TLS from the first byte (IMAPS/SMTPS, ports 993/465).
    Ssl,
    /// Plaintext connection upgraded via `STARTTLS`.
    StartTls,
    /// No transport security. Only useful for local/test servers.
    None,
}

/// One configured mail account. Passwords are looked up separately, from the
/// OS keyring, keyed by `(id, "imap" | "smtp")` — see [`crate::secrets`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AccountConfig {
    /// Stable identifier for this account, used as the keyring key and to
    /// match this entry across edits. Not shown in the UI.
    pub id: String,
    /// Human-readable label, e.g. "Work" or the email address.
    pub display_name: String,
    pub imap_host: String,
    pub imap_port: u16,
    pub imap_tls: TlsMode,
    pub smtp_host: String,
    pub smtp_port: u16,
    pub smtp_tls: TlsMode,
    /// Used for both IMAP and SMTP auth.
    pub username: String,
}

impl AccountConfig {
    /// A new account for `username`@`imap_host`, with the common IMAPS/SMTPS
    /// defaults (993/465, implicit TLS) until B7 lets the user override them.
    pub fn new(display_name: String, imap_host: String, imap_port: u16, username: String) -> Self {
        Self {
            id: format!("{username}@{imap_host}"),
            display_name,
            imap_host,
            imap_port,
            imap_tls: TlsMode::Ssl,
            smtp_host: String::new(),
            smtp_port: 465,
            smtp_tls: TlsMode::Ssl,
            username,
        }
    }
}

/// All configured accounts. Serialized as TOML to
/// `<config dir>/esmail/config.toml`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub accounts: Vec<AccountConfig>,
}

fn config_dir() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "esmail").map(|dirs| dirs.config_dir().to_path_buf())
}

fn config_path() -> Option<PathBuf> {
    config_dir().map(|dir| dir.join("config.toml"))
}

/// The old plain-text config file this format replaces, so first-run
/// migration knows where to look.
fn legacy_config_path() -> PathBuf {
    let appdata = std::env::var("APPDATA").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(appdata).join("esmail_config.txt")
}

impl Config {
    /// Load `config.toml`, or an empty config if it does not exist yet or
    /// fails to parse (rather than refusing to start).
    pub fn load() -> Self {
        let Some(path) = config_path() else {
            return Self::default();
        };
        match std::fs::read_to_string(&path) {
            Ok(content) => toml::from_str(&content).unwrap_or_else(|e| {
                log::warn!("could not parse {}: {e}; starting with no accounts", path.display());
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    /// Write `config.toml`, creating the config directory if needed.
    pub fn save(&self) -> anyhow::Result<()> {
        let path = config_path().ok_or_else(|| anyhow::anyhow!("no config directory available"))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let toml = toml::to_string_pretty(self)?;
        std::fs::write(path, toml)?;
        Ok(())
    }

    /// If there are no accounts yet, read the legacy `esmail_config.txt`
    /// (host/port/username — it never held a password) and turn it into a
    /// single account. Returns `true` if a legacy file was found and
    /// migrated, so the caller knows to persist the result.
    ///
    /// The legacy file is left in place: this only reads it, so a user who
    /// has not yet accepted this branch's config format is not surprised by
    /// a deleted file.
    pub fn migrate_legacy(&mut self) -> bool {
        let Ok(content) = std::fs::read_to_string(legacy_config_path()) else {
            return false;
        };
        self.migrate_legacy_content(&content)
    }

    /// The parsing half of [`Config::migrate_legacy`], split out so it can be
    /// tested without touching the real `%APPDATA%`.
    fn migrate_legacy_content(&mut self, content: &str) -> bool {
        if !self.accounts.is_empty() {
            return false;
        }
        let lines: Vec<&str> = content.lines().collect();
        if lines.len() < 3 {
            return false;
        }
        let host = lines[0].trim().to_string();
        let port: u16 = lines[1].trim().parse().unwrap_or(993);
        let username = lines[2].trim().to_string();
        if host.is_empty() || username.is_empty() {
            return false;
        }
        log::info!("migrating legacy config for {username}@{host} into config.toml");
        self.accounts.push(AccountConfig::new(username.clone(), host, port, username));
        true
    }

    /// Insert `account`, or replace the existing entry with the same `id`.
    pub fn upsert_account(&mut self, account: AccountConfig) {
        if let Some(existing) = self.accounts.iter_mut().find(|a| a.id == account.id) {
            *existing = account;
        } else {
            self.accounts.push(account);
        }
    }

    /// Remove the account with this `id`, if any.
    pub fn remove_account(&mut self, id: &str) {
        self.accounts.retain(|a| a.id != id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_id_combines_username_and_host_so_two_accounts_on_one_host_differ() {
        let a = AccountConfig::new("A".into(), "imap.example.com".into(), 993, "alice".into());
        let b = AccountConfig::new("B".into(), "imap.example.com".into(), 993, "bob".into());
        assert_ne!(a.id, b.id);
    }

    #[test]
    fn config_round_trips_through_toml() {
        let mut config = Config::default();
        config.accounts.push(AccountConfig::new(
            "Home".into(),
            "imap.example.com".into(),
            993,
            "alice".into(),
        ));

        let toml = toml::to_string_pretty(&config).unwrap();
        let parsed: Config = toml::from_str(&toml).unwrap();
        assert_eq!(parsed, config);
    }

    #[test]
    fn upsert_replaces_the_matching_account_rather_than_duplicating_it() {
        let mut config = Config::default();
        let mut account = AccountConfig::new("Home".into(), "imap.example.com".into(), 993, "alice".into());
        config.upsert_account(account.clone());

        account.display_name = "Home (renamed)".into();
        config.upsert_account(account.clone());

        assert_eq!(config.accounts.len(), 1);
        assert_eq!(config.accounts[0].display_name, "Home (renamed)");
    }

    #[test]
    fn upsert_appends_when_the_id_is_new() {
        let mut config = Config::default();
        config.upsert_account(AccountConfig::new("Home".into(), "a.example.com".into(), 993, "alice".into()));
        config.upsert_account(AccountConfig::new("Work".into(), "b.example.com".into(), 993, "alice".into()));
        assert_eq!(config.accounts.len(), 2);
    }

    #[test]
    fn remove_account_drops_only_the_matching_id() {
        let mut config = Config::default();
        config.upsert_account(AccountConfig::new("Home".into(), "a.example.com".into(), 993, "alice".into()));
        let keep = AccountConfig::new("Work".into(), "b.example.com".into(), 993, "alice".into());
        config.upsert_account(keep.clone());

        config.remove_account(&AccountConfig::new("Home".into(), "a.example.com".into(), 993, "alice".into()).id);

        assert_eq!(config.accounts, vec![keep]);
    }

    #[test]
    fn migrate_legacy_parses_the_three_line_format() {
        let mut config = Config::default();
        let migrated = config.migrate_legacy_content("imap.gmail.com\n993\nalice@gmail.com\n");
        assert!(migrated);
        assert_eq!(config.accounts.len(), 1);
        assert_eq!(config.accounts[0].imap_host, "imap.gmail.com");
        assert_eq!(config.accounts[0].imap_port, 993);
        assert_eq!(config.accounts[0].username, "alice@gmail.com");
    }

    #[test]
    fn migrate_legacy_does_nothing_if_accounts_already_exist() {
        // Otherwise a real config would be silently clobbered by a stale
        // esmail_config.txt left over from before this format existed.
        let mut config = Config::default();
        config.upsert_account(AccountConfig::new("Home".into(), "imap.example.com".into(), 993, "alice".into()));
        let before = config.clone();

        let migrated = config.migrate_legacy_content("imap.gmail.com\n993\nalice@gmail.com\n");

        assert!(!migrated);
        assert_eq!(config, before);
    }

    #[test]
    fn migrate_legacy_rejects_malformed_or_empty_input() {
        let mut config = Config::default();
        assert!(!config.migrate_legacy_content(""));
        assert!(!config.migrate_legacy_content("only one line"));
        assert!(!config.migrate_legacy_content("\n993\nalice")); // empty host
        assert_eq!(config.accounts.len(), 0);
    }
}

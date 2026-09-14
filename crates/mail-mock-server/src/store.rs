//! In-memory mailbox state shared between the IMAP and SMTP halves of the
//! mock server. An SMTP `DATA` delivery appends straight into the
//! recipient's mailbox here, so "send then fetch" round-trips work without
//! esmail ever issuing `APPEND` (it doesn't -- see smtp.rs's doc comment).

use std::collections::HashMap;
use std::sync::Mutex;

use tokio::sync::broadcast;

/// One stored message: the raw RFC822 bytes (source of truth, what
/// `UID FETCH ... RFC822` returns) plus the envelope fields extracted once
/// so `FETCH (UID ENVELOPE)` doesn't reparse on every request.
#[derive(Debug, Clone)]
pub struct StoredMessage {
    pub uid: u32,
    pub raw: Vec<u8>,
    pub envelope: Envelope,
}

#[derive(Debug, Clone, Default)]
pub struct Envelope {
    pub date: String,
    pub subject: String,
    /// (display name, mailbox local-part, host) -- just the first address,
    /// same simplification `imap.rs::format_address` makes on the client
    /// side, which is all these tests need.
    pub from: Option<(String, String, String)>,
    pub to: Option<(String, String, String)>,
    pub message_id: String,
}

pub struct Mailbox {
    pub name: String,
    pub messages: Vec<StoredMessage>,
    pub uid_validity: u32,
    pub uid_next: u32,
}

impl Mailbox {
    fn new(name: &str, uid_validity: u32) -> Self {
        Mailbox { name: name.to_string(), messages: Vec::new(), uid_validity, uid_next: 1 }
    }

    pub fn append(&mut self, raw: Vec<u8>, envelope: Envelope) -> u32 {
        let uid = self.uid_next;
        self.uid_next += 1;
        self.messages.push(StoredMessage { uid, raw, envelope });
        uid
    }
}

pub struct Store {
    pub users: HashMap<String, String>,
    pub mailboxes: HashMap<String, Mailbox>,
    /// Broadcasts the name of any mailbox a delivery just landed in, so an
    /// `IDLE` connection selected on that mailbox can push an untagged
    /// `EXISTS` instead of the client having to poll (see `imap_server.rs`'s
    /// `IDLE` handling). A plain `Mutex`-guarded field rather than something
    /// fancier: `broadcast` already handles the "zero or many idling
    /// connections care about this" fan-out, and `send` on no subscribers is
    /// just a no-op `Err` every `deliver` call already ignores.
    pub notify: broadcast::Sender<String>,
}

impl Store {
    pub fn new() -> Self {
        let (notify, _) = broadcast::channel(32);
        Store { users: HashMap::new(), mailboxes: HashMap::new(), notify }
    }

    pub fn add_user(&mut self, username: &str, password: &str) {
        self.users.insert(username.to_string(), password.to_string());
        // A fresh account gets the usual special-use mailboxes so LIST has
        // something realistic to return and SMTP delivery always has an
        // INBOX to land in.
        for name in ["INBOX", "Sent", "Drafts", "Trash"] {
            self.mailboxes.entry(name.to_string()).or_insert_with(|| Mailbox::new(name, 1));
        }
    }

    pub fn check_login(&self, username: &str, password: &str) -> bool {
        self.users.get(username).is_some_and(|p| p == password)
    }

    pub fn mailbox_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.mailboxes.keys().cloned().collect();
        names.sort();
        names
    }

    pub fn mailbox(&self, name: &str) -> Option<&Mailbox> {
        self.mailboxes.get(name)
    }

    pub fn mailbox_mut(&mut self, name: &str) -> Option<&mut Mailbox> {
        self.mailboxes.get_mut(name)
    }

    pub fn deliver(&mut self, recipient_mailbox: &str, raw: Vec<u8>) -> u32 {
        let envelope = parse_envelope(&raw);
        let mailbox = self
            .mailboxes
            .entry(recipient_mailbox.to_string())
            .or_insert_with(|| Mailbox::new(recipient_mailbox, 1));
        let uid = mailbox.append(raw, envelope);
        let _ = self.notify.send(recipient_mailbox.to_string());
        uid
    }
}

/// Extracts the handful of headers `FETCH ENVELOPE` needs from a raw
/// message, for mail delivered live over SMTP (fixture messages build their
/// `Envelope` directly instead -- see fixtures.rs).
pub fn parse_envelope(raw: &[u8]) -> Envelope {
    let parsed = match mailparse::parse_mail(raw) {
        Ok(p) => p,
        Err(_) => return Envelope::default(),
    };
    let header = |name: &str| -> String {
        parsed.headers.iter().find(|h| h.get_key_ref().eq_ignore_ascii_case(name)).map(|h| h.get_value()).unwrap_or_default()
    };
    let address = |name: &str| -> Option<(String, String, String)> {
        let raw = header(name);
        if raw.is_empty() {
            return None;
        }
        let addrs = mailparse::addrparse(&raw).ok()?;
        match addrs.first()? {
            mailparse::MailAddr::Single(info) => {
                let (mailbox, host) = info.addr.split_once('@').unwrap_or((info.addr.as_str(), ""));
                Some((info.display_name.clone().unwrap_or_default(), mailbox.to_string(), host.to_string()))
            }
            mailparse::MailAddr::Group(_) => None,
        }
    };

    Envelope {
        date: header("Date"),
        subject: header("Subject"),
        from: address("From"),
        to: address("To"),
        message_id: header("Message-ID"),
    }
}

pub type SharedStore = std::sync::Arc<Mutex<Store>>;

//! SMTP sending (B7 of PLAN.md), following the same actor pattern as
//! `imap.rs`/`db.rs`: a tokio task behind an mpsc channel, so sending never
//! blocks the UI thread.
//!
//! **Not done:** persisted retry queue. A failed send reports
//! [`SmtpEvent::Error`] and the compose window stays open with the typed
//! text intact so the user can just click Send again; there is no background
//! retry-with-backoff and nothing survives an app restart. PLAN.md's B7 asks
//! for "queue sends so a failure retries rather than losing the message" —
//! the "don't lose the message" half is covered (nothing is cleared on
//! failure), the automatic-retry half is not.
//!
//! **Also not done:** IMAP `APPEND` to Sent/Drafts. A sent message reaches
//! the recipient but is never saved to the account's Sent folder locally,
//! and there is no draft autosave. Both are IMAP-protocol-facing work
//! (folder discovery via the `\Sent`/`\Drafts` special-use flags, or a
//! name-based fallback, then `APPEND`) layered on top of an already large
//! feature; deferred for the same reason B2/B3/B4/B6's own live-IMAP halves
//! were — no real or mock IMAP server here to verify it against.

use lettre::message::{Attachment, Message, MultiPart, SinglePart, header::ContentType};
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Tokio1Executor};
use secrecy::{ExposeSecret, SecretString};
use tokio::sync::mpsc;

use crate::compose::ComposeState;
use crate::config::TlsMode;

/// Everything sending needs that isn't in the [`ComposeState`] itself.
pub struct SmtpAccount {
    pub host: String,
    pub port: u16,
    pub tls: TlsMode,
    pub username: String,
    pub password: SecretString,
    /// `"Display Name <address@host>"` (or just the bare address) — this
    /// account's own address, used as the `From`.
    pub from_address: String,
}

pub enum SmtpCommand {
    Send { account: SmtpAccount, compose: ComposeState },
}

#[derive(Debug)]
pub enum SmtpEvent {
    Sent,
    Error(String),
}

pub struct SmtpActor {
    cmd_rx: mpsc::Receiver<SmtpCommand>,
    event_tx: mpsc::Sender<SmtpEvent>,
}

impl SmtpActor {
    pub fn spawn(cmd_rx: mpsc::Receiver<SmtpCommand>, event_tx: mpsc::Sender<SmtpEvent>) {
        let mut actor = SmtpActor { cmd_rx, event_tx };
        tokio::spawn(async move {
            actor.run().await;
        });
    }

    async fn run(&mut self) {
        while let Some(cmd) = self.cmd_rx.recv().await {
            match cmd {
                SmtpCommand::Send { account, compose } => match Self::send(&account, &compose).await {
                    Ok(()) => {
                        let _ = self.event_tx.send(SmtpEvent::Sent).await;
                    }
                    Err(e) => {
                        let _ = self.event_tx.send(SmtpEvent::Error(e.to_string())).await;
                    }
                },
            }
        }
    }

    async fn send(account: &SmtpAccount, compose: &ComposeState) -> anyhow::Result<()> {
        let message = build_message(account, compose)?;
        let transport = build_transport(account)?;
        transport.send(message).await?;
        Ok(())
    }
}

fn build_transport(account: &SmtpAccount) -> anyhow::Result<AsyncSmtpTransport<Tokio1Executor>> {
    let credentials = Credentials::new(account.username.clone(), account.password.expose_secret().to_string());
    let builder = match account.tls {
        TlsMode::Ssl => AsyncSmtpTransport::<Tokio1Executor>::relay(&account.host)?,
        TlsMode::StartTls => AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&account.host)?,
        // Only useful for a local/test server -- same caveat as the identical
        // TlsMode::None case in config.rs's doc comment.
        TlsMode::None => AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&account.host),
    };
    Ok(builder.port(account.port).credentials(credentials).build())
}

fn build_message(account: &SmtpAccount, compose: &ComposeState) -> anyhow::Result<Message> {
    let mut builder = Message::builder()
        .from(account.from_address.parse()?)
        .subject(compose.subject.clone());
    for address in split_addresses(&compose.to) {
        builder = builder.to(address.parse()?);
    }
    for address in split_addresses(&compose.cc) {
        builder = builder.cc(address.parse()?);
    }
    for address in split_addresses(&compose.bcc) {
        builder = builder.bcc(address.parse()?);
    }
    if let Some(id) = &compose.in_reply_to {
        builder = builder.in_reply_to(id.clone());
    }
    if let Some(id) = &compose.references {
        builder = builder.references(id.clone());
    }

    if compose.attachments.is_empty() {
        Ok(builder.body(compose.body.clone())?)
    } else {
        let mut multipart = MultiPart::mixed().singlepart(SinglePart::plain(compose.body.clone()));
        for (filename, data) in &compose.attachments {
            let content_type = ContentType::parse(guess_mime_type(filename))
                .unwrap_or_else(|_| ContentType::parse("application/octet-stream").expect("static value is valid"));
            multipart = multipart.singlepart(Attachment::new(filename.clone()).body(data.clone(), content_type));
        }
        Ok(builder.multipart(multipart)?)
    }
}

/// Comma-separated addresses, e.g. from a To/Cc/Bcc field, trimmed and with
/// empty entries (a trailing comma, blank field) dropped.
fn split_addresses(field: &str) -> Vec<String> {
    field.split(',').map(str::trim).filter(|a| !a.is_empty()).map(str::to_string).collect()
}

/// A small built-in extension → MIME type table, rather than a
/// `mime_guess`-style dependency for what's realistically going to be a
/// handful of common attachment types. Anything unrecognized becomes
/// `application/octet-stream`, which every mail client treats as "just an
/// attachment, no special handling" -- a safe, generic fallback rather than
/// a guess that could be wrong.
fn guess_mime_type(filename: &str) -> &'static str {
    let ext = filename.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "txt" => "text/plain",
        "csv" => "text/csv",
        "html" | "htm" => "text/html",
        "zip" => "application/zip",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xls" => "application/vnd.ms-excel",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_addresses_trims_and_drops_empty_entries() {
        assert_eq!(
            split_addresses(" alice@example.com ,bob@example.com,, carol@example.com"),
            vec!["alice@example.com", "bob@example.com", "carol@example.com"]
        );
    }

    #[test]
    fn split_addresses_of_an_empty_field_is_empty() {
        assert!(split_addresses("").is_empty());
        assert!(split_addresses("   ").is_empty());
    }

    #[test]
    fn guess_mime_type_recognizes_common_extensions() {
        assert_eq!(guess_mime_type("report.pdf"), "application/pdf");
        assert_eq!(guess_mime_type("photo.JPG"), "image/jpeg");
    }

    #[test]
    fn guess_mime_type_falls_back_to_octet_stream() {
        assert_eq!(guess_mime_type("mystery.xyz"), "application/octet-stream");
        assert_eq!(guess_mime_type("no_extension"), "application/octet-stream");
    }

    #[test]
    fn build_message_with_no_attachments_produces_a_plain_body() {
        let account = SmtpAccount {
            host: "smtp.example.com".to_string(),
            port: 465,
            tls: TlsMode::Ssl,
            username: "alice@example.com".to_string(),
            password: SecretString::from("hunter2"),
            from_address: "Alice <alice@example.com>".to_string(),
        };
        let compose = ComposeState {
            to: "bob@example.com".to_string(),
            subject: "Hello".to_string(),
            body: "Hi Bob".to_string(),
            ..Default::default()
        };
        let message = build_message(&account, &compose).expect("should build");
        let raw = String::from_utf8_lossy(&message.formatted()).to_string();
        assert!(raw.contains("Hi Bob"));
        assert!(raw.contains("Subject: Hello"));
    }

    #[test]
    fn build_message_sets_threading_headers_when_present() {
        let account = SmtpAccount {
            host: "smtp.example.com".to_string(),
            port: 465,
            tls: TlsMode::Ssl,
            username: "alice@example.com".to_string(),
            password: SecretString::from("hunter2"),
            from_address: "alice@example.com".to_string(),
        };
        let compose = ComposeState {
            to: "bob@example.com".to_string(),
            subject: "Re: Hello".to_string(),
            body: "Hi Bob".to_string(),
            in_reply_to: Some("<abc@example.com>".to_string()),
            references: Some("<abc@example.com>".to_string()),
            ..Default::default()
        };
        let message = build_message(&account, &compose).expect("should build");
        let raw = String::from_utf8_lossy(&message.formatted()).to_string();
        assert!(raw.contains("In-Reply-To: <abc@example.com>"));
        assert!(raw.contains("References: <abc@example.com>"));
    }

    #[test]
    fn build_message_rejects_an_unparseable_recipient_rather_than_silently_dropping_it() {
        let account = SmtpAccount {
            host: "smtp.example.com".to_string(),
            port: 465,
            tls: TlsMode::Ssl,
            username: "alice@example.com".to_string(),
            password: SecretString::from("hunter2"),
            from_address: "alice@example.com".to_string(),
        };
        let compose = ComposeState {
            to: "not an email address".to_string(),
            subject: "Hello".to_string(),
            body: "Hi".to_string(),
            ..Default::default()
        };
        assert!(build_message(&account, &compose).is_err());
    }

    #[test]
    fn build_message_with_an_attachment_includes_it_as_a_separate_part() {
        let account = SmtpAccount {
            host: "smtp.example.com".to_string(),
            port: 465,
            tls: TlsMode::Ssl,
            username: "alice@example.com".to_string(),
            password: SecretString::from("hunter2"),
            from_address: "alice@example.com".to_string(),
        };
        let compose = ComposeState {
            to: "bob@example.com".to_string(),
            subject: "Files".to_string(),
            body: "see attached".to_string(),
            attachments: vec![("notes.txt".to_string(), b"hello world".to_vec())],
            ..Default::default()
        };
        let message = build_message(&account, &compose).expect("should build");
        let raw = String::from_utf8_lossy(&message.formatted()).to_string();
        assert!(raw.contains("notes.txt"));
        assert!(raw.contains("see attached"));
    }
}

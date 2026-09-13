use std::time::Duration;

use tokio::sync::mpsc;
use tokio::net::TcpStream;
use tokio_native_tls::TlsStream;
use tokio_native_tls::native_tls::TlsConnector;
use mailparse::parse_mail;
use secrecy::{SecretString, ExposeSecret};
use futures::StreamExt;
use anyhow::anyhow;

/// How many times [`ImapActor::ensure_connected`] retries a lost connection
/// before giving up and reporting the error to the UI.
const MAX_RECONNECT_ATTEMPTS: u32 = 5;
/// Backoff between reconnect attempts: 1s, 2s, 4s, 8s, capped at 16s.
const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(16);

/// Credentials kept around so a dropped connection can be retried without the
/// user re-entering their password. Held only in memory, never persisted —
/// see `crate::secrets` for the on-disk (keyring) copy.
#[derive(Clone)]
struct Credentials {
    host: String,
    port: u16,
    username: String,
    password: SecretString,
}

#[derive(Debug, Clone)]
pub struct MailHeader {
    pub uid: u32,
    pub subject: String,
    pub from: String,
    pub to: String,
    pub date: String,
}

/// UIDVALIDITY/UIDNEXT as of the most recent `EXAMINE`/`SELECT`, read off
/// values `async_imap` already parses from the server's untagged response —
/// getting this costs nothing beyond what `fetch_headers`/`fetch_body`
/// already do. Feeds `db.rs`'s incremental-sync bookkeeping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MailboxState {
    pub uid_validity: u32,
    pub uid_next: u32,
}

pub enum ImapCommand {
    Connect {
        host: String,
        port: u16,
        username: String,
        password: SecretString,
    },
    FetchMailboxes,
    /// `req_id` is echoed on the resulting [`ImapEvent::Headers`] (or
    /// `Error`, which does not carry it — see its doc) so the UI can drop a
    /// reply that arrives after a newer request superseded it, instead of the
    /// old mailbox-name string compare which could not tell two requests for
    /// the *same* mailbox apart (e.g. hitting "Refresh" twice quickly).
    FetchHeaders { mailbox: String, page: u32, req_id: u64 },
    /// See `FetchHeaders`; echoed on [`ImapEvent::Body`].
    FetchBody { mailbox: String, uid: u32, req_id: u64 },
    BulkDownload { mailbox: String },
}

pub enum ImapEvent {
    Connected,
    /// The connection was lost (or a command needed a reconnect). Followed by
    /// either a `Connected` once [`ImapActor::ensure_connected`]'s retry loop
    /// succeeds, or an `Error` once it exhausts its attempts.
    Disconnected,
    Error(String),
    Mailboxes(Vec<String>),
    Headers { mailbox: String, headers: Vec<MailHeader>, page: u32, total_pages: u32, req_id: u64, mailbox_state: MailboxState },
    Body { uid: u32, html: String, req_id: u64 },
    DownloadProgress { current: u32, total: u32 },
    MailData { mailbox: String, header: MailHeader, body: String },
}

pub struct ImapActor {
    cmd_rx: mpsc::Receiver<ImapCommand>,
    event_tx: mpsc::Sender<ImapEvent>,
    session: Option<async_imap::Session<TlsStream<TcpStream>>>,
    /// Set on the first successful [`ImapActor::connect`]; reused by
    /// [`ImapActor::ensure_connected`] to reconnect without the user retyping
    /// their password.
    credentials: Option<Credentials>,
}

impl ImapActor {
    pub fn spawn(
        cmd_rx: mpsc::Receiver<ImapCommand>,
        event_tx: mpsc::Sender<ImapEvent>,
    ) {
        let mut actor = ImapActor {
            cmd_rx,
            event_tx,
            session: None,
            credentials: None,
        };

        tokio::spawn(async move {
            actor.run().await;
        });
    }

    async fn run(&mut self) {
        while let Some(cmd) = self.cmd_rx.recv().await {
            match cmd {
                ImapCommand::Connect { host, port, username, password } => {
                    match self.connect(&host, port, &username, password.expose_secret()).await {
                        Ok(_) => {
                            // Remembered so `ensure_connected` can reconnect
                            // without the user retyping their password.
                            self.credentials = Some(Credentials { host, port, username, password });
                            let _ = self.event_tx.send(ImapEvent::Connected).await;
                        }
                        Err(e) => {
                            let _ = self.event_tx.send(ImapEvent::Error(e.to_string())).await;
                        }
                    }
                }
                ImapCommand::FetchMailboxes => {
                    if let Err(e) = self.ensure_connected().await {
                        let _ = self.event_tx.send(ImapEvent::Error(e.to_string())).await;
                        continue;
                    }
                    let session = self.session.as_mut().expect("ensure_connected just verified this");
                    match Self::fetch_mailboxes(session).await {
                        Ok(mbs) => {
                            let _ = self.event_tx.send(ImapEvent::Mailboxes(mbs)).await;
                        }
                        Err(e) => {
                            self.session = None;
                            let _ = self.event_tx.send(ImapEvent::Error(e.to_string())).await;
                        }
                    }
                }
                ImapCommand::FetchHeaders { mailbox, page, req_id } => {
                    if let Err(e) = self.ensure_connected().await {
                        let _ = self.event_tx.send(ImapEvent::Error(e.to_string())).await;
                        continue;
                    }
                    let session = self.session.as_mut().expect("ensure_connected just verified this");
                    match Self::fetch_headers(session, &mailbox, page).await {
                        Ok((headers, total_pages, mailbox_state)) => {
                            let _ = self.event_tx.send(ImapEvent::Headers { mailbox, headers, page, total_pages, req_id, mailbox_state }).await;
                        }
                        Err(e) => {
                            self.session = None;
                            let _ = self.event_tx.send(ImapEvent::Error(e.to_string())).await;
                        }
                    }
                }
                ImapCommand::FetchBody { mailbox, uid, req_id } => {
                    if let Err(e) = self.ensure_connected().await {
                        let _ = self.event_tx.send(ImapEvent::Error(e.to_string())).await;
                        continue;
                    }
                    let session = self.session.as_mut().expect("ensure_connected just verified this");
                    match Self::fetch_body(session, &mailbox, uid).await {
                        Ok(html) => {
                            let _ = self.event_tx.send(ImapEvent::Body { uid, html, req_id }).await;
                        }
                        Err(e) => {
                            self.session = None;
                            let _ = self.event_tx.send(ImapEvent::Error(e.to_string())).await;
                        }
                    }
                }
                ImapCommand::BulkDownload { mailbox } => {
                    if let Err(e) = self.ensure_connected().await {
                        let _ = self.event_tx.send(ImapEvent::Error(e.to_string())).await;
                        continue;
                    }
                    let session = self.session.as_mut().expect("ensure_connected just verified this");
                    if let Err(e) = Self::bulk_download(session, &mailbox, &self.event_tx).await {
                        self.session = None;
                        let _ = self.event_tx.send(ImapEvent::Error(e.to_string())).await;
                    }
                }
            }
        }
    }

    /// Reconnect using the last credentials that worked, with exponential
    /// backoff, if the session was dropped (by `Connect` never having
    /// succeeded, or by a prior command failing and clearing `self.session`).
    /// A no-op — and free — when already connected.
    ///
    /// This is the auto-reconnect half of B2; it does *not* attempt to keep a
    /// separate control session alive for IDLE (there is no IDLE session yet
    /// at all — see PLAN.md §B2's noted scope cut).
    async fn ensure_connected(&mut self) -> anyhow::Result<()> {
        if self.session.is_some() {
            return Ok(());
        }
        let creds = self
            .credentials
            .clone()
            .ok_or_else(|| anyhow!("not connected yet"))?;

        let _ = self.event_tx.send(ImapEvent::Disconnected).await;

        let mut delay = Duration::from_secs(1);
        let mut last_err = None;
        for attempt in 1..=MAX_RECONNECT_ATTEMPTS {
            match self
                .connect(&creds.host, creds.port, &creds.username, creds.password.expose_secret())
                .await
            {
                Ok(()) => {
                    let _ = self.event_tx.send(ImapEvent::Connected).await;
                    return Ok(());
                }
                Err(e) => {
                    log::warn!("reconnect attempt {attempt}/{MAX_RECONNECT_ATTEMPTS} failed: {e}");
                    last_err = Some(e);
                    if attempt < MAX_RECONNECT_ATTEMPTS {
                        tokio::time::sleep(delay).await;
                        delay = (delay * 2).min(MAX_RECONNECT_DELAY);
                    }
                }
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow!("reconnect failed")))
    }

    async fn connect(&mut self, host: &str, port: u16, username: &str, password: &str) -> anyhow::Result<()> {
        let tls_connector = TlsConnector::builder().build()?;
        let tokio_tls_connector = tokio_native_tls::TlsConnector::from(tls_connector);

        let stream = TcpStream::connect((host, port)).await?;
        let tls_stream = tokio_tls_connector.connect(host, stream).await?;
        let mut client = async_imap::Client::new(tls_stream);
        let _ = client.read_response().await;

        let session = client.login(username, password).await.map_err(|(e, _)| e)?;

        self.session = Some(session);
        Ok(())
    }

    async fn fetch_mailboxes(session: &mut async_imap::Session<TlsStream<TcpStream>>) -> anyhow::Result<Vec<String>> {
        let mut mailboxes = Vec::new();
        let mut fetches = session.list(Some(""), Some("*")).await?;
        while let Some(name) = fetches.next().await {
            if let Ok(name) = name {
                mailboxes.push(name.name().to_string());
            }
        }
        Ok(mailboxes)
    }

    fn decode_rfc2047(bytes: &[u8]) -> String {
        let mut raw = b"X: ".to_vec();
        raw.extend_from_slice(bytes);
        raw.push(b'\n');
        if let Ok((header, _)) = mailparse::parse_header(&raw) {
            header.get_value()
        } else {
            String::from_utf8_lossy(bytes).to_string()
        }
    }

    async fn fetch_headers(session: &mut async_imap::Session<TlsStream<TcpStream>>, mailbox_name: &str, page: u32) -> anyhow::Result<(Vec<MailHeader>, u32, MailboxState)> {
        let mailbox = session.examine(mailbox_name).await?;
        // `examine` already gets these off the server's untagged response —
        // no extra round trip. `db.rs`'s incremental-sync bookkeeping
        // (`DbCommand::ReportMailboxState`) rides along on every header
        // fetch for free.
        let mailbox_state = MailboxState {
            uid_validity: mailbox.uid_validity.unwrap_or(0),
            uid_next: mailbox.uid_next.unwrap_or(0),
        };

        let total = mailbox.exists;
        if total == 0 {
            return Ok((Vec::new(), 0, mailbox_state));
        }

        let per_page = 50;
        let total_pages = (total + per_page - 1) / per_page;
        let page = page.min(total_pages).max(1);

        let end = total.saturating_sub((page - 1) * per_page);
        let start = end.saturating_sub(per_page - 1).max(1);

        let query = format!("{}:{}", start, end);
        let fetches = session.fetch(query, "(UID ENVELOPE)").await?;
        let messages = fetches.collect::<Vec<_>>().await;
        
        let mut headers = Vec::new();
        for msg in messages {
            let msg = msg?;
            let uid = msg.uid.ok_or_else(|| anyhow!("No UID"))?;
            let envelope = msg.envelope().ok_or_else(|| anyhow!("No envelope"))?;
            
            let subject = envelope.subject.as_ref().map(|s| Self::decode_rfc2047(s)).unwrap_or_default();
            
            let format_address = |addrs: Option<&[async_imap::imap_proto::Address<'_>]>| -> String {
                addrs.and_then(|f| f.first()).map(|addr| {
                    let name = addr.name.as_ref().map(|n| Self::decode_rfc2047(n));
                    let mailbox = addr.mailbox.as_ref().map(|m| String::from_utf8_lossy(m).to_string()).unwrap_or_default();
                    let host = addr.host.as_ref().map(|h| String::from_utf8_lossy(h).to_string()).unwrap_or_default();
                    match name {
                        Some(n) => format!("{} <{}@{}>", n, mailbox, host),
                        None => format!("{}@{}", mailbox, host),
                    }
                }).unwrap_or_default()
            };
            
            let from = format_address(envelope.from.as_deref());
            let to = format_address(envelope.to.as_deref());
            let date = envelope.date.as_ref().map(|d| String::from_utf8_lossy(d).to_string()).unwrap_or_default();

            headers.push(MailHeader { uid, subject, from, to, date });
        }
        
        headers.reverse(); // Newest first
        Ok((headers, total_pages, mailbox_state))
    }

    async fn fetch_body(session: &mut async_imap::Session<TlsStream<TcpStream>>, mailbox_name: &str, uid: u32) -> anyhow::Result<String> {
        session.examine(mailbox_name).await?;
        let query = format!("{}", uid);
        let mut fetches = session.uid_fetch(query, "RFC822").await?;
        
        if let Some(msg) = fetches.next().await {
            let msg = msg?;
            let body = msg.body().ok_or_else(|| anyhow::anyhow!("No body"))?;
            let parsed = parse_mail(body)?;
            
            // Try to find HTML part, fallback to text
            fn find_html(part: &mailparse::ParsedMail) -> Option<String> {
                if part.ctype.mimetype == "text/html" {
                    return part.get_body().ok();
                }
                for subpart in &part.subparts {
                    if let Some(html) = find_html(subpart) {
                        return Some(html);
                    }
                }
                None
            }

            fn find_text(part: &mailparse::ParsedMail) -> Option<String> {
                if part.ctype.mimetype == "text/plain" {
                    return part.get_body().ok();
                }
                for subpart in &part.subparts {
                    if let Some(text) = find_text(subpart) {
                        return Some(text);
                    }
                }
                None
            }

            if let Some(html) = find_html(&parsed) {
                return Ok(html);
            } else if let Some(text) = find_text(&parsed) {
                return Ok(format!("<pre>{}</pre>", text));
            }
        }
        
        Err(anyhow!("Message not found or no body"))
    }

    async fn bulk_download(
        session: &mut async_imap::Session<TlsStream<TcpStream>>,
        mailbox_name: &str,
        event_tx: &mpsc::Sender<ImapEvent>,
    ) -> anyhow::Result<()> {
        let mailbox = session.examine(mailbox_name).await?;
        let total = mailbox.exists;
        if total == 0 {
            return Ok(());
        }

        let _ = event_tx.send(ImapEvent::DownloadProgress { current: 0, total }).await;

        // Fetch all UIDs and Envelopes first to get metadata
        let query = format!("1:{}", total);
        let fetches = session.fetch(query, "(UID ENVELOPE)").await?;
        let messages = fetches.collect::<Vec<_>>().await;

        for (i, msg) in messages.into_iter().enumerate() {
            let msg = msg?;
            let uid = msg.uid.ok_or_else(|| anyhow!("No UID"))?;
            let envelope = msg.envelope().ok_or_else(|| anyhow!("No envelope"))?;

            let subject = envelope.subject.as_ref().map(|s| Self::decode_rfc2047(s)).unwrap_or_default();
            
            let format_address = |addrs: Option<&[async_imap::imap_proto::Address<'_>]>| -> String {
                addrs.and_then(|f| f.first()).map(|addr| {
                    let name = addr.name.as_ref().map(|n| Self::decode_rfc2047(n));
                    let mailbox = addr.mailbox.as_ref().map(|m| String::from_utf8_lossy(m).to_string()).unwrap_or_default();
                    let host = addr.host.as_ref().map(|h| String::from_utf8_lossy(h).to_string()).unwrap_or_default();
                    match name {
                        Some(n) => format!("{} <{}@{}>", n, mailbox, host),
                        None => format!("{}@{}", mailbox, host),
                    }
                }).unwrap_or_default()
            };
            
            let from = format_address(envelope.from.as_deref());
            let to = format_address(envelope.to.as_deref());
            let date = envelope.date.as_ref().map(|d| String::from_utf8_lossy(d).to_string()).unwrap_or_default();

            let header = MailHeader { uid, subject, from, to, date };

            // Now fetch body for this UID
            let body_query = format!("{}", uid);
            let mut body_fetches = session.uid_fetch(body_query, "RFC822").await?;
            let mut body = String::new();
            if let Some(body_msg) = body_fetches.next().await {
                let body_msg = body_msg?;
                if let Some(bytes) = body_msg.body() {
                    let parsed = parse_mail(bytes)?;
                    
                    fn find_html(part: &mailparse::ParsedMail) -> Option<String> {
                        if part.ctype.mimetype == "text/html" {
                            return part.get_body().ok();
                        }
                        for subpart in &part.subparts {
                            if let Some(html) = find_html(subpart) {
                                return Some(html);
                            }
                        }
                        None
                    }

                    fn find_text(part: &mailparse::ParsedMail) -> Option<String> {
                        if part.ctype.mimetype == "text/plain" {
                            return part.get_body().ok();
                        }
                        for subpart in &part.subparts {
                            if let Some(text) = find_text(subpart) {
                                return Some(text);
                            }
                        }
                        None
                    }

                    if let Some(html) = find_html(&parsed) {
                        body = html;
                    } else if let Some(text) = find_text(&parsed) {
                        body = format!("<pre>{}</pre>", text);
                    }
                }
            }

            let _ = event_tx.send(ImapEvent::MailData {
                mailbox: mailbox_name.to_string(),
                header,
                body,
            }).await;

            let _ = event_tx.send(ImapEvent::DownloadProgress {
                current: (i + 1) as u32,
                total,
            }).await;
        }

        Ok(())
    }
}

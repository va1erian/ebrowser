use std::time::Duration;

use tokio::sync::mpsc;
use tokio::net::TcpStream;
use tokio_native_tls::TlsStream;
use tokio_native_tls::native_tls::TlsConnector;
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
    /// The `Message-ID` header, e.g. `<abc123@example.com>`, angle brackets
    /// included (that's how `In-Reply-To`/`References` expect it). Empty
    /// when the server's ENVELOPE didn't include one — rare, but legal.
    pub message_id: String,
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
    /// Lightweight new-mail poll (B10): re-`EXAMINE`s `mailbox` to read the
    /// fresh UIDVALIDITY/UIDNEXT off the untagged response -- the same free
    /// ride `fetch_headers` already takes, just without the ENVELOPE fetch
    /// that pulls the actual header list. Silently dropped if there is no
    /// live session: unlike every other command here, this deliberately
    /// does *not* call `ensure_connected` and retry with backoff -- a
    /// background poll on a timer should never itself trigger a reconnect
    /// storm while the user is offline. The next real user action still
    /// reconnects normally.
    PollMailbox { mailbox: String },
    /// Fetch just the envelopes for UIDs `first_uid..` in `mailbox`, to
    /// build a new-mail notification (B10). Unlike `FetchHeaders`, this is
    /// not paged and not `req_id`-tracked: it never feeds the visible
    /// message list, only `notify::build_notification`. Same "no
    /// `ensure_connected`" reasoning as `PollMailbox` -- this only ever
    /// follows a `PollMailbox` that just proved there's a live session.
    FetchNewHeaders { mailbox: String, first_uid: u32 },
    /// Fetch envelopes for UIDs `first_uid..` in `mailbox`, to feed `db.rs`'s
    /// incremental cache sync (B3) -- a `DbEvent::SyncPlan::FetchFrom` names
    /// exactly this UID range as what the cache is missing. Deliberately a
    /// distinct command/event pair from `FetchNewHeaders`/`NewHeaders`
    /// despite doing the identical fetch: those feed B10's new-mail toast
    /// (`spawn_new_mail_watch` in main.rs builds a notification from every
    /// `NewHeaders` it sees), and this fires far more often -- on every
    /// `FetchHeaders` that turns up UIDs the cache hasn't seen yet, which
    /// includes the user's own routine "open INBOX"/"hit refresh". Routing
    /// both through one event would toast the user for their own actions.
    /// Unlike `FetchNewHeaders`, this *does* call `ensure_connected`: it's a
    /// direct follow-up to a `FetchHeaders` that just proved the session
    /// live moments ago (not an independent background-timer poll), so
    /// there's no "don't start a reconnect storm while offline" concern to
    /// preserve.
    FetchHeadersFrom { mailbox: String, first_uid: u32 },
}

#[derive(Debug)]
pub enum ImapEvent {
    Connected,
    /// The connection was lost (or a command needed a reconnect). Followed by
    /// either a `Connected` once [`ImapActor::ensure_connected`]'s retry loop
    /// succeeds, or an `Error` once it exhausts its attempts.
    Disconnected,
    Error(String),
    Mailboxes(Vec<String>),
    Headers { mailbox: String, headers: Vec<MailHeader>, page: u32, total_pages: u32, req_id: u64, mailbox_state: MailboxState },
    Body { uid: u32, html: String, attachments: Vec<crate::render::Attachment>, req_id: u64 },
    DownloadProgress { current: u32, total: u32 },
    MailData { mailbox: String, header: MailHeader, body: String },
    /// Reply to `PollMailbox` (B10).
    MailboxPolled { mailbox: String, state: MailboxState },
    /// Reply to `FetchNewHeaders` (B10).
    NewHeaders { mailbox: String, headers: Vec<MailHeader> },
    /// Reply to `FetchHeadersFrom` (B3).
    HeadersFrom { mailbox: String, headers: Vec<MailHeader> },
    /// `PollMailbox`/`FetchNewHeaders` failed. Deliberately a separate
    /// variant from `Error` rather than reusing it: those two commands are
    /// background/best-effort (see their docs), and `main.rs` logs this
    /// instead of overwriting `self.status` with it, so a transient blip on
    /// a 60-second timer never stomps on whatever the user is actually
    /// looking at.
    PollFailed(String),
}

pub struct ImapActor {
    cmd_rx: mpsc::Receiver<ImapCommand>,
    event_tx: mpsc::Sender<ImapEvent>,
    session: Option<async_imap::Session<TlsStream<TcpStream>>>,
    /// Set on the first successful [`ImapActor::connect`]; reused by
    /// [`ImapActor::ensure_connected`] to reconnect without the user retyping
    /// their password.
    credentials: Option<Credentials>,
    /// Where `FetchBody`/`BulkDownload` get routed once connected -- see
    /// `spawn_body_worker`'s doc for why those two specifically live on a
    /// second connection instead of this actor's own `session`. `None`
    /// before the first successful `Connect`.
    worker_tx: Option<mpsc::Sender<WorkerCommand>>,
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
            worker_tx: None,
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
                            let creds = Credentials { host, port, username, password };
                            self.credentials = Some(creds.clone());
                            // One worker per successful `Connect`, not one
                            // per process: a second `Connect` (e.g. logging
                            // into a different account without restarting)
                            // should get a fresh worker on the new
                            // credentials rather than silently keep feeding
                            // `FetchBody`/`BulkDownload` to a worker still
                            // logged into the old account. The old worker's
                            // task simply ends once its `cmd_rx` (the
                            // `Sender` half we're about to drop here) closes.
                            let (worker_tx, worker_rx) = mpsc::channel(8);
                            spawn_body_worker(worker_rx, self.event_tx.clone(), creds);
                            self.worker_tx = Some(worker_tx);
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
                    // Routed to the body worker's own connection (see
                    // `spawn_body_worker`) rather than handled here, so a
                    // slow body fetch can't stall `FetchHeaders`/
                    // `FetchMailboxes` waiting behind it in this actor's own
                    // command queue.
                    let Some(worker) = &self.worker_tx else {
                        let _ = self.event_tx.send(ImapEvent::Error("not connected".to_string())).await;
                        continue;
                    };
                    let _ = worker.send(WorkerCommand::FetchBody { mailbox, uid, req_id }).await;
                }
                ImapCommand::BulkDownload { mailbox } => {
                    // Same reasoning as `FetchBody` above -- and doubly so
                    // here, since a bulk download is the single slowest,
                    // longest-running thing this actor ever does.
                    let Some(worker) = &self.worker_tx else {
                        let _ = self.event_tx.send(ImapEvent::Error("not connected".to_string())).await;
                        continue;
                    };
                    let _ = worker.send(WorkerCommand::BulkDownload { mailbox }).await;
                }
                ImapCommand::PollMailbox { mailbox } => {
                    // No `ensure_connected` here on purpose -- see the
                    // command's doc. Nothing to poll if there's no session.
                    let Some(session) = self.session.as_mut() else {
                        continue;
                    };
                    match session.examine(&mailbox).await {
                        Ok(mb) => {
                            let state = MailboxState {
                                uid_validity: mb.uid_validity.unwrap_or(0),
                                uid_next: mb.uid_next.unwrap_or(0),
                            };
                            let _ = self.event_tx.send(ImapEvent::MailboxPolled { mailbox, state }).await;
                        }
                        Err(e) => {
                            self.session = None;
                            let _ = self.event_tx.send(ImapEvent::PollFailed(e.to_string())).await;
                        }
                    }
                }
                ImapCommand::FetchNewHeaders { mailbox, first_uid } => {
                    let Some(session) = self.session.as_mut() else {
                        continue;
                    };
                    match Self::fetch_new_headers(session, &mailbox, first_uid).await {
                        Ok(headers) => {
                            let _ = self.event_tx.send(ImapEvent::NewHeaders { mailbox, headers }).await;
                        }
                        Err(e) => {
                            self.session = None;
                            let _ = self.event_tx.send(ImapEvent::PollFailed(e.to_string())).await;
                        }
                    }
                }
                ImapCommand::FetchHeadersFrom { mailbox, first_uid } => {
                    if let Err(e) = self.ensure_connected().await {
                        let _ = self.event_tx.send(ImapEvent::Error(e.to_string())).await;
                        continue;
                    }
                    let session = self.session.as_mut().expect("ensure_connected just verified this");
                    match Self::fetch_new_headers(session, &mailbox, first_uid).await {
                        Ok(headers) => {
                            let _ = self.event_tx.send(ImapEvent::HeadersFrom { mailbox, headers }).await;
                        }
                        Err(e) => {
                            self.session = None;
                            let _ = self.event_tx.send(ImapEvent::Error(e.to_string())).await;
                        }
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
    /// This only manages *this* actor's own session -- the one
    /// `FetchMailboxes`/`FetchHeaders`/`PollMailbox`/`FetchNewHeaders` use.
    /// It deliberately does not touch `self.worker_tx`: the body worker
    /// (`spawn_body_worker`, B2's session-pool split) keeps its own
    /// independent connection and reconnect loop, so a primary-session drop
    /// and reconnect here has no effect on it, and vice versa. IDLE (B11)
    /// is a third, still-separate connection (`idle_watch.rs`), managed
    /// entirely outside this actor.
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
        self.session = Some(connect_session(host, port, username, password).await?);
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

    /// Turn one fetched UID + its IMAP `ENVELOPE` into a [`MailHeader`].
    /// Factored out of `fetch_headers`/`bulk_download`, which each built
    /// this by hand before B10 needed a third copy for `fetch_new_headers` --
    /// three near-identical copies was the point at which "just duplicate it
    /// again" stopped being the lower-risk option.
    fn parse_envelope_header(uid: u32, envelope: &async_imap::imap_proto::Envelope<'_>) -> MailHeader {
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
        let message_id = envelope.message_id.as_ref().map(|m| String::from_utf8_lossy(m).to_string()).unwrap_or_default();

        MailHeader { uid, subject, from, to, date, message_id }
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
            headers.push(Self::parse_envelope_header(uid, envelope));
        }

        headers.reverse(); // Newest first
        Ok((headers, total_pages, mailbox_state))
    }

    async fn fetch_body(session: &mut async_imap::Session<TlsStream<TcpStream>>, mailbox_name: &str, uid: u32) -> anyhow::Result<(String, Vec<crate::render::Attachment>)> {
        session.examine(mailbox_name).await?;
        let query = format!("{}", uid);
        let mut fetches = session.uid_fetch(query, "RFC822").await?;

        if let Some(msg) = fetches.next().await {
            let msg = msg?;
            let body = msg.body().ok_or_else(|| anyhow::anyhow!("No body"))?;
            let html = crate::render::render_message(body);
            let attachments = crate::render::extract_attachments(body);
            return Ok((html, attachments));
        }

        Err(anyhow!("Message not found or no body"))
    }

    /// Fetch envelopes for every UID from `first_uid` onward (B10). Same
    /// `EXAMINE` + `UID FETCH ... (UID ENVELOPE)` shape as `fetch_headers`,
    /// minus the paging -- this always wants everything from `first_uid` to
    /// the end, since it exists to describe exactly the messages a
    /// `notify::WatermarkUpdate::NewMail` just reported as new.
    async fn fetch_new_headers(
        session: &mut async_imap::Session<TlsStream<TcpStream>>,
        mailbox_name: &str,
        first_uid: u32,
    ) -> anyhow::Result<Vec<MailHeader>> {
        session.examine(mailbox_name).await?;
        let query = format!("{}:*", first_uid);
        let fetches = session.uid_fetch(query, "(UID ENVELOPE)").await?;
        let messages = fetches.collect::<Vec<_>>().await;

        let mut headers = Vec::new();
        for msg in messages {
            let msg = msg?;
            let uid = msg.uid.ok_or_else(|| anyhow!("No UID"))?;
            let envelope = msg.envelope().ok_or_else(|| anyhow!("No envelope"))?;
            headers.push(Self::parse_envelope_header(uid, envelope));
        }
        Ok(headers)
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
            let header = Self::parse_envelope_header(uid, envelope);

            // Now fetch body for this UID
            let body_query = format!("{}", uid);
            let mut body_fetches = session.uid_fetch(body_query, "RFC822").await?;
            let mut body = String::new();
            if let Some(body_msg) = body_fetches.next().await {
                let body_msg = body_msg?;
                if let Some(bytes) = body_msg.body() {
                    body = crate::render::render_message(bytes);
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

/// The TLS-connect-then-`LOGIN` sequence, shared by [`ImapActor::connect`]
/// and [`spawn_body_worker`]'s own independent connection -- previously
/// inlined once in each of `ImapActor`'s two (now three, counting the
/// worker) call sites before B2's session-pool split gave it a second
/// caller.
async fn connect_session(
    host: &str,
    port: u16,
    username: &str,
    password: &str,
) -> anyhow::Result<async_imap::Session<TlsStream<TcpStream>>> {
    let tls_connector = TlsConnector::builder().build()?;
    let tokio_tls_connector = tokio_native_tls::TlsConnector::from(tls_connector);

    let stream = TcpStream::connect((host, port)).await?;
    let tls_stream = tokio_tls_connector.connect(host, stream).await?;
    let mut client = async_imap::Client::new(tls_stream);
    let _ = client.read_response().await;

    let session = client.login(username, password).await.map_err(|(e, _)| e)?;
    Ok(session)
}

/// Commands [`ImapActor`] hands off to [`spawn_body_worker`]'s dedicated
/// connection rather than handling on its own `session`.
enum WorkerCommand {
    FetchBody { mailbox: String, uid: u32, req_id: u64 },
    BulkDownload { mailbox: String },
}

/// B2's session-pool split: a second, independent IMAP connection that
/// handles only [`ImapCommand::FetchBody`]/[`ImapCommand::BulkDownload`],
/// so opening a message (or running a bulk download) never blocks
/// `ImapActor`'s own session -- the one `FetchHeaders`/`FetchMailboxes` use
/// to keep the header list and mailbox tree responsive. This was the one
/// piece of B2 (PLAN.md's own wording: "one long-lived control session ...
/// plus a worker session for fetches") that stayed undone through B7 for
/// lack of anything to verify a live-IMAP-protocol rework against;
/// `mail-mock-server` (added since) is what unblocks it now, the same way
/// it unblocked B11's `IDLE` support.
///
/// Deliberately its own small connect/reconnect loop rather than sharing
/// `ImapActor::ensure_connected`: that method emits `ImapEvent::Connected`/
/// `Disconnected`, which the UI uses to gate the whole "are we logged in"
/// state (`EsMailApp::is_connected`) and `spawn_new_mail_watch`'s poll
/// gate. A worker reconnect blipping that global state on every dropped
/// body fetch would be misleading -- the *account* is still connected as
/// far as the user should see, only this one background connection needed
/// to retry. So failures here are reported only on the specific request
/// that hit them (`ImapEvent::Error`), and a successful reconnect is
/// silent, exactly like a request that never needed to reconnect at all.
fn spawn_body_worker(
    mut cmd_rx: mpsc::Receiver<WorkerCommand>,
    event_tx: mpsc::Sender<ImapEvent>,
    credentials: Credentials,
) {
    tokio::spawn(async move {
        let mut session: Option<async_imap::Session<TlsStream<TcpStream>>> = None;

        while let Some(cmd) = cmd_rx.recv().await {
            if session.is_none() {
                match ensure_worker_connected(&credentials).await {
                    Ok(s) => session = Some(s),
                    Err(e) => {
                        let _ = event_tx.send(ImapEvent::Error(format!("could not open a connection for this request: {e}"))).await;
                        continue;
                    }
                }
            }
            let sess = session.as_mut().expect("just verified Some above");

            match cmd {
                WorkerCommand::FetchBody { mailbox, uid, req_id } => {
                    match ImapActor::fetch_body(sess, &mailbox, uid).await {
                        Ok((html, attachments)) => {
                            let _ = event_tx.send(ImapEvent::Body { uid, html, attachments, req_id }).await;
                        }
                        Err(e) => {
                            session = None; // let the next command's ensure_worker_connected retry
                            let _ = event_tx.send(ImapEvent::Error(e.to_string())).await;
                        }
                    }
                }
                WorkerCommand::BulkDownload { mailbox } => {
                    if let Err(e) = ImapActor::bulk_download(sess, &mailbox, &event_tx).await {
                        session = None;
                        let _ = event_tx.send(ImapEvent::Error(e.to_string())).await;
                    }
                }
            }
        }
    });
}

/// Connect-with-backoff for [`spawn_body_worker`], mirroring
/// [`ImapActor::ensure_connected`]'s retry shape (same attempt count and
/// delay curve, see `MAX_RECONNECT_ATTEMPTS`/`MAX_RECONNECT_DELAY`) but
/// returning the session instead of storing it on `self`, and never
/// sending `Connected`/`Disconnected` -- see `spawn_body_worker`'s doc for
/// why.
async fn ensure_worker_connected(credentials: &Credentials) -> anyhow::Result<async_imap::Session<TlsStream<TcpStream>>> {
    let mut delay = Duration::from_secs(1);
    let mut last_err = None;
    for attempt in 1..=MAX_RECONNECT_ATTEMPTS {
        match connect_session(&credentials.host, credentials.port, &credentials.username, credentials.password.expose_secret()).await {
            Ok(session) => return Ok(session),
            Err(e) => {
                log::warn!("body worker reconnect attempt {attempt}/{MAX_RECONNECT_ATTEMPTS} failed: {e}");
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

use esmail::{compose, config, db, idle_watch, imap, notify, render, screenshot, search_query, secrets, smtp};
/// Tray icon + Windows toast notifications (B10). Windows-only: see
/// notify.rs's module doc for why the pure detection logic lives separately
/// and builds everywhere.
#[cfg(target_os = "windows")]
use esmail::tray;

use egui_servo_webview::{
    InterceptOutcome, NavigationPolicy, WebResourceRequest, WebView, WebViewConfig, WebViewHandler,
    WebViewHost, WebViewSource,
};
use imap::{ImapActor, ImapCommand, ImapEvent, MailHeader};
use db::{DbActor, DbCommand, DbEvent};
use config::{AccountConfig, Config};
use search_query::ParsedQuery;
use egui_servo_webview::dpi::PhysicalSize;
use secrecy::SecretString;
use std::cell::RefCell;
use std::rc::Rc;
use tokio::sync::mpsc;

/// Navigation/interception policy for the single [`WebView`] esmail reuses to
/// show every message body.
///
/// - Navigation is always denied: a clicked link is reported as
///   [`egui_servo_webview::WebViewEvent::LinkClicked`] and opened in the
///   system browser instead (see below), so the view showing untrusted mail
///   HTML never navigates itself away from the message (B5 in PLAN.md).
/// - Remote `http(s)` resources are blocked unless `allow_remote` is set,
///   which the "Load remote images" button flips for the message currently
///   showing. This is the real blocking mechanism B5 calls for — markup
///   alone can't stop a network fetch, so `render.rs` leaves every remote
///   URL in the message's HTML exactly as it was, and this is what actually
///   decides whether the request happens at all.
struct MessageViewHandler {
    allow_remote: bool,
}

impl WebViewHandler for MessageViewHandler {
    fn navigation(&mut self, _url: &egui_servo_webview::url::Url) -> NavigationPolicy {
        NavigationPolicy::Deny
    }

    fn intercept(&mut self, request: &WebResourceRequest) -> InterceptOutcome {
        if self.allow_remote {
            return InterceptOutcome::Allow;
        }
        match request.url.scheme() {
            "http" | "https" => InterceptOutcome::Block,
            _ => InterceptOutcome::Allow,
        }
    }
}

struct EsMailApp {
    // Field order is drop order: the view must be torn down before the engine
    // that backs it, so it stays declared above the host.
    web_view: WebView,
    /// Owns the Servo engine; one per window. Outlives every view.
    web_view_host: WebViewHost,
    /// Bound to `web_view` at construction. Toggled per-message by the "Load
    /// remote images" button; reset to blocked whenever a new message is
    /// opened. See [`MessageViewHandler`].
    message_view_handler: Rc<RefCell<MessageViewHandler>>,
    screenshotter: screenshot::Screenshotter,
    /// Show only the webview, with no IMAP account. See ESMAIL_PREVIEW.
    preview: bool,
    imap_tx: mpsc::Sender<ImapCommand>,
    imap_rx: mpsc::Receiver<ImapEvent>,
    db_tx: mpsc::Sender<DbCommand>,
    db_rx: mpsc::Receiver<DbEvent>,
    smtp_tx: mpsc::Sender<smtp::SmtpCommand>,
    smtp_rx: mpsc::Receiver<smtp::SmtpEvent>,
    /// Sender half of the channel `spawn_new_mail_watch`'s task reads
    /// [`idle_watch::MailboxChanged`] pushes from. Kept on `EsMailApp` so the
    /// "Connect" button can hand a fresh clone to `idle_watch::spawn` once it
    /// knows the account's host/username/password — `idle_watch` itself has
    /// no way to learn those except from the same login form `ImapCommand::
    /// Connect` already reads them from.
    idle_wake_tx: mpsc::Sender<idle_watch::MailboxChanged>,
    /// Set once the "Connect" button has spawned an `idle_watch` task, so a
    /// second click (retrying after a typo'd password, say) doesn't leak
    /// another one. Unlike `ImapActor`'s single `Option<Session>` — which
    /// naturally drops (and so closes) the old TLS connection when `connect`
    /// overwrites it — each `idle_watch::spawn` call starts an independent
    /// `tokio::spawn` loop with no handle to cancel the previous one, so
    /// without this guard every retry would leave one more IDLE connection
    /// running forever. Never reset back to `false`: reconnecting to a
    /// *different* account without restarting the app is already not
    /// supported cleanly ("Logout" doesn't tear down `ImapActor`'s session
    /// either — a pre-existing limitation, not one this field adds).
    idle_watch_started: bool,

    /// Saved accounts (host/port/username; no passwords — those are in the OS
    /// keyring, see `secrets`). Persisted to `config.toml`.
    config: Config,

    // UI state
    host: String,
    port: String,
    username: String,
    password: String,
    /// SMTP host for the login form, prefilled from
    /// [`config::derive_smtp_host`]'s guess but editable — see B7 in
    /// PLAN.md. TLS mode is fixed to `Ssl`/465 for now; `StartTls`/`None`
    /// have no UI toggle yet, only the `AccountConfig` fields to hold them.
    smtp_host: String,
    smtp_port: String,
    status: String,
    is_connected: bool,
    
    mailboxes: Vec<String>,
    selected_mailbox: String,
    
    headers: Vec<MailHeader>,
    selected_uid: Option<u32>,
    current_page: u32,
    total_pages: u32,
    /// Attachments for the currently-open message (B6), if fetched directly
    /// from IMAP. Cleared whenever a different message is opened. A message
    /// opened from a cached search result never populates this — the cache
    /// only stores rendered HTML, not the raw bytes attachments come from;
    /// see PLAN.md §B6.
    current_attachments: Vec<render::Attachment>,
    /// The currently-open message's rendered HTML, kept only so
    /// Reply/Reply All/Forward (B7) can quote it — see `compose.rs`. Empty
    /// when no message is loaded.
    current_message_html: String,

    /// The compose window's state, when one is open — `None` means it's
    /// closed. See `compose.rs`.
    compose: Option<compose::ComposeState>,
    compose_status: String,

    /// Monotonic source for `ImapCommand::FetchHeaders`/`FetchBody` request
    /// ids. Only the reply matching `current_headers_req`/`current_body_req`
    /// is applied; an older one arriving late (e.g. a slow page-2 fetch
    /// answered after the user already moved to page 3) is dropped instead of
    /// clobbering newer state.
    next_req_id: u64,
    current_headers_req: u64,
    current_body_req: u64,

    // Search and Progress
    search_query: String,
    search_results: Option<Vec<MailHeader>>,
    download_progress: Option<(u32, u32)>,

    /// The tray icon (B10), or `None` if either it couldn't be created (see
    /// `tray::TrayState::new`'s doc) or this is a preview/screenshot run,
    /// where a tray icon would be unwanted background noise for what's
    /// meant to be a one-shot, no-account render. Window-close falls back to
    /// exiting normally whenever this is `None`, rather than hiding a window
    /// with no way to bring it back.
    #[cfg(target_os = "windows")]
    tray: Option<tray::TrayState>,
    /// Set by the tray's "Quit" action; the next close-request is then
    /// allowed to actually close the app instead of being redirected to
    /// "hide to tray". See `EsMailApp::logic`.
    #[cfg(target_os = "windows")]
    exit_requested: bool,
}

impl EsMailApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        init_logging();
        
        let (imap_cmd_tx, imap_cmd_rx) = mpsc::channel(32);
        let (imap_evt_tx, imap_evt_rx) = mpsc::channel(32);
        
        let (db_cmd_tx, db_cmd_rx) = mpsc::channel(32);
        let (db_evt_tx, db_evt_rx) = mpsc::channel(32);

        let egui_ctx = cc.egui_ctx.clone();
        
        // Wrap IMAP events: forward every event to the UI channel (bumping a
        // repaint), same as before B10 existed. `spawn_new_mail_watch` also
        // watches this same stream for the new-mail signal (B10) and turns
        // it into a background poll timer + a toast -- as a plain tokio
        // task, not anything hung off `EsMailApp::ui`/`logic`, it keeps
        // running (and can keep showing toasts) for as long as the process
        // is alive, independent of whether the main window is visible. See
        // its doc comment and tray.rs for how the window survives being
        // "closed".
        let (tx, rx) = mpsc::channel(32);
        // `idle_watch::spawn` (created once the "Connect" button knows the
        // account's credentials — see its call site) sends here whenever its
        // dedicated IDLE connection sees the server push something, so
        // `spawn_new_mail_watch` can poll immediately instead of waiting for
        // its own timer. A small buffer is enough: this only ever carries a
        // "go check" signal, never data, and a missed send just means the
        // next poll-timer tick catches it instead.
        let (idle_wake_tx, idle_wake_rx) = mpsc::channel(4);
        spawn_new_mail_watch(rx, imap_evt_tx, imap_cmd_tx.clone(), egui_ctx.clone(), idle_wake_rx);
        ImapActor::spawn(imap_cmd_rx, tx);

        // Wrap DB events
        let (tx_db, mut rx_db) = mpsc::channel(32);
        let ctx_clone_db = egui_ctx.clone();
        tokio::spawn(async move {
            while let Some(evt) = rx_db.recv().await {
                let _ = db_evt_tx.send(evt).await;
                ctx_clone_db.request_repaint();
            }
        });
        DbActor::spawn(db_cmd_rx, tx_db);

        let (smtp_cmd_tx, smtp_cmd_rx) = mpsc::channel(8);
        let (smtp_evt_tx, smtp_evt_rx) = mpsc::channel(8);
        let (tx_smtp, mut rx_smtp) = mpsc::channel(8);
        let ctx_clone_smtp = egui_ctx.clone();
        tokio::spawn(async move {
            while let Some(evt) = rx_smtp.recv().await {
                let _ = smtp_evt_tx.send(evt).await;
                ctx_clone_smtp.request_repaint();
            }
        });
        smtp::SmtpActor::spawn(smtp_cmd_rx, tx_smtp);

        // Preview mode: render one page full-window with no IMAP account, so the
        // webview itself can be exercised and screenshotted. ESMAIL_PREVIEW is
        // either a path to an HTML file, a URL, or "demo" for a built-in page.
        let preview = std::env::var("ESMAIL_PREVIEW").ok();
        let source = match preview.as_deref() {
            None => WebViewSource::Html(
                "<h1>Welcome to esMail</h1><p>Connect to your IMAP account to start reading.</p>"
                    .to_string(),
            ),
            Some("demo") => WebViewSource::Html(preview_demo_html()),
            Some(target) if target.starts_with("http") => WebViewSource::Url(target.to_string()),
            Some(path) => match std::fs::read_to_string(path) {
                Ok(html) => WebViewSource::Html(html),
                Err(e) => WebViewSource::Html(format!("<h1>could not read {path}</h1><p>{e}</p>")),
            },
        };
        
        let mut config = Config::load();
        if config.migrate_legacy() {
            if let Err(e) = config.save() {
                log::warn!("could not persist migrated config: {e}");
            }
        }

        // Prefill the login form from the first saved account, if any; its
        // password (if the OS keyring has one) comes along too, so a
        // returning user does not have to retype it.
        let (host_str, port_str, username_str, password_str, smtp_host_str, smtp_port_str) =
            match config.accounts.first() {
                Some(account) => {
                    let password = secrets::get_password(&account.id, "imap")
                        .map(|s| secrecy::ExposeSecret::expose_secret(&s).to_string())
                        .unwrap_or_default();
                    (
                        account.imap_host.clone(),
                        account.imap_port.to_string(),
                        account.username.clone(),
                        password,
                        account.smtp_host.clone(),
                        account.smtp_port.to_string(),
                    )
                }
                None => (
                    "imap.gmail.com".to_string(),
                    "993".to_string(),
                    String::new(),
                    String::new(),
                    "smtp.gmail.com".to_string(),
                    "465".to_string(),
                ),
            };
        let initial_status = "Ready".to_string();

        // One engine per window; the view borrows it to start up. A second view
        // (a compose preview, say) would come from this same host.
        let web_view_host = WebViewHost::from_eframe(cc, PhysicalSize::new(1280, 720))
            .expect("failed to initialise the Servo engine");
        let message_view_handler = Rc::new(RefCell::new(MessageViewHandler { allow_remote: false }));
        let web_view = web_view_host.new_view(
            &cc.egui_ctx,
            WebViewConfig::new(source).with_handler(message_view_handler.clone()),
        );

        // Skipped in preview/screenshot mode: HANDOFF.md's automated
        // screenshot verification runs a one-shot, no-account render and
        // exits on its own -- a tray icon there would be unwanted
        // background noise (and a needless dependency on the tray shell
        // being available) for a run nothing ever clicks on.
        #[cfg(target_os = "windows")]
        let tray = if preview.is_none() {
            match tray::TrayState::new() {
                Ok(t) => Some(t),
                Err(e) => {
                    log::warn!(
                        "could not create the system tray icon; closing the window will exit \
                         esMail normally instead of minimizing it: {e}"
                    );
                    None
                }
            }
        } else {
            None
        };

        Self {
            web_view_host,
            web_view,
            message_view_handler,
            screenshotter: screenshot::Screenshotter::from_env(),
            preview: preview.is_some(),
            imap_tx: imap_cmd_tx,
            imap_rx: imap_evt_rx,
            db_tx: db_cmd_tx,
            db_rx: db_evt_rx,
            smtp_tx: smtp_cmd_tx,
            smtp_rx: smtp_evt_rx,
            idle_wake_tx,
            idle_watch_started: false,
            config,
            host: host_str,
            port: port_str,
            username: username_str,
            password: password_str,
            smtp_host: smtp_host_str,
            smtp_port: smtp_port_str,
            status: initial_status,
            is_connected: false,
            mailboxes: Vec::new(),
            selected_mailbox: "INBOX".to_string(),
            headers: Vec::new(),
            selected_uid: None,
            current_page: 1,
            total_pages: 1,
            current_attachments: Vec::new(),
            current_message_html: String::new(),
            compose: None,
            compose_status: String::new(),
            next_req_id: 0,
            current_headers_req: 0,
            current_body_req: 0,
            search_query: String::new(),
            search_results: None,
            download_progress: None,
            #[cfg(target_os = "windows")]
            tray,
            #[cfg(target_os = "windows")]
            exit_requested: false,
        }
    }

    fn handle_imap_events(&mut self) {
        while let Ok(evt) = self.imap_rx.try_recv() {
            match evt {
                ImapEvent::Connected => {
                    self.status = "Connected!".to_string();
                    self.is_connected = true;
                    self.persist_current_account();
                    let _ = self.imap_tx.try_send(ImapCommand::FetchMailboxes);
                    self.fetch_headers(self.selected_mailbox.clone(), 1);
                }
                ImapEvent::Disconnected => {
                    self.status = "Connection lost, reconnecting...".to_string();
                }
                ImapEvent::Error(e) => {
                    self.status = format!("Error: {}", e);
                }
                ImapEvent::Mailboxes(mbs) => {
                    self.mailboxes = mbs;
                }
                ImapEvent::Headers { mailbox, headers, page, total_pages, req_id, mailbox_state } => {
                    // Only the most recently issued FetchHeaders' reply is
                    // applied; an older one arriving late (e.g. the mailbox
                    // was changed again before it came back) is dropped.
                    if req_id == self.current_headers_req && mailbox == self.selected_mailbox {
                        self.headers = headers;
                        self.current_page = page;
                        self.total_pages = total_pages;
                        self.status = format!("Page {} of {}", page, total_pages);
                    }
                    // Rides along on every header fetch regardless of
                    // req_id/mailbox staleness — db.rs's cache bookkeeping for
                    // `mailbox` should stay current even if this particular
                    // reply is no longer the one the UI is showing.
                    let _ = self.db_tx.try_send(DbCommand::ReportMailboxState {
                        account_id: self.account_id(),
                        mailbox,
                        uid_validity: mailbox_state.uid_validity,
                        uid_next: mailbox_state.uid_next,
                    });
                }
                ImapEvent::Body { uid, html, attachments, req_id } => {
                    if req_id == self.current_body_req
                        && self.selected_uid == Some(uid)
                        && self.search_results.is_none()
                    {
                        self.current_message_html = html.clone();
                        self.web_view.load(WebViewSource::Html(html));
                        self.current_attachments = attachments;
                    }
                }
                ImapEvent::DownloadProgress { current, total } => {
                    self.download_progress = Some((current, total));
                    if current == total {
                        self.download_progress = None;
                        self.status = "Download complete".to_string();
                    }
                }
                ImapEvent::MailData { mailbox, header, body } => {
                    let _ = self.db_tx.try_send(DbCommand::IndexMail {
                        account_id: self.account_id(),
                        mailbox,
                        header,
                        body,
                    });
                }
                ImapEvent::MailboxPolled { .. } | ImapEvent::NewHeaders { .. } => {
                    // B10's new-mail signal: already consumed by
                    // `spawn_new_mail_watch` before this event reached the
                    // UI channel at all (it decides whether to poll again /
                    // fetch new envelopes / show a toast). Nothing left here
                    // for the UI to do with either variant.
                }
                ImapEvent::HeadersFrom { mailbox, headers } => {
                    // B3: reply to the `FetchHeadersFrom` sent in
                    // `handle_db_events`'s `SyncPlan::FetchFrom`/`Resync`
                    // arm -- index into the cache now that these envelopes
                    // are in hand. See `ImapCommand::FetchHeadersFrom`'s doc
                    // for why this is a separate event from `NewHeaders`
                    // rather than reusing it.
                    let _ = self.db_tx.try_send(DbCommand::IndexHeaders {
                        account_id: self.account_id(),
                        mailbox,
                        headers,
                    });
                }
                ImapEvent::PollFailed(e) => {
                    // Deliberately not `self.status` -- see the variant's
                    // doc in imap.rs: a background poll failing every 60s
                    // shouldn't overwrite whatever the user is looking at.
                    log::warn!("background new-mail poll failed: {e}");
                }
            }
        }
    }

    fn handle_db_events(&mut self) {
        while let Ok(evt) = self.db_rx.try_recv() {
            match evt {
                DbEvent::SearchResult { headers } => {
                    self.search_results = Some(headers);
                }
                DbEvent::MailFetched { header, body } => {
                    if self.selected_uid == Some(header.uid) {
                        self.current_message_html = body.clone();
                        self.web_view.load(WebViewSource::Html(body));
                    }
                }
                DbEvent::SyncPlan { account_id, mailbox, plan } => {
                    // B3: turn a `FetchFrom`/`Resync` decision into an
                    // actual incremental fetch, so the cache accumulates
                    // message metadata for this mailbox over time instead of
                    // only ever being populated by `BulkDownload`. A
                    // `Resync` already wiped the cache's rows for this
                    // mailbox in `db.rs::report_mailbox_state` by the time
                    // this event arrives -- fetching from UID 1 repopulates
                    // it under the server's new UIDVALIDITY.
                    //
                    // `account_id` isn't used to route this -- there is
                    // exactly one account connected at a time today (see
                    // `EsMailApp::account_id`'s own doc), so it's implicitly
                    // always "the" account; kept on the event for when that
                    // stops being true.
                    let _ = account_id;
                    match plan {
                        db::SyncPlan::UpToDate => {}
                        db::SyncPlan::FetchFrom { first_new_uid } => {
                            let _ = self.imap_tx.try_send(ImapCommand::FetchHeadersFrom { mailbox, first_uid: first_new_uid });
                        }
                        db::SyncPlan::Resync => {
                            let _ = self.imap_tx.try_send(ImapCommand::FetchHeadersFrom { mailbox, first_uid: 1 });
                        }
                    }
                }
                DbEvent::Error(e) => {
                    self.status = format!("DB Error: {}", e);
                }
            }
        }
    }

    fn handle_smtp_events(&mut self) {
        while let Ok(evt) = self.smtp_rx.try_recv() {
            match evt {
                smtp::SmtpEvent::Sent => {
                    // The compose window closes on success; a failure (the
                    // Error arm below) leaves it open with the typed text
                    // intact instead, so nothing is lost -- see smtp.rs's
                    // module docs on why that's a deliberately smaller
                    // promise than a real retry queue.
                    self.compose = None;
                    self.compose_status.clear();
                    self.status = "Message sent".to_string();
                }
                smtp::SmtpEvent::Error(e) => {
                    self.compose_status = format!("Send failed: {e}");
                }
            }
        }
    }

    /// Identifies the connected account to `db.rs`, in the same
    /// `username@host` shape [`AccountConfig::new`] uses for its `id` — so
    /// the cache keys line up with the saved-accounts list even though this
    /// is derived from the live login form rather than looked up from
    /// `self.config`.
    fn account_id(&self) -> String {
        format!("{}@{}", self.username, self.host)
    }

    /// A fresh request id for `FetchHeaders`/`FetchBody`, mechanically
    /// distinct from the last one handed out.
    fn next_req_id(&mut self) -> u64 {
        self.next_req_id += 1;
        self.next_req_id
    }

    /// Send `FetchHeaders`, recording its request id as the only one whose
    /// reply `handle_imap_events` will still accept.
    fn fetch_headers(&mut self, mailbox: String, page: u32) {
        let req_id = self.next_req_id();
        self.current_headers_req = req_id;
        let _ = self.imap_tx.try_send(ImapCommand::FetchHeaders { mailbox, page, req_id });
    }

    /// Send `FetchBody`, recording its request id the same way `fetch_headers` does.
    fn fetch_body(&mut self, mailbox: String, uid: u32) {
        let req_id = self.next_req_id();
        self.current_body_req = req_id;
        let _ = self.imap_tx.try_send(ImapCommand::FetchBody { mailbox, uid, req_id });
    }

    /// Fill the login form from a saved account and pull its password back
    /// out of the OS keyring, if there is one.
    fn select_account(&mut self, account: &AccountConfig) {
        self.host = account.imap_host.clone();
        self.port = account.imap_port.to_string();
        self.username = account.username.clone();
        self.smtp_host = account.smtp_host.clone();
        self.smtp_port = account.smtp_port.to_string();
        self.password = secrets::get_password(&account.id, "imap")
            .map(|s| secrecy::ExposeSecret::expose_secret(&s).to_string())
            .unwrap_or_default();
    }

    /// Persist the account currently in the login form: upsert it into
    /// `config.toml` and its password into the OS keyring (under both
    /// `"imap"` and `"smtp"` — B7 sends with the same credentials, since
    /// `AccountConfig::username` is documented as used for both). Called
    /// once a connection actually succeeds, not on every keystroke or click.
    fn persist_current_account(&mut self) {
        let mut account = AccountConfig::new(
            self.username.clone(),
            self.host.clone(),
            self.port.parse().unwrap_or(993),
            self.username.clone(),
        );
        // AccountConfig::new only guesses smtp_host/smtp_port; the login
        // form's fields (pre-filled from that guess, but editable) win.
        if !self.smtp_host.is_empty() {
            account.smtp_host = self.smtp_host.clone();
        }
        if let Ok(port) = self.smtp_port.parse() {
            account.smtp_port = port;
        }
        let password = SecretString::from(self.password.clone());
        if let Err(e) = secrets::set_password(&account.id, "imap", &password) {
            log::warn!("could not save IMAP password to the OS keyring: {e}");
        }
        if let Err(e) = secrets::set_password(&account.id, "smtp", &password) {
            log::warn!("could not save SMTP password to the OS keyring: {e}");
        }
        self.config.upsert_account(account);
        if let Err(e) = self.config.save() {
            log::warn!("could not persist account config: {e}");
        }
    }

    /// Build the SMTP account `smtp.rs` needs to send, from the current
    /// login form and saved keyring password. `None` if there's no SMTP
    /// password saved yet — e.g. the very first connection, before
    /// [`EsMailApp::persist_current_account`] has ever run for this account.
    fn smtp_account(&self) -> Option<smtp::SmtpAccount> {
        let account_id = self.account_id();
        let password = secrets::get_password(&account_id, "smtp")?;
        Some(smtp::SmtpAccount {
            host: self.smtp_host.clone(),
            port: self.smtp_port.parse().unwrap_or(465),
            tls: config::TlsMode::Ssl,
            username: self.username.clone(),
            password,
            from_address: self.username.clone(),
        })
    }

    /// Draws the compose window when `self.compose` is `Some`, and handles
    /// its Send/Attach/Discard buttons. A separate top-level `egui::Window`
    /// rather than part of the main layout — B7 says "compose window", and
    /// this can stay open (or get discarded) independent of what the user
    /// does with the message list behind it.
    fn show_compose_window(&mut self, ctx: &egui::Context) {
        let Some(compose) = &mut self.compose else {
            return;
        };

        let mut open = true;
        let mut send_clicked = false;
        let mut discard_clicked = false;
        egui::Window::new("Compose")
            .open(&mut open)
            .default_size([480.0, 420.0])
            .show(ctx, |ui| {
                egui::Grid::new("compose_grid").num_columns(2).show(ui, |ui| {
                    ui.label("To:");
                    ui.add(egui::TextEdit::singleline(&mut compose.to).desired_width(f32::INFINITY));
                    ui.end_row();

                    ui.label("Cc:");
                    ui.add(egui::TextEdit::singleline(&mut compose.cc).desired_width(f32::INFINITY));
                    ui.end_row();

                    ui.label("Bcc:");
                    ui.add(egui::TextEdit::singleline(&mut compose.bcc).desired_width(f32::INFINITY));
                    ui.end_row();

                    ui.label("Subject:");
                    ui.add(egui::TextEdit::singleline(&mut compose.subject).desired_width(f32::INFINITY));
                    ui.end_row();
                });

                ui.separator();

                if !compose.attachments.is_empty() {
                    ui.horizontal_wrapped(|ui| {
                        for (filename, data) in &compose.attachments {
                            ui.label(format!("{filename} ({})", format_size(data.len())));
                        }
                    });
                }
                if ui.button("Attach file…").clicked() {
                    if let Some(path) = rfd::FileDialog::new().pick_file() {
                        match std::fs::read(&path) {
                            Ok(data) => {
                                let filename = path
                                    .file_name()
                                    .map(|n| n.to_string_lossy().into_owned())
                                    .unwrap_or_else(|| "attachment".to_string());
                                compose.attachments.push((filename, data));
                            }
                            Err(e) => {
                                self.compose_status = format!("Could not read {}: {e}", path.display());
                            }
                        }
                    }
                }

                ui.add_sized(
                    ui.available_size() - egui::vec2(0.0, 60.0),
                    egui::TextEdit::multiline(&mut compose.body),
                );

                ui.horizontal(|ui| {
                    if ui.button("Send").clicked() {
                        send_clicked = true;
                    }
                    if ui.button("Discard").clicked() {
                        discard_clicked = true;
                    }
                    if !self.compose_status.is_empty() {
                        ui.label(egui::RichText::new(&self.compose_status).color(egui::Color32::RED));
                    }
                });
            });

        if send_clicked {
            match self.smtp_account() {
                Some(account) => {
                    let compose = self.compose.clone().expect("just matched Some above");
                    let _ = self.smtp_tx.try_send(smtp::SmtpCommand::Send { account, compose });
                    self.compose_status = "Sending…".to_string();
                }
                None => {
                    self.compose_status =
                        "No SMTP password on file yet — connect once via IMAP first.".to_string();
                }
            }
        }
        if discard_clicked || !open {
            self.compose = None;
            self.compose_status.clear();
        }
    }
}

/// Windows only (B10): tray icon polling + minimize-to-tray. Kept in its own
/// `impl` block, called only from `EsMailApp::logic`, so the cfg-gating
/// needed to keep this out of non-Windows builds stays contained to one
/// place instead of scattered through the main `ui()`/`impl EsMailApp` code.
#[cfg(target_os = "windows")]
impl EsMailApp {
    fn handle_tray(&mut self, ctx: &egui::Context) {
        let Some(tray) = &self.tray else { return };

        for action in tray.poll_actions() {
            match action {
                tray::TrayAction::Show => {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                }
                tray::TrayAction::Quit => {
                    self.exit_requested = true;
                    // Hidden windows don't organically generate another
                    // close-request -- nothing is clicking their (invisible)
                    // close button -- so ask for one explicitly. The check
                    // below sees `exit_requested` and lets it through rather
                    // than redirecting it to "hide to tray" again.
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
        }

        // The redirect: a first close-request (the user clicked the window's
        // own close button) is canceled and turned into "hide instead",
        // *unless* it was `self.exit_requested` that triggered this request
        // (tray Quit), in which case letting it proceed is the point.
        if ctx.input(|i| i.viewport().close_requested()) && !self.exit_requested {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }

        // `logic()` (unlike `ui()`) keeps running while the window is
        // hidden, but only when something requests a repaint -- nothing
        // does that for us just because a tray click landed in `tray-icon`'s
        // own event channel, so ask again here to keep polling it promptly.
        ctx.request_repaint_after(std::time::Duration::from_millis(250));
    }
}

impl eframe::App for EsMailApp {
    /// Called every frame `ui()` is, *and* while the window is hidden as
    /// long as a repaint keeps getting requested (see `handle_tray`'s last
    /// line) -- unlike `ui()`, which eframe skips entirely while hidden.
    /// That's the whole mechanism B10's tray support depends on: draining
    /// tray-icon/menu clicks and the close-to-tray redirect both need to
    /// keep working after the main window is gone, so they live here rather
    /// than in `ui()`. New-mail polling and toast notifications do *not*
    /// need to be here -- see `spawn_new_mail_watch`, a plain tokio task
    /// that runs independent of both `logic()` and `ui()`.
    #[cfg(target_os = "windows")]
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.handle_tray(ctx);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Drive Servo once per frame, independent of how many views are drawn.
        self.web_view_host.spin();

        self.screenshotter.update(ui.ctx());

        if self.preview {
            egui::CentralPanel::default().show_inside(ui, |ui| {
                for event in self.web_view.show(ui) {
                    if let egui_servo_webview::WebViewEvent::LinkClicked(url) = event {
                        log::info!("preview: link clicked -> {url}");
                    }
                }
            });
            return;
        }

        self.handle_imap_events();
        self.handle_db_events();
        self.handle_smtp_events();

        if self.is_connected {
            egui::MenuBar::new().ui(ui, |ui| {
                ui.menu_button("File", |ui| {
                    if ui.button("Download All (This Mailbox)").clicked() {
                        let _ = self.imap_tx.try_send(ImapCommand::BulkDownload { mailbox: self.selected_mailbox.clone() });
                        ui.close();
                    }
                    ui.separator();
                    if ui.button("Logout").clicked() {
                        self.is_connected = false;
                        self.headers.clear();
                        self.selected_uid = None;
                        self.status = "Logged out".to_string();
                        self.web_view.load(WebViewSource::Html("<h1>Logged out</h1>".to_string()));
                        ui.close();
                    }
                });
                if ui.button("New Message").clicked() {
                    self.compose = Some(compose::ComposeState::default());
                    self.compose_status.clear();
                }
            });
        }

        egui::Panel::top("top_panel").show_inside(ui, |ui| {
            ui.horizontal(|ui| {
                ui.heading("esMail");
                ui.separator();
                
                if self.is_connected {
                    ui.label("Search:");
                    let search_resp = ui.add(egui::TextEdit::singleline(&mut self.search_query).hint_text("Enter keywords..."));
                    if search_resp.changed() || (search_resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter))) {
                        // `from:`/`to:`/`subject:`/`body:` and bare text all
                        // become an FTS5 MATCH expression; `since:`/`before:`/
                        // `is:unread`/`has:attachment` parse but aren't
                        // applied yet (see search_query.rs) — a query made
                        // only of those is treated the same as an empty one.
                        match ParsedQuery::parse(&self.search_query).to_fts_match() {
                            Some(fts_query) => {
                                let _ = self.db_tx.try_send(DbCommand::Search {
                                    account_id: self.account_id(),
                                    query: fts_query,
                                    mailbox: Some(self.selected_mailbox.clone()),
                                });
                            }
                            None => {
                                self.search_results = None;
                            }
                        }
                    }
                    if ui.button("Clear").clicked() {
                        self.search_query.clear();
                        self.search_results = None;
                    }
                    ui.separator();
                }

                ui.label(&self.status);
            });
        });

        if !self.is_connected {
            egui::CentralPanel::default().show_inside(ui, |ui| {
                ui.vertical_centered(|ui| {
                    ui.group(|ui| {
                        ui.set_width(300.0);
                        ui.heading("Login");

                        if !self.config.accounts.is_empty() {
                            ui.label("Saved accounts:");
                            let mut to_remove = None;
                            for account in self.config.accounts.clone() {
                                ui.horizontal(|ui| {
                                    if ui.button(&account.display_name).clicked() {
                                        self.select_account(&account);
                                    }
                                    if ui.small_button("x").on_hover_text("Forget this account").clicked() {
                                        to_remove = Some(account.id.clone());
                                    }
                                });
                            }
                            if let Some(id) = to_remove {
                                secrets::delete_password(&id, "imap");
                                self.config.remove_account(&id);
                                if let Err(e) = self.config.save() {
                                    log::warn!("could not persist account removal: {e}");
                                }
                            }
                            ui.separator();
                        }

                        ui.add(egui::TextEdit::singleline(&mut self.host).hint_text("IMAP Host"));
                        ui.add(egui::TextEdit::singleline(&mut self.port).hint_text("Port"));
                        ui.add(egui::TextEdit::singleline(&mut self.username).hint_text("Username"));
                        ui.add(egui::TextEdit::singleline(&mut self.password).password(true).hint_text("Password"));

                        // Guessed by config::derive_smtp_host (imap. -> smtp.)
                        // when this is a brand new account; editable since
                        // that guess is often wrong. Used by B7's Send.
                        ui.add(egui::TextEdit::singleline(&mut self.smtp_host).hint_text("SMTP Host"));
                        ui.add(egui::TextEdit::singleline(&mut self.smtp_port).hint_text("SMTP Port"));

                        if ui.button("Connect").clicked() {
                            self.status = "Connecting...".to_string();
                            let port: u16 = self.port.parse().unwrap_or(993);
                            let cmd = ImapCommand::Connect {
                                host: self.host.clone(),
                                port,
                                username: self.username.clone(),
                                password: self.password.clone().into(),
                            };
                            let _ = self.imap_tx.try_send(cmd);
                            // A separate, dedicated IDLE connection (see
                            // idle_watch's module doc for why it can't share
                            // ImapActor's session) so new-mail detection is
                            // push-based instead of relying only on
                            // spawn_new_mail_watch's poll timer. Started
                            // alongside the normal connect rather than only
                            // after `ImapEvent::Connected` arrives: it does
                            // its own independent login/reconnect and simply
                            // has nothing to push until it succeeds, so there
                            // is no ordering requirement between the two.
                            // Guarded by `idle_watch_started` -- see that
                            // field's doc -- so clicking Connect more than
                            // once can't spawn more than one.
                            if !self.idle_watch_started {
                                self.idle_watch_started = true;
                                idle_watch::spawn(
                                    self.host.clone(),
                                    port,
                                    self.username.clone(),
                                    self.password.clone().into(),
                                    NEW_MAIL_POLL_MAILBOX.to_string(),
                                    self.idle_wake_tx.clone(),
                                );
                            }
                        }
                    });
                });
            });
        } else {
            egui::Panel::left("left_panel").resizable(true).default_size(300.0).show_inside(ui, |ui| {
                ui.heading("Mailboxes");
                egui::ScrollArea::vertical().id_salt("mailboxes_scroll").max_height(150.0).show(ui, |ui| {
                    ui.with_layout(egui::Layout::top_down_justified(egui::Align::LEFT), |ui| {
                        // Deferred past the loop for the same reason as the
                        // message list below: fetch_headers needs &mut self,
                        // which can't happen while `mb` still borrows
                        // self.mailboxes.
                        let mut clicked_mailbox = None;
                        for mb in &self.mailboxes {
                            let is_selected = self.selected_mailbox == *mb;
                            if ui.add(egui::Button::selectable(is_selected, mb)).clicked() {
                                clicked_mailbox = Some(mb.clone());
                            }
                        }
                        if let Some(mb) = clicked_mailbox {
                            self.selected_mailbox = mb.clone();
                            self.selected_uid = None;
                            self.current_page = 1;
                            self.fetch_headers(mb, 1);
                        }
                    });
                });
                
                ui.separator();
                let title = if self.search_results.is_some() { "Search Results" } else { "Inbox" };
                ui.horizontal(|ui| {
                    ui.heading(title);
                    if self.search_results.is_none() {
                        if ui.button("Refresh").clicked() {
                            self.fetch_headers(self.selected_mailbox.clone(), self.current_page);
                        }
                    }
                });
                
                if self.search_results.is_none() {
                    egui::Panel::bottom("pagination_panel").show_inside(ui, |ui| {
                        ui.horizontal(|ui| {
                            if ui.button("<").clicked() && self.current_page > 1 {
                                self.current_page -= 1;
                                self.fetch_headers(self.selected_mailbox.clone(), self.current_page);
                            }
                            ui.label(format!("Page {} of {}", self.current_page, self.total_pages));
                            if ui.button(">").clicked() && self.current_page < self.total_pages {
                                self.current_page += 1;
                                self.fetch_headers(self.selected_mailbox.clone(), self.current_page);
                            }
                        });
                    });
                }
                ui.separator();
                egui::ScrollArea::vertical().show(ui, |ui| {
                    ui.with_layout(egui::Layout::top_down_justified(egui::Align::LEFT), |ui| {
                        // `clicked_uid` defers the FetchBody/FetchMail send
                        // until after `list`'s borrow of self.headers /
                        // self.search_results ends below: fetch_body takes
                        // &mut self, which the borrow checker won't allow
                        // while `list` (borrowed from those same fields) is
                        // still alive across the loop.
                        let list = self.search_results.as_ref().unwrap_or(&self.headers);
                        let is_search = self.search_results.is_some();
                        let mut clicked_uid = None;
                        for header in list {
                            let is_selected = self.selected_uid == Some(header.uid);
                            let text = format!("{}\n{}", header.from, header.subject);
                            let resp = ui.add(egui::Button::selectable(is_selected, text));
                            if resp.clicked() {
                                self.selected_uid = Some(header.uid);
                                clicked_uid = Some(header.uid);
                                // A new message defaults to blocked remote
                                // content, same as any other mail client;
                                // "Load remote images" opts back in per view.
                                self.message_view_handler.borrow_mut().allow_remote = false;
                                self.current_attachments.clear();
                                self.web_view.load(WebViewSource::Html("<i>Loading message...</i>".to_string()));
                            }
                        }
                        if let Some(uid) = clicked_uid {
                            if is_search {
                                let _ = self.db_tx.try_send(DbCommand::FetchMail {
                                    account_id: self.account_id(),
                                    mailbox: self.selected_mailbox.clone(),
                                    uid,
                                });
                            } else {
                                self.fetch_body(self.selected_mailbox.clone(), uid);
                            }
                        }
                    });
                });
            });

            if let Some((current, total)) = self.download_progress {
                egui::Panel::bottom("progress_status").show_inside(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(format!("Indexing {}... ", self.selected_mailbox));
                        ui.add(egui::ProgressBar::new(current as f32 / total as f32)
                            .text(format!("{}/{}", current, total)));
                    });
                });
            }

            egui::CentralPanel::default().show_inside(ui, |ui| {
                if let Some(uid) = self.selected_uid {
                    // Cloned rather than borrowed: the Reply/Reply All/
                    // Forward buttons below need `&mut self.compose` while
                    // this is in scope, which can't coexist with a borrow of
                    // `self.headers` (the same reason the mailbox/message
                    // list loops elsewhere in this file defer their sends).
                    if let Some(header) = self.headers.iter().find(|h| h.uid == uid).cloned() {
                        egui::Panel::top("mail_info").show_inside(ui, |ui| {
                            egui::Grid::new("mail_info_grid").num_columns(2).show(ui, |ui| {
                                ui.label(egui::RichText::new("From:").strong());
                                ui.add(egui::Label::new(&header.from).selectable(true));
                                ui.end_row();

                                ui.label(egui::RichText::new("To:").strong());
                                ui.add(egui::Label::new(&header.to).selectable(true));
                                ui.end_row();

                                ui.label(egui::RichText::new("Date:").strong());
                                ui.add(egui::Label::new(&header.date).selectable(true));
                                ui.end_row();

                                ui.label(egui::RichText::new("Subject:").strong());
                                ui.add(egui::Label::new(&header.subject).selectable(true));
                                ui.end_row();
                            });
                            ui.horizontal(|ui| {
                                if ui.button("Reply").clicked() {
                                    self.compose = Some(compose::ComposeState::reply(&header, &self.current_message_html));
                                    self.compose_status.clear();
                                }
                                if ui.button("Reply All").clicked() {
                                    self.compose = Some(compose::ComposeState::reply_all(
                                        &header,
                                        &self.current_message_html,
                                        &self.username,
                                    ));
                                    self.compose_status.clear();
                                }
                                if ui.button("Forward").clicked() {
                                    self.compose = Some(compose::ComposeState::forward(&header, &self.current_message_html));
                                    self.compose_status.clear();
                                }
                            });
                        });
                    }

                    // Every message opens with remote content blocked (see
                    // MessageViewHandler); this is the opt-in per B5. Always
                    // shown rather than only when the message actually has
                    // remote images — knowing whether it does would mean
                    // parsing the HTML again here just to answer that.
                    if !self.message_view_handler.borrow().allow_remote {
                        egui::Panel::top("remote_images_bar").show_inside(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.label("Remote images are blocked for this message.");
                                if ui.button("Load remote images").clicked() {
                                    self.message_view_handler.borrow_mut().allow_remote = true;
                                    // The markup never lost its original
                                    // http(s) URLs (see render.rs) -- a
                                    // reload against the same document is
                                    // enough for the now-unblocked requests
                                    // to actually go out.
                                    self.web_view.reload();
                                }
                            });
                        });
                    }

                    if !self.current_attachments.is_empty() {
                        egui::Panel::top("attachments_bar").show_inside(ui, |ui| {
                            ui.horizontal_wrapped(|ui| {
                                for attachment in &self.current_attachments {
                                    ui.group(|ui| {
                                        ui.label(format!(
                                            "{} — {}, {}",
                                            attachment.filename,
                                            attachment.mime_type,
                                            format_size(attachment.data.len())
                                        ));
                                        if ui.button("Save…").clicked() {
                                            save_attachment(attachment);
                                        }
                                        if ui.button("Open").clicked() {
                                            if let Err(e) = open_attachment(attachment) {
                                                log::warn!("could not open attachment {}: {e}", attachment.filename);
                                            }
                                        }
                                    });
                                }
                            });
                        });
                    }
                }

                let events = self.web_view.show(ui);
                for event in events {
                    if let egui_servo_webview::WebViewEvent::LinkClicked(url) = event {
                        ui.ctx().open_url(egui::OpenUrl::new_tab(url));
                    }
                }
            });
        }

        self.show_compose_window(ui.ctx());
    }
}

/// Mailbox `spawn_new_mail_watch` polls (B10). Hardcoded rather than
/// following `selected_mailbox`: watching whatever mailbox happens to be
/// selected would mean a background task's behavior silently changes based
/// on what the user last clicked in the UI, and would poll nothing at all
/// for a user who is reading a different folder. INBOX is the one mailbox
/// every account has and the one "new mail" conventionally means; see
/// PLAN.md §B10 for the fuller reasoning and what a per-mailbox version
/// would need.
const NEW_MAIL_POLL_MAILBOX: &str = "INBOX";
/// How often `spawn_new_mail_watch` asks `ImapActor` to check
/// [`NEW_MAIL_POLL_MAILBOX`] for new mail.
const NEW_MAIL_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

/// Background watcher for B10 (new-mail notifications). Forwards every
/// `ImapEvent` from `ImapActor` to the UI channel (bumping a repaint) --
/// exactly what the bridging task this replaced did -- while additionally:
///
/// - tracking whether the account is currently connected (from `Connected`/
///   `Disconnected`, which pass through this same stream already),
/// - asking for a [`ImapCommand::PollMailbox`] on [`NEW_MAIL_POLL_INTERVAL`]
///   whenever connected, **and** immediately on every `idle_wake` push (IMAP
///   `IDLE`, via `idle_watch` -- see its module doc) rather than waiting for
///   the timer, so new mail shows up within about as long as the round trip
///   takes instead of up to [`NEW_MAIL_POLL_INTERVAL`] later. The interval
///   timer still runs unconditionally: it is what keeps working if `IDLE`
///   isn't supported by the server, or `idle_watch`'s connection is
///   mid-reconnect, so nothing regresses versus B10's original poll-only
///   behavior -- `IDLE` only ever makes new mail show up *sooner*,
/// - folding each [`ImapEvent::MailboxPolled`] into `notify::update_watermark`
///   and, on a `NewMail` verdict, requesting the envelopes that describe it,
/// - turning the resulting [`ImapEvent::NewHeaders`] into a toast via
///   `notify::build_notification` + [`notify_new_mail`].
///
/// This is a plain tokio task, not anything driven by `EsMailApp::logic`/
/// `ui`, so it keeps running -- and can keep showing toasts -- for as long
/// as the process is alive, independent of whether the main window is
/// visible. That's what "notifications work even with the window closed"
/// means in practice here: the process (and this task) survives a window
/// close because `tray.rs` turns that close into hide-to-tray instead of
/// exit; nothing about *this* function knows or cares whether the window is
/// visible.
///
/// The in-memory UID watermark this keeps is deliberately not `db.rs`'s
/// `sync_decision`/cache -- see `notify.rs`'s module doc for why -- and is
/// deliberately not a field on `EsMailApp`: keeping it as a local in this
/// task's own async block means no other code can accidentally read or
/// reset it, and it needs no `Send`/lock story since it never leaves this
/// task.
fn spawn_new_mail_watch(
    mut actor_events: mpsc::Receiver<ImapEvent>,
    ui_events: mpsc::Sender<ImapEvent>,
    imap_tx: mpsc::Sender<ImapCommand>,
    ctx: egui::Context,
    mut idle_wake: mpsc::Receiver<idle_watch::MailboxChanged>,
) {
    tokio::spawn(async move {
        let mut connected = false;
        let mut watermark: Option<notify::MailWatermark> = None;
        let mut poll_interval = tokio::time::interval(NEW_MAIL_POLL_INTERVAL);
        poll_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // Disabled once `idle_wake` closes (which nothing currently does --
        // `idle_watch::spawn`'s task loops forever -- but a channel that
        // keeps returning `None` would otherwise busy-loop this `select!`)
        // so the timer-only path keeps working even in that case.
        let mut idle_wake_open = true;

        loop {
            tokio::select! {
                evt = actor_events.recv() => {
                    let Some(evt) = evt else { break };
                    match &evt {
                        ImapEvent::Connected => {
                            connected = true;
                            // A fresh connection (or reconnection) starts a
                            // new baseline -- see `notify::update_watermark`'s
                            // doc for why the first observation after one
                            // must never itself be reported as "new mail".
                            watermark = None;
                        }
                        ImapEvent::Disconnected => connected = false,
                        ImapEvent::MailboxPolled { mailbox, state } if mailbox == NEW_MAIL_POLL_MAILBOX => {
                            let (next, update) = notify::update_watermark(
                                watermark,
                                notify::MailWatermark {
                                    uid_validity: state.uid_validity,
                                    uid_next: state.uid_next,
                                },
                            );
                            watermark = Some(next);
                            if let notify::WatermarkUpdate::NewMail { first_new_uid, .. } = update {
                                let _ = imap_tx.try_send(ImapCommand::FetchNewHeaders {
                                    mailbox: NEW_MAIL_POLL_MAILBOX.to_string(),
                                    first_uid: first_new_uid,
                                });
                            }
                        }
                        ImapEvent::NewHeaders { mailbox, headers } if mailbox == NEW_MAIL_POLL_MAILBOX => {
                            if let Some((title, body)) = notify::build_notification(headers) {
                                notify_new_mail(&title, &body);
                            }
                        }
                        _ => {}
                    }
                    if ui_events.send(evt).await.is_err() {
                        break; // EsMailApp is gone; nothing left to forward to.
                    }
                    ctx.request_repaint();
                }
                _ = poll_interval.tick(), if connected => {
                    let _ = imap_tx.try_send(ImapCommand::PollMailbox {
                        mailbox: NEW_MAIL_POLL_MAILBOX.to_string(),
                    });
                }
                woke = idle_wake.recv(), if idle_wake_open => {
                    match woke {
                        Some(idle_watch::MailboxChanged) if connected => {
                            let _ = imap_tx.try_send(ImapCommand::PollMailbox {
                                mailbox: NEW_MAIL_POLL_MAILBOX.to_string(),
                            });
                        }
                        Some(idle_watch::MailboxChanged) => {
                            // A push arrived while `ImapActor`'s own session
                            // is disconnected/reconnecting -- nothing to poll
                            // with right now; the timer (once `connected`
                            // again) or the next push will catch it.
                        }
                        None => idle_wake_open = false,
                    }
                }
            }
        }
    });
}

/// Show a new-mail toast on Windows; elsewhere, just log it. B10 is
/// Windows-only (see PLAN.md §B10) -- this is the one place that
/// distinction is made, so `spawn_new_mail_watch` above doesn't need its own
/// `#[cfg]`.
fn notify_new_mail(title: &str, body: &str) {
    #[cfg(target_os = "windows")]
    tray::show_new_mail_toast(title, body);
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (title, body);
        log::info!("new mail: {title} -- {body} (desktop notifications are Windows-only, see PLAN.md §B10)");
    }
}

/// Save-as, via a native file picker pre-filled with the attachment's own
/// name. Does nothing if the user cancels the dialog; a write failure is
/// logged rather than surfaced (mirroring the "log, don't crash the UI over
/// it" treatment other best-effort I/O gets in this file).
fn save_attachment(attachment: &render::Attachment) {
    let Some(path) = rfd::FileDialog::new().set_file_name(&attachment.filename).save_file() else {
        return;
    };
    if let Err(e) = std::fs::write(&path, &attachment.data) {
        log::warn!("could not save attachment to {}: {e}", path.display());
    }
}

/// Open-with: write the attachment to a temp file (there is no path for it
/// yet — it only exists as bytes in memory) and hand that to the OS's
/// default handler for its type. The temp file is left behind rather than
/// cleaned up immediately, since the opened application may still be reading
/// it after this call returns.
fn open_attachment(attachment: &render::Attachment) -> std::io::Result<()> {
    let dir = std::env::temp_dir().join("esmail-attachments");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(safe_attachment_filename(&attachment.filename));
    std::fs::write(&path, &attachment.data)?;
    opener::open(&path).map_err(|e| std::io::Error::other(e.to_string()))
}

/// `filename` comes straight from the message's own
/// Content-Disposition/Content-Type header — an attacker-controlled sender's
/// mail. Taking only the final path component (and falling back to a fixed
/// name if that leaves nothing usable) keeps a crafted `"../../../whatever"`
/// or an absolute path from writing outside the caller's chosen directory,
/// since `Path::join` would otherwise honor either verbatim.
///
/// Splits on `/` *and* `\` manually rather than using `std::path::Path`:
/// `Path`'s separator handling is host-OS-dependent, so on a Linux build
/// `Path::new(r"C:\Windows\System32\evil.dll").file_name()` treats the
/// whole string as one component (`\` isn't a separator on Unix) and
/// returns it unstripped. A sender-controlled filename is untrusted
/// regardless of which OS esmail happens to be running on, so the
/// stripping has to be too.
fn safe_attachment_filename(filename: &str) -> String {
    match filename.rsplit(['/', '\\']).next() {
        Some(name) if !name.is_empty() && name != "." && name != ".." => name.to_string(),
        _ => "attachment".to_string(),
    }
}

/// A human-readable size, e.g. `"4.2 KB"`. Only goes up to MB since a mail
/// attachment in the GB range would be unusual enough to want the exact byte
/// count anyway.
fn format_size(bytes: usize) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    let bytes = bytes as f64;
    if bytes >= MB {
        format!("{:.1} MB", bytes / MB)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes / KB)
    } else {
        format!("{} B", bytes as u64)
    }
}

#[tokio::main]
async fn main() -> eframe::Result {
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1280.0, 720.0]),
        ..Default::default()
    };

    eframe::run_native(
        "esMail",
        native_options,
        Box::new(|cc| Ok(Box::new(EsMailApp::new(cc)))),
    )
}

/// A page that exercises the parts of the webview we care about for mail:
/// text flow, images, tables, links, forms, and scrolling past the fold.
fn preview_demo_html() -> String {
    r#"<!doctype html>
<meta charset="utf-8">
<style>
  body { font: 16px/1.5 system-ui, sans-serif; margin: 2rem; color: #111; }
  table { border-collapse: collapse; } td, th { border: 1px solid #999; padding: .3rem .6rem; }
  .tall { height: 60vh; background: linear-gradient(#eee, #fff); }
</style>
<h1>esMail webview preview</h1>
<p>Accented text to check character encoding: <b>&eacute;&agrave;&uuml;&ccedil;</b> &euro; &mdash; &ldquo;quoted&rdquo;.</p>
<p><a href="https://example.com/clicked">A link</a> &mdash; clicking it should emit LinkClicked and not navigate.</p>
<table><tr><th>From</th><th>Subject</th></tr><tr><td>a@b.c</td><td>Hello</td></tr></table>
<p>Type here to check keyboard input: <input type="text" size="30" placeholder="type me"></p>
<div class="tall">Scroll down past this block to check scrolling.</div>
<h2 id="bottom">Bottom of the page</h2>
"#
    .to_string()
}

/// Install the logger, quietening Servo's known-benign chatter by default.
///
/// These are engine-internal and not caused by (or fixable from) the embedder:
///
/// * `webrender::device::gl` warns "Cropping texture upload Box2D((0,0),(0,1))"
///   six times while its GPU cache warms up over the first two paints, and
///   reports missing optimised shader sources.
/// * `profile_traits::mem` warns that the memory profiler thread disconnected,
///   once per component, while Servo tears itself down on drop. This happens
///   even when no webview is ever drawn.
/// * `fontdb` complains about individual malformed fonts installed on the
///   system, which says nothing about this application.
///
/// Setting RUST_LOG overrides all of it, so nothing is permanently hidden.
fn init_logging() {
    const QUIET: &str = "warn,webrender::device::gl=error,profile_traits::mem=error,fontdb=error";

    let filter = std::env::var("RUST_LOG").unwrap_or_else(|_| QUIET.to_string());
    let _ = env_logger::Builder::new().parse_filters(&filter).try_init();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_size_uses_bytes_below_one_kb() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(1023), "1023 B");
    }

    #[test]
    fn format_size_uses_kb_between_one_kb_and_one_mb() {
        assert_eq!(format_size(1024), "1.0 KB");
        assert_eq!(format_size(4300), "4.2 KB");
    }

    #[test]
    fn format_size_uses_mb_at_one_mb_and_above() {
        assert_eq!(format_size(1024 * 1024), "1.0 MB");
        assert_eq!(format_size(5 * 1024 * 1024 + 512 * 1024), "5.5 MB");
    }

    // ── safe_attachment_filename ─────────────────────────────────────────────

    #[test]
    fn safe_attachment_filename_passes_an_ordinary_name_through() {
        assert_eq!(safe_attachment_filename("report.pdf"), "report.pdf");
    }

    #[test]
    fn safe_attachment_filename_strips_relative_traversal() {
        // Regression test: a crafted "../../../whatever" from a malicious
        // sender's Content-Disposition header must not be able to write
        // outside the caller's chosen directory when joined onto it.
        assert_eq!(safe_attachment_filename("../../../evil.exe"), "evil.exe");
        assert_eq!(safe_attachment_filename("../../etc/passwd"), "passwd");
    }

    #[test]
    fn safe_attachment_filename_strips_a_windows_absolute_path() {
        assert_eq!(
            safe_attachment_filename(r"C:\Windows\System32\evil.dll"),
            "evil.dll"
        );
    }

    #[test]
    fn safe_attachment_filename_falls_back_when_nothing_usable_remains() {
        assert_eq!(safe_attachment_filename(""), "attachment");
        assert_eq!(safe_attachment_filename(".."), "attachment");
        assert_eq!(safe_attachment_filename("/"), "attachment");
    }
}

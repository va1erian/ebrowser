mod imap;
mod db;
mod screenshot;
mod config;
mod secrets;
mod search_query;

use egui_servo_webview::{WebView, WebViewConfig, WebViewHost, WebViewSource};
use imap::{ImapActor, ImapCommand, ImapEvent, MailHeader};
use db::{DbActor, DbCommand, DbEvent};
use config::{AccountConfig, Config};
use search_query::ParsedQuery;
use egui_servo_webview::dpi::PhysicalSize;
use secrecy::SecretString;
use tokio::sync::mpsc;

struct EsMailApp {
    // Field order is drop order: the view must be torn down before the engine
    // that backs it, so it stays declared above the host.
    web_view: WebView,
    /// Owns the Servo engine; one per window. Outlives every view.
    web_view_host: WebViewHost,
    screenshotter: screenshot::Screenshotter,
    /// Show only the webview, with no IMAP account. See ESMAIL_PREVIEW.
    preview: bool,
    imap_tx: mpsc::Sender<ImapCommand>,
    imap_rx: mpsc::Receiver<ImapEvent>,
    db_tx: mpsc::Sender<DbCommand>,
    db_rx: mpsc::Receiver<DbEvent>,

    /// Saved accounts (host/port/username; no passwords — those are in the OS
    /// keyring, see `secrets`). Persisted to `config.toml`.
    config: Config,

    // UI state
    host: String,
    port: String,
    username: String,
    password: String,
    status: String,
    is_connected: bool,
    
    mailboxes: Vec<String>,
    selected_mailbox: String,
    
    headers: Vec<MailHeader>,
    selected_uid: Option<u32>,
    current_page: u32,
    total_pages: u32,

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
}

impl EsMailApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        init_logging();
        
        let (imap_cmd_tx, imap_cmd_rx) = mpsc::channel(32);
        let (imap_evt_tx, imap_evt_rx) = mpsc::channel(32);
        
        let (db_cmd_tx, db_cmd_rx) = mpsc::channel(32);
        let (db_evt_tx, db_evt_rx) = mpsc::channel(32);

        let egui_ctx = cc.egui_ctx.clone();
        
        // Wrap IMAP events
        let (tx, mut rx) = mpsc::channel(32);
        let ctx_clone = egui_ctx.clone();
        tokio::spawn(async move {
            while let Some(evt) = rx.recv().await {
                let _ = imap_evt_tx.send(evt).await;
                ctx_clone.request_repaint();
            }
        });
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
        let (host_str, port_str, username_str, password_str) = match config.accounts.first() {
            Some(account) => {
                let password = secrets::get_password(&account.id, "imap")
                    .map(|s| secrecy::ExposeSecret::expose_secret(&s).to_string())
                    .unwrap_or_default();
                (account.imap_host.clone(), account.imap_port.to_string(), account.username.clone(), password)
            }
            None => ("imap.gmail.com".to_string(), "993".to_string(), String::new(), String::new()),
        };
        let initial_status = "Ready".to_string();

        // One engine per window; the view borrows it to start up. A second view
        // (a compose preview, say) would come from this same host.
        let web_view_host = WebViewHost::from_eframe(cc, PhysicalSize::new(1280, 720))
            .expect("failed to initialise the Servo engine");
        let web_view = web_view_host.new_view(&cc.egui_ctx, WebViewConfig::new(source));

        Self {
            web_view_host,
            web_view,
            screenshotter: screenshot::Screenshotter::from_env(),
            preview: preview.is_some(),
            imap_tx: imap_cmd_tx,
            imap_rx: imap_evt_rx,
            db_tx: db_cmd_tx,
            db_rx: db_evt_rx,
            config,
            host: host_str,
            port: port_str,
            username: username_str,
            password: password_str,
            status: initial_status,
            is_connected: false,
            mailboxes: Vec::new(),
            selected_mailbox: "INBOX".to_string(),
            headers: Vec::new(),
            selected_uid: None,
            current_page: 1,
            total_pages: 1,
            next_req_id: 0,
            current_headers_req: 0,
            current_body_req: 0,
            search_query: String::new(),
            search_results: None,
            download_progress: None,
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
                ImapEvent::Body { uid, html, req_id } => {
                    if req_id == self.current_body_req
                        && self.selected_uid == Some(uid)
                        && self.search_results.is_none()
                    {
                        self.web_view.load(WebViewSource::Html(html));
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
                        self.web_view.load(WebViewSource::Html(body));
                    }
                }
                DbEvent::SyncPlan { account_id, mailbox, plan } => {
                    // Not yet acted on — no incremental fetch is issued in
                    // response to `FetchFrom`/`Resync` today, so BulkDownload
                    // remains the only way to pull more than the current
                    // page. Logged (not surfaced in the UI) so the decision
                    // is at least visible while nothing consumes it yet.
                    // See PLAN.md §B3.
                    log::debug!("sync plan for {account_id}/{mailbox}: {plan:?}");
                }
                DbEvent::Error(e) => {
                    self.status = format!("DB Error: {}", e);
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
        self.password = secrets::get_password(&account.id, "imap")
            .map(|s| secrecy::ExposeSecret::expose_secret(&s).to_string())
            .unwrap_or_default();
    }

    /// Persist the account currently in the login form: upsert it into
    /// `config.toml` and its password into the OS keyring. Called once a
    /// connection actually succeeds, not on every keystroke or click.
    fn persist_current_account(&mut self) {
        let account = AccountConfig::new(
            self.username.clone(),
            self.host.clone(),
            self.port.parse().unwrap_or(993),
            self.username.clone(),
        );
        if let Err(e) = secrets::set_password(&account.id, "imap", &SecretString::from(self.password.clone())) {
            log::warn!("could not save password to the OS keyring: {e}");
        }
        self.config.upsert_account(account);
        if let Err(e) = self.config.save() {
            log::warn!("could not persist account config: {e}");
        }
    }
}

impl eframe::App for EsMailApp {
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

                        if ui.button("Connect").clicked() {
                            self.status = "Connecting...".to_string();
                            let cmd = ImapCommand::Connect {
                                host: self.host.clone(),
                                port: self.port.parse().unwrap_or(993),
                                username: self.username.clone(),
                                password: self.password.clone().into(),
                            };
                            let _ = self.imap_tx.try_send(cmd);
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
                    if let Some(header) = self.headers.iter().find(|h| h.uid == uid) {
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

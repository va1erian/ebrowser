mod imap;
mod db;
mod screenshot;

use egui_servo_webview::{WebView, WebViewConfig, WebViewHost, WebViewSource};
use imap::{ImapActor, ImapCommand, ImapEvent, MailHeader};
use db::{DbActor, DbCommand, DbEvent};
use egui_servo_webview::dpi::PhysicalSize;
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

    // Search and Progress
    search_query: String,
    search_results: Option<Vec<MailHeader>>,
    download_progress: Option<(u32, u32)>,
}

impl EsMailApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let _ = env_logger::try_init();
        
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
        
        let (host_str, port_str, username_str) = load_config().unwrap_or_else(|| {
            ("imap.gmail.com".to_string(), "993".to_string(), "".to_string())
        });
        
        let password_str = "".to_string();
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
                    save_config(&self.host, &self.port, &self.username);
                    let _ = self.imap_tx.try_send(ImapCommand::FetchMailboxes);
                    let _ = self.imap_tx.try_send(ImapCommand::FetchHeaders { mailbox: self.selected_mailbox.clone(), page: 1 });
                }
                ImapEvent::Error(e) => {
                    self.status = format!("Error: {}", e);
                }
                ImapEvent::Mailboxes(mbs) => {
                    self.mailboxes = mbs;
                }
                ImapEvent::Headers { mailbox, headers, page, total_pages } => {
                    if mailbox == self.selected_mailbox {
                        self.headers = headers;
                        self.current_page = page;
                        self.total_pages = total_pages;
                        self.status = format!("Page {} of {}", page, total_pages);
                    }
                }
                ImapEvent::Body { uid, html } => {
                    if self.selected_uid == Some(uid) && self.search_results.is_none() {
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
                    let _ = self.db_tx.try_send(DbCommand::IndexMail { mailbox, header, body });
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
                DbEvent::Error(e) => {
                    self.status = format!("DB Error: {}", e);
                }
            }
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
                    let egui_servo_webview::WebViewEvent::LinkClicked(url) = event;
                    log::info!("preview: link clicked -> {url}");
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
                        if self.search_query.is_empty() {
                            self.search_results = None;
                        } else {
                            let _ = self.db_tx.try_send(DbCommand::Search { 
                                query: self.search_query.clone(), 
                                mailbox: Some(self.selected_mailbox.clone()) 
                            });
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
                        ui.add(egui::TextEdit::singleline(&mut self.host).hint_text("IMAP Host"));
                        ui.add(egui::TextEdit::singleline(&mut self.port).hint_text("Port"));
                        ui.add(egui::TextEdit::singleline(&mut self.username).hint_text("Username"));
                        ui.add(egui::TextEdit::singleline(&mut self.password).password(true).hint_text("Password"));
                        
                        if ui.button("Connect").clicked() {
                            self.status = "Connecting...".to_string();
                            save_config(&self.host, &self.port, &self.username);
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
                        for mb in &self.mailboxes {
                            let is_selected = self.selected_mailbox == *mb;
                            if ui.add(egui::Button::selectable(is_selected, mb)).clicked() {
                            self.selected_mailbox = mb.clone();
                            self.selected_uid = None;
                            self.current_page = 1;
                            let _ = self.imap_tx.try_send(ImapCommand::FetchHeaders { mailbox: mb.clone(), page: 1 });
                        }
                    }
                    });
                });
                
                ui.separator();
                let title = if self.search_results.is_some() { "Search Results" } else { "Inbox" };
                ui.horizontal(|ui| {
                    ui.heading(title);
                    if self.search_results.is_none() {
                        if ui.button("Refresh").clicked() {
                            let _ = self.imap_tx.try_send(ImapCommand::FetchHeaders { mailbox: self.selected_mailbox.clone(), page: self.current_page });
                        }
                    }
                });
                
                if self.search_results.is_none() {
                    egui::Panel::bottom("pagination_panel").show_inside(ui, |ui| {
                        ui.horizontal(|ui| {
                            if ui.button("<").clicked() && self.current_page > 1 {
                                self.current_page -= 1;
                                let _ = self.imap_tx.try_send(ImapCommand::FetchHeaders { mailbox: self.selected_mailbox.clone(), page: self.current_page });
                            }
                            ui.label(format!("Page {} of {}", self.current_page, self.total_pages));
                            if ui.button(">").clicked() && self.current_page < self.total_pages {
                                self.current_page += 1;
                                let _ = self.imap_tx.try_send(ImapCommand::FetchHeaders { mailbox: self.selected_mailbox.clone(), page: self.current_page });
                            }
                        });
                    });
                }
                ui.separator();
                egui::ScrollArea::vertical().show(ui, |ui| {
                    ui.with_layout(egui::Layout::top_down_justified(egui::Align::LEFT), |ui| {
                        let list = self.search_results.as_ref().unwrap_or(&self.headers);
                        for header in list {
                            let is_selected = self.selected_uid == Some(header.uid);
                            let text = format!("{}\n{}", header.from, header.subject);
                            let resp = ui.add(egui::Button::selectable(is_selected, text));
                            if resp.clicked() {
                                self.selected_uid = Some(header.uid);
                                if self.search_results.is_some() {
                                    let _ = self.db_tx.try_send(DbCommand::FetchMail { 
                                        mailbox: self.selected_mailbox.clone(), 
                                        uid: header.uid 
                                    });
                                } else {
                                    let _ = self.imap_tx.try_send(ImapCommand::FetchBody { 
                                        mailbox: self.selected_mailbox.clone(), 
                                        uid: header.uid 
                                    });
                                }
                                self.web_view.load(WebViewSource::Html("<i>Loading message...</i>".to_string()));
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
                    let egui_servo_webview::WebViewEvent::LinkClicked(url) = event;
                    ui.ctx().open_url(egui::OpenUrl::new_tab(url));
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

fn get_config_path() -> String {
    std::env::var("APPDATA").unwrap_or_else(|_| ".".to_string()) + "/esmail_config.txt"
}

fn load_config() -> Option<(String, String, String)> {
    if let Ok(content) = std::fs::read_to_string(get_config_path()) {
        let lines: Vec<&str> = content.lines().collect();
        if lines.len() >= 3 {
            return Some((lines[0].trim().to_string(), lines[1].trim().to_string(), lines[2].trim().to_string()));
        }
    }
    None
}

fn save_config(host: &str, port: &str, username: &str) {
    let content = format!("{}\n{}\n{}", host, port, username);
    let _ = std::fs::write(get_config_path(), content);
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

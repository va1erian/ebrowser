mod imap;

use es_webview::{ESWebView, WebViewSource};
use imap::{ImapActor, ImapCommand, ImapEvent, MailHeader};
use tokio::sync::mpsc;
struct EsMailApp {
    web_view: ESWebView,
    imap_tx: mpsc::Sender<ImapCommand>,
    imap_rx: mpsc::Receiver<ImapEvent>,
    
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
}

impl EsMailApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let _ = env_logger::try_init();
        
        let (cmd_tx, cmd_rx) = mpsc::channel(32);
        let (evt_tx, evt_rx) = mpsc::channel(32);
        
        let egui_ctx = cc.egui_ctx.clone();
        
        // Wrap the event sender so it triggers a repaint
        let (tx, mut rx) = mpsc::channel(32);
        tokio::spawn(async move {
            while let Some(evt) = rx.recv().await {
                let _ = evt_tx.send(evt).await;
                egui_ctx.request_repaint();
            }
        });

        ImapActor::spawn(cmd_rx, tx);

        let source = WebViewSource::Html("<h1>Welcome to esMail</h1><p>Connect to your IMAP account to start reading.</p>".to_string());
        
        let (host_str, port_str, username_str) = load_config().unwrap_or_else(|| {
            ("imap.gmail.com".to_string(), "993".to_string(), "".to_string())
        });
        
        let password_str = "".to_string();
        
        let initial_status = "Ready".to_string();

        Self {
            web_view: ESWebView::new(cc, source),
            imap_tx: cmd_tx,
            imap_rx: evt_rx,
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
                    if self.selected_uid == Some(uid) {
                        self.web_view.load(WebViewSource::Html(html));
                    }
                }
            }
        }
    }
}

impl eframe::App for EsMailApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.handle_imap_events();

        egui::Panel::top("top_panel").show_inside(ui, |ui| {
            ui.horizontal(|ui| {
                ui.heading("esMail");
                ui.separator();
                ui.label(&self.status);
                if self.is_connected {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Logout").clicked() {
                            self.is_connected = false;
                            self.headers.clear();
                            self.selected_uid = None;
                            self.status = "Logged out".to_string();
                            self.web_view.load(WebViewSource::Html("<h1>Logged out</h1>".to_string()));
                        }
                    });
                }
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
                
                ui.horizontal(|ui| {
                    ui.heading("Inbox");
                    if ui.button("Refresh").clicked() {
                        let _ = self.imap_tx.try_send(ImapCommand::FetchHeaders { mailbox: self.selected_mailbox.clone(), page: self.current_page });
                    }
                });
                
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
                ui.separator();
                egui::ScrollArea::vertical().show(ui, |ui| {
                    ui.with_layout(egui::Layout::top_down_justified(egui::Align::LEFT), |ui| {
                        for header in &self.headers {
                            let is_selected = self.selected_uid == Some(header.uid);
                            let text = format!("{}\n{}", header.from, header.subject);
                            let resp = ui.add(egui::Button::selectable(is_selected, text));
                            if resp.clicked() {
                            self.selected_uid = Some(header.uid);
                            let _ = self.imap_tx.try_send(ImapCommand::FetchBody { mailbox: self.selected_mailbox.clone(), uid: header.uid });
                            self.web_view.load(WebViewSource::Html("<i>Loading message...</i>".to_string()));
                        }
                    }
                    });
                });
            });

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
                    let es_webview::ESWebViewEvent::LinkClicked(url) = event;
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

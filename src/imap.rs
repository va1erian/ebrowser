use tokio::sync::mpsc;
use tokio::net::TcpStream;
use tokio_native_tls::TlsStream;
use tokio_native_tls::native_tls::TlsConnector;
use mailparse::parse_mail;
use secrecy::{SecretString, ExposeSecret};
use futures::StreamExt;
use anyhow::anyhow;

#[derive(Debug, Clone)]
pub struct MailHeader {
    pub uid: u32,
    pub subject: String,
    pub from: String,
    pub to: String,
    pub date: String,
}

pub enum ImapCommand {
    Connect {
        host: String,
        port: u16,
        username: String,
        password: SecretString,
    },
    FetchMailboxes,
    FetchHeaders { mailbox: String, page: u32 },
    FetchBody { mailbox: String, uid: u32 },
}

pub enum ImapEvent {
    Connected,
    Error(String),
    Mailboxes(Vec<String>),
    Headers { mailbox: String, headers: Vec<MailHeader>, page: u32, total_pages: u32 },
    Body { uid: u32, html: String },
}

pub struct ImapActor {
    cmd_rx: mpsc::Receiver<ImapCommand>,
    event_tx: mpsc::Sender<ImapEvent>,
    session: Option<async_imap::Session<TlsStream<TcpStream>>>,
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
                            let _ = self.event_tx.send(ImapEvent::Connected).await;
                        }
                        Err(e) => {
                            let _ = self.event_tx.send(ImapEvent::Error(e.to_string())).await;
                        }
                    }
                }
                ImapCommand::FetchMailboxes => {
                    if let Some(ref mut session) = self.session {
                        match Self::fetch_mailboxes(session).await {
                            Ok(mbs) => {
                                let _ = self.event_tx.send(ImapEvent::Mailboxes(mbs)).await;
                            }
                            Err(e) => {
                                let _ = self.event_tx.send(ImapEvent::Error(e.to_string())).await;
                            }
                        }
                    }
                }
                ImapCommand::FetchHeaders { mailbox, page } => {
                    if let Some(ref mut session) = self.session {
                        match Self::fetch_headers(session, &mailbox, page).await {
                            Ok((headers, total_pages)) => {
                                let _ = self.event_tx.send(ImapEvent::Headers { mailbox, headers, page, total_pages }).await;
                            }
                            Err(e) => {
                                let _ = self.event_tx.send(ImapEvent::Error(e.to_string())).await;
                            }
                        }
                    } else {
                        let _ = self.event_tx.send(ImapEvent::Error("Not connected".into())).await;
                    }
                }
                ImapCommand::FetchBody { mailbox, uid } => {
                    if let Some(ref mut session) = self.session {
                        match Self::fetch_body(session, &mailbox, uid).await {
                            Ok(html) => {
                                let _ = self.event_tx.send(ImapEvent::Body { uid, html }).await;
                            }
                            Err(e) => {
                                let _ = self.event_tx.send(ImapEvent::Error(e.to_string())).await;
                            }
                        }
                    }
                }
            }
        }
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

    async fn fetch_headers(session: &mut async_imap::Session<TlsStream<TcpStream>>, mailbox_name: &str, page: u32) -> anyhow::Result<(Vec<MailHeader>, u32)> {
        let mailbox = session.examine(mailbox_name).await?;
        
        let total = mailbox.exists;
        if total == 0 {
            return Ok((Vec::new(), 0));
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
        Ok((headers, total_pages))
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
}

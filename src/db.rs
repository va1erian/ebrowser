use rusqlite::{params, Connection};
use tokio::sync::mpsc;
use crate::imap::MailHeader;

pub enum DbCommand {
    IndexMail {
        mailbox: String,
        header: MailHeader,
        body: String,
    },
    Search {
        query: String,
        mailbox: Option<String>,
    },
    FetchMail {
        mailbox: String,
        uid: u32,
    },
}

pub enum DbEvent {
    SearchResult {
        headers: Vec<MailHeader>,
    },
    MailFetched {
        header: MailHeader,
        body: String,
    },
    Error(String),
}

pub struct DbActor {
    cmd_rx: mpsc::Receiver<DbCommand>,
    event_tx: mpsc::Sender<DbEvent>,
    conn: Connection,
}

impl DbActor {
    pub fn spawn(
        cmd_rx: mpsc::Receiver<DbCommand>,
        event_tx: mpsc::Sender<DbEvent>,
    ) {
        tokio::task::spawn_blocking(move || {
            let conn = match Connection::open("mails.db") {
                Ok(c) => c,
                Err(e) => {
                    let _ = event_tx.blocking_send(DbEvent::Error(e.to_string()));
                    return;
                }
            };

            if let Err(e) = Self::init_db(&conn) {
                let _ = event_tx.blocking_send(DbEvent::Error(e.to_string()));
                return;
            }

            let mut actor = DbActor {
                cmd_rx,
                event_tx,
                conn,
            };

            actor.run();
        });
    }

    fn init_db(conn: &Connection) -> rusqlite::Result<()> {
        conn.execute(
            "CREATE VIRTUAL TABLE IF NOT EXISTS mails_fts USING fts5(
                mailbox,
                uid UNINDEXED,
                subject,
                from_addr,
                to_addr,
                date,
                body
            )",
            [],
        )?;
        Ok(())
    }

    fn run(&mut self) {
        while let Some(cmd) = self.cmd_rx.blocking_recv() {
            match cmd {
                DbCommand::IndexMail { mailbox, header, body } => {
                    if let Err(e) = self.index_mail(&mailbox, &header, &body) {
                        let _ = self.event_tx.blocking_send(DbEvent::Error(e.to_string()));
                    }
                }
                DbCommand::Search { query, mailbox } => {
                    match self.search(&query, mailbox) {
                        Ok(headers) => {
                            let _ = self.event_tx.blocking_send(DbEvent::SearchResult { headers });
                        }
                        Err(e) => {
                            let _ = self.event_tx.blocking_send(DbEvent::Error(e.to_string()));
                        }
                    }
                }
                DbCommand::FetchMail { mailbox, uid } => {
                    match self.fetch_mail(&mailbox, uid) {
                        Ok((header, body)) => {
                            let _ = self.event_tx.blocking_send(DbEvent::MailFetched { header, body });
                        }
                        Err(e) => {
                            let _ = self.event_tx.blocking_send(DbEvent::Error(e.to_string()));
                        }
                    }
                }
            }
        }
    }

    fn index_mail(&self, mailbox: &str, header: &MailHeader, body: &str) -> rusqlite::Result<()> {
        // First check if already indexed to avoid duplicates
        let count: u32 = self.conn.query_row(
            "SELECT count(*) FROM mails_fts WHERE mailbox = ? AND uid = ?",
            params![mailbox, header.uid],
            |row| row.get(0),
        )?;

        if count == 0 {
            self.conn.execute(
                "INSERT INTO mails_fts (mailbox, uid, subject, from_addr, to_addr, date, body) VALUES (?, ?, ?, ?, ?, ?, ?)",
                params![
                    mailbox,
                    header.uid,
                    header.subject,
                    header.from,
                    header.to,
                    header.date,
                    body
                ],
            )?;
        }
        Ok(())
    }

    fn search(&self, query: &str, mailbox: Option<String>) -> rusqlite::Result<Vec<MailHeader>> {
        let sql = if let Some(ref mb) = mailbox {
            format!("SELECT uid, subject, from_addr, to_addr, date FROM mails_fts WHERE mailbox = '{}' AND mails_fts MATCH ? ORDER BY rank", mb)
        } else {
            "SELECT uid, subject, from_addr, to_addr, date FROM mails_fts WHERE mails_fts MATCH ? ORDER BY rank".to_string()
        };

        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params![query], |row| {
            Ok(MailHeader {
                uid: row.get(0)?,
                subject: row.get(1)?,
                from: row.get(2)?,
                to: row.get(3)?,
                date: row.get(4)?,
            })
        })?;

        let mut results = Vec::new();
        for row in rows {
            results.push(row?);
        }
        Ok(results)
    }

    fn fetch_mail(&self, mailbox: &str, uid: u32) -> rusqlite::Result<(MailHeader, String)> {
        self.conn.query_row(
            "SELECT subject, from_addr, to_addr, date, body FROM mails_fts WHERE mailbox = ? AND uid = ?",
            params![mailbox, uid],
            |row| {
                Ok((
                    MailHeader {
                        uid,
                        subject: row.get(0)?,
                        from: row.get(1)?,
                        to: row.get(2)?,
                        date: row.get(3)?,
                    },
                    row.get(4)?,
                ))
            },
        )
    }
}

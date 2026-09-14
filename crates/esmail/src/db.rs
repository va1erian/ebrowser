//! Local SQLite cache: message metadata, cached bodies, and a full-text
//! index, keyed by `(account_id, mailbox, uid)`.
//!
//! B3 of PLAN.md. What's here: the relational schema (`mailboxes`,
//! `messages`, `bodies`), an LRU cap on cached bodies, and the pure
//! `sync_decision` this needs to eventually drive incremental sync. What's
//! **not** here yet: anything that actually issues the incremental IMAP
//! fetch a `FetchFrom` decision calls for — `imap.rs` reports the
//! UIDVALIDITY/UIDNEXT it already reads off `session.examine()`, `db.rs`
//! records it and computes the decision, but nothing acts on `FetchFrom` by
//! requesting more messages yet. `BulkDownload` still pulls the whole
//! mailbox every time. See PLAN.md §B3 for why that part waited.

use rusqlite::{params, Connection};
use tokio::sync::mpsc;
use crate::imap::MailHeader;

/// Cap on rows in `bodies` across all accounts/mailboxes; the oldest
/// (by `cached_at`) are evicted once a write pushes past it.
const MAX_CACHED_BODIES: i64 = 2000;

pub enum DbCommand {
    IndexMail {
        account_id: String,
        mailbox: String,
        header: MailHeader,
        body: String,
    },
    Search {
        account_id: String,
        query: String,
        mailbox: Option<String>,
    },
    FetchMail {
        account_id: String,
        mailbox: String,
        uid: u32,
    },
    /// Report what the server said about a mailbox on the most recent
    /// `EXAMINE`/`SELECT` (`imap.rs` already reads `uid_validity`/`uid_next`
    /// off the `Mailbox` it gets back from `session.examine()` — this just
    /// forwards it). Answered with a [`DbEvent::SyncPlan`].
    ReportMailboxState {
        account_id: String,
        mailbox: String,
        uid_validity: u32,
        uid_next: u32,
    },
}

pub enum DbEvent {
    SearchResult { headers: Vec<MailHeader> },
    MailFetched { header: MailHeader, body: String },
    SyncPlan { account_id: String, mailbox: String, plan: SyncPlan },
    Error(String),
}

/// What should happen to bring a mailbox's local cache up to date, given what
/// the server just reported vs. what was last stored for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncPlan {
    /// UIDVALIDITY is unchanged from last time and UIDNEXT didn't move: the
    /// cache already has everything the server has.
    UpToDate,
    /// UIDVALIDITY is unchanged; UIDs in `first..last_known_uidnext` (both
    /// exclusive of `first_new_uid`) may exist on the server but not locally.
    /// `first_new_uid` is the first UID worth fetching.
    FetchFrom { first_new_uid: u32 },
    /// UIDVALIDITY changed since we last saw this mailbox: the server has
    /// reassigned UIDs, so every UID this cache has for it means nothing
    /// anymore. The mailbox's cached messages and bodies were wiped as part
    /// of computing this plan; start a full resync from UID 1.
    Resync,
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

            if let Err(e) = init_schema(&conn) {
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

    fn run(&mut self) {
        while let Some(cmd) = self.cmd_rx.blocking_recv() {
            match cmd {
                DbCommand::IndexMail { account_id, mailbox, header, body } => {
                    if let Err(e) = index_mail(&self.conn, &account_id, &mailbox, &header, &body) {
                        let _ = self.event_tx.blocking_send(DbEvent::Error(e.to_string()));
                    }
                }
                DbCommand::Search { account_id, query, mailbox } => {
                    match search(&self.conn, &account_id, &query, mailbox.as_deref()) {
                        Ok(headers) => {
                            let _ = self.event_tx.blocking_send(DbEvent::SearchResult { headers });
                        }
                        Err(e) => {
                            let _ = self.event_tx.blocking_send(DbEvent::Error(e.to_string()));
                        }
                    }
                }
                DbCommand::FetchMail { account_id, mailbox, uid } => {
                    match fetch_mail(&self.conn, &account_id, &mailbox, uid) {
                        Ok((header, body)) => {
                            let _ = self.event_tx.blocking_send(DbEvent::MailFetched { header, body });
                        }
                        Err(e) => {
                            let _ = self.event_tx.blocking_send(DbEvent::Error(e.to_string()));
                        }
                    }
                }
                DbCommand::ReportMailboxState { account_id, mailbox, uid_validity, uid_next } => {
                    match report_mailbox_state(&self.conn, &account_id, &mailbox, uid_validity, uid_next) {
                        Ok(plan) => {
                            let _ = self.event_tx.blocking_send(DbEvent::SyncPlan { account_id, mailbox, plan });
                        }
                        Err(e) => {
                            let _ = self.event_tx.blocking_send(DbEvent::Error(e.to_string()));
                        }
                    }
                }
            }
        }
    }
}

fn init_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS mailboxes (
            account_id      TEXT NOT NULL,
            mailbox         TEXT NOT NULL,
            uid_validity    INTEGER NOT NULL,
            uid_next        INTEGER NOT NULL,
            highest_modseq  INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY (account_id, mailbox)
        );

        CREATE TABLE IF NOT EXISTS messages (
            account_id  TEXT NOT NULL,
            mailbox     TEXT NOT NULL,
            uid         INTEGER NOT NULL,
            subject     TEXT NOT NULL,
            from_addr   TEXT NOT NULL,
            to_addr     TEXT NOT NULL,
            date        TEXT NOT NULL,
            message_id  TEXT NOT NULL DEFAULT '',
            size        INTEGER NOT NULL DEFAULT 0,
            flags       TEXT NOT NULL DEFAULT '',
            thread_key  TEXT,
            PRIMARY KEY (account_id, mailbox, uid)
        );

        CREATE TABLE IF NOT EXISTS bodies (
            account_id  TEXT NOT NULL,
            mailbox     TEXT NOT NULL,
            uid         INTEGER NOT NULL,
            body        TEXT NOT NULL,
            cached_at   INTEGER NOT NULL,
            PRIMARY KEY (account_id, mailbox, uid)
        );

        CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
            account_id UNINDEXED,
            mailbox UNINDEXED,
            uid UNINDEXED,
            subject,
            from_addr,
            to_addr,
            body
        );
        ",
    )?;
    add_message_id_column_if_missing(conn)
}

/// `messages.message_id` (B7) was added after `messages` itself (B3).
/// `CREATE TABLE IF NOT EXISTS` only creates a table that doesn't exist yet
/// at all — it does nothing to a `messages` table an earlier build of this
/// app already created without the column, which is exactly the local
/// `mails.db` this session's own B3-B6 testing left behind. Without this,
/// every `INSERT INTO messages (..., message_id, ...)` in `index_mail` would
/// fail against that file with "table messages has no column named
/// message_id" the first time a message was indexed.
fn add_message_id_column_if_missing(conn: &Connection) -> rusqlite::Result<()> {
    match conn.execute("ALTER TABLE messages ADD COLUMN message_id TEXT NOT NULL DEFAULT ''", []) {
        Ok(_) => Ok(()),
        // SQLite has no "ALTER TABLE ... ADD COLUMN IF NOT EXISTS"; detect
        // the column already being there by its own error text instead.
        Err(rusqlite::Error::SqliteFailure(_, Some(msg))) if msg.contains("duplicate column name") => Ok(()),
        Err(e) => Err(e),
    }
}

/// Insert or update one message's metadata, cached body, and FTS row. Safe to
/// call repeatedly for the same `(account_id, mailbox, uid)` — every table
/// upserts rather than duplicating.
fn index_mail(
    conn: &Connection,
    account_id: &str,
    mailbox: &str,
    header: &MailHeader,
    body: &str,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO messages (account_id, mailbox, uid, subject, from_addr, to_addr, date, message_id, size)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT (account_id, mailbox, uid) DO UPDATE SET
            subject = excluded.subject,
            from_addr = excluded.from_addr,
            to_addr = excluded.to_addr,
            date = excluded.date,
            message_id = excluded.message_id,
            size = excluded.size",
        params![
            account_id,
            mailbox,
            header.uid,
            header.subject,
            header.from,
            header.to,
            header.date,
            header.message_id,
            body.len() as i64,
        ],
    )?;

    let cached_at = now_unix();
    conn.execute(
        "INSERT INTO bodies (account_id, mailbox, uid, body, cached_at)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT (account_id, mailbox, uid) DO UPDATE SET
            body = excluded.body,
            cached_at = excluded.cached_at",
        params![account_id, mailbox, header.uid, body, cached_at],
    )?;

    // The FTS table has no natural key to upsert on, so replace-by-delete.
    conn.execute(
        "DELETE FROM messages_fts WHERE account_id = ?1 AND mailbox = ?2 AND uid = ?3",
        params![account_id, mailbox, header.uid],
    )?;
    conn.execute(
        "INSERT INTO messages_fts (account_id, mailbox, uid, subject, from_addr, to_addr, body)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![account_id, mailbox, header.uid, header.subject, header.from, header.to, body],
    )?;

    evict_lru_bodies(conn, MAX_CACHED_BODIES)?;
    Ok(())
}

/// Delete the oldest-cached rows in `bodies` until at most `max_rows` remain.
/// `messages`/`messages_fts` are untouched — this only trims the (larger,
/// re-fetchable) cached RFC822 bodies, not the metadata used to render the
/// header list or find things in search.
fn evict_lru_bodies(conn: &Connection, max_rows: i64) -> rusqlite::Result<()> {
    conn.execute(
        "DELETE FROM bodies WHERE rowid IN (
            SELECT rowid FROM bodies ORDER BY cached_at ASC
            LIMIT MAX(0, (SELECT COUNT(*) FROM bodies) - ?1)
        )",
        params![max_rows],
    )?;
    Ok(())
}

fn search(
    conn: &Connection,
    account_id: &str,
    query: &str,
    mailbox: Option<&str>,
) -> rusqlite::Result<Vec<MailHeader>> {
    // `mailbox` is bound as a parameter (not spliced into the SQL string) —
    // the previous version of this query built the WHERE clause with
    // `format!("... mailbox = '{}'", mb)`, which let a mailbox name
    // containing a `'` alter the query. IMAP mailbox names are server-
    // controlled, so this was reachable from an untrusted source.
    let mut stmt = conn.prepare(
        "SELECT m.uid, m.subject, m.from_addr, m.to_addr, m.date, m.message_id
         FROM messages_fts f
         JOIN messages m ON m.account_id = f.account_id
            AND m.mailbox = f.mailbox AND m.uid = f.uid
         WHERE f.account_id = ?1
            AND (?2 IS NULL OR f.mailbox = ?2)
            AND messages_fts MATCH ?3
         ORDER BY f.rank",
    )?;
    let rows = stmt.query_map(params![account_id, mailbox, query], |row| {
        Ok(MailHeader {
            uid: row.get(0)?,
            subject: row.get(1)?,
            from: row.get(2)?,
            to: row.get(3)?,
            date: row.get(4)?,
            message_id: row.get(5)?,
        })
    })?;

    let mut results = Vec::new();
    for row in rows {
        results.push(row?);
    }
    Ok(results)
}

fn fetch_mail(
    conn: &Connection,
    account_id: &str,
    mailbox: &str,
    uid: u32,
) -> rusqlite::Result<(MailHeader, String)> {
    conn.query_row(
        "SELECT m.subject, m.from_addr, m.to_addr, m.date, m.message_id, b.body
         FROM messages m JOIN bodies b
            ON b.account_id = m.account_id AND b.mailbox = m.mailbox AND b.uid = m.uid
         WHERE m.account_id = ?1 AND m.mailbox = ?2 AND m.uid = ?3",
        params![account_id, mailbox, uid],
        |row| {
            Ok((
                MailHeader {
                    uid,
                    subject: row.get(0)?,
                    from: row.get(1)?,
                    to: row.get(2)?,
                    date: row.get(3)?,
                    message_id: row.get(4)?,
                },
                row.get(5)?,
            ))
        },
    )
}

fn report_mailbox_state(
    conn: &Connection,
    account_id: &str,
    mailbox: &str,
    uid_validity: u32,
    uid_next: u32,
) -> rusqlite::Result<SyncPlan> {
    let previous: Option<(u32, u32)> = conn
        .query_row(
            "SELECT uid_validity, uid_next FROM mailboxes WHERE account_id = ?1 AND mailbox = ?2",
            params![account_id, mailbox],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .ok();

    let plan = sync_decision(previous, uid_validity, uid_next);

    if plan == SyncPlan::Resync {
        conn.execute(
            "DELETE FROM messages WHERE account_id = ?1 AND mailbox = ?2",
            params![account_id, mailbox],
        )?;
        conn.execute(
            "DELETE FROM bodies WHERE account_id = ?1 AND mailbox = ?2",
            params![account_id, mailbox],
        )?;
        conn.execute(
            "DELETE FROM messages_fts WHERE account_id = ?1 AND mailbox = ?2",
            params![account_id, mailbox],
        )?;
    }

    conn.execute(
        "INSERT INTO mailboxes (account_id, mailbox, uid_validity, uid_next)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT (account_id, mailbox) DO UPDATE SET
            uid_validity = excluded.uid_validity,
            uid_next = excluded.uid_next",
        params![account_id, mailbox, uid_validity, uid_next],
    )?;

    Ok(plan)
}

/// The pure decision behind [`report_mailbox_state`]: given what was stored
/// last time (`None` the first time this mailbox is ever seen) and what the
/// server just reported, decide what the cache needs.
fn sync_decision(
    previous: Option<(u32, u32)>,
    server_uid_validity: u32,
    server_uid_next: u32,
) -> SyncPlan {
    match previous {
        None => {
            // Never seen this mailbox before: everything up to uid_next - 1
            // is "new" from the cache's point of view.
            if server_uid_next <= 1 {
                SyncPlan::UpToDate
            } else {
                SyncPlan::FetchFrom { first_new_uid: 1 }
            }
        }
        Some((prev_validity, _)) if prev_validity != server_uid_validity => SyncPlan::Resync,
        Some((_, prev_uid_next)) if server_uid_next > prev_uid_next => {
            SyncPlan::FetchFrom { first_new_uid: prev_uid_next }
        }
        Some(_) => SyncPlan::UpToDate,
    }
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_header(uid: u32) -> MailHeader {
        MailHeader {
            uid,
            subject: format!("Subject {uid}"),
            from: "alice@example.com".to_string(),
            to: "bob@example.com".to_string(),
            date: "2026-01-01".to_string(),
            message_id: format!("<msg{uid}@example.com>"),
        }
    }

    fn test_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        conn
    }

    #[test]
    fn init_schema_is_idempotent() {
        // Regression guard for the ALTER TABLE migration: running init twice
        // (e.g. every app startup against the same mails.db) must not error
        // the second time just because the column is already there.
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        init_schema(&conn).unwrap();
    }

    #[test]
    fn init_schema_adds_message_id_to_a_pre_b7_messages_table() {
        // Simulates a mails.db left over from before B7 added the column:
        // a `messages` table that init_schema's CREATE TABLE IF NOT EXISTS
        // alone would never touch, since the table already exists.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE messages (
                account_id TEXT NOT NULL, mailbox TEXT NOT NULL, uid INTEGER NOT NULL,
                subject TEXT NOT NULL, from_addr TEXT NOT NULL, to_addr TEXT NOT NULL,
                date TEXT NOT NULL, size INTEGER NOT NULL DEFAULT 0,
                flags TEXT NOT NULL DEFAULT '', thread_key TEXT,
                PRIMARY KEY (account_id, mailbox, uid)
            )",
        )
        .unwrap();

        init_schema(&conn).unwrap();
        index_mail(&conn, "acc", "INBOX", &test_header(1), "body").unwrap();

        let (header, _) = fetch_mail(&conn, "acc", "INBOX", 1).unwrap();
        assert_eq!(header.message_id, "<msg1@example.com>");
    }

    // ── index_mail / fetch_mail ──────────────────────────────────────────────

    #[test]
    fn index_then_fetch_round_trips() {
        let conn = test_conn();
        index_mail(&conn, "acc", "INBOX", &test_header(1), "<p>hello</p>").unwrap();

        let (header, body) = fetch_mail(&conn, "acc", "INBOX", 1).unwrap();
        assert_eq!(header.uid, 1);
        assert_eq!(header.subject, "Subject 1");
        assert_eq!(body, "<p>hello</p>");
    }

    #[test]
    fn indexing_the_same_uid_twice_updates_rather_than_duplicates() {
        let conn = test_conn();
        index_mail(&conn, "acc", "INBOX", &test_header(1), "<p>v1</p>").unwrap();
        let mut updated = test_header(1);
        updated.subject = "Updated subject".to_string();
        index_mail(&conn, "acc", "INBOX", &updated, "<p>v2</p>").unwrap();

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);

        let (header, body) = fetch_mail(&conn, "acc", "INBOX", 1).unwrap();
        assert_eq!(header.subject, "Updated subject");
        assert_eq!(body, "<p>v2</p>");
    }

    #[test]
    fn accounts_and_mailboxes_do_not_collide_on_the_same_uid() {
        let conn = test_conn();
        index_mail(&conn, "acc1", "INBOX", &test_header(1), "acc1 inbox").unwrap();
        index_mail(&conn, "acc2", "INBOX", &test_header(1), "acc2 inbox").unwrap();
        index_mail(&conn, "acc1", "Archive", &test_header(1), "acc1 archive").unwrap();

        assert_eq!(fetch_mail(&conn, "acc1", "INBOX", 1).unwrap().1, "acc1 inbox");
        assert_eq!(fetch_mail(&conn, "acc2", "INBOX", 1).unwrap().1, "acc2 inbox");
        assert_eq!(fetch_mail(&conn, "acc1", "Archive", 1).unwrap().1, "acc1 archive");
    }

    // ── search ────────────────────────────────────────────────────────────────

    #[test]
    fn search_finds_a_matching_subject() {
        let conn = test_conn();
        index_mail(&conn, "acc", "INBOX", &test_header(1), "irrelevant body").unwrap();
        let results = search(&conn, "acc", "\"Subject 1\"", None).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].uid, 1);
    }

    #[test]
    fn search_is_scoped_to_the_given_account() {
        let conn = test_conn();
        index_mail(&conn, "acc1", "INBOX", &test_header(1), "body").unwrap();
        index_mail(&conn, "acc2", "INBOX", &test_header(2), "body").unwrap();
        let results = search(&conn, "acc1", "body", None).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].uid, 1);
    }

    #[test]
    fn search_mailbox_filter_does_not_allow_sql_injection() {
        // Regression test for the format!()-built WHERE clause this replaced:
        // a mailbox name containing a quote must be treated as a literal
        // value, not splice into the query.
        let conn = test_conn();
        index_mail(&conn, "acc", "INBOX", &test_header(1), "body").unwrap();
        let malicious_mailbox = "INBOX' OR '1'='1";
        // Must not error, and must not match anything (no mailbox has that
        // literal name), rather than the old code's behavior of the quote
        // breaking out of the string and the OR making every row match.
        let results = search(&conn, "acc", "body", Some(malicious_mailbox)).unwrap();
        assert_eq!(results.len(), 0);
    }

    // ── evict_lru_bodies ──────────────────────────────────────────────────────

    #[test]
    fn evict_lru_bodies_keeps_only_the_newest_rows() {
        let conn = test_conn();
        for uid in 1..=5 {
            conn.execute(
                "INSERT INTO bodies (account_id, mailbox, uid, body, cached_at) VALUES ('acc', 'INBOX', ?1, 'x', ?1)",
                params![uid],
            ).unwrap();
        }
        evict_lru_bodies(&conn, 3).unwrap();

        let mut stmt = conn.prepare("SELECT uid FROM bodies ORDER BY uid").unwrap();
        let uids: Vec<i64> = stmt
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(uids, vec![3, 4, 5]);
    }

    #[test]
    fn evict_lru_bodies_is_a_no_op_under_the_cap() {
        let conn = test_conn();
        conn.execute(
            "INSERT INTO bodies (account_id, mailbox, uid, body, cached_at) VALUES ('acc', 'INBOX', 1, 'x', 1)",
            [],
        ).unwrap();
        evict_lru_bodies(&conn, 100).unwrap();
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM bodies", [], |r| r.get(0)).unwrap();
        assert_eq!(count, 1);
    }

    // ── sync_decision ─────────────────────────────────────────────────────────

    #[test]
    fn first_time_seeing_a_nonempty_mailbox_fetches_from_uid_1() {
        assert_eq!(
            sync_decision(None, 100, 50),
            SyncPlan::FetchFrom { first_new_uid: 1 }
        );
    }

    #[test]
    fn first_time_seeing_an_empty_mailbox_is_up_to_date() {
        // uid_next of 1 means no message has ever been assigned a UID yet.
        assert_eq!(sync_decision(None, 100, 1), SyncPlan::UpToDate);
    }

    #[test]
    fn unchanged_uid_validity_and_uid_next_is_up_to_date() {
        assert_eq!(sync_decision(Some((100, 50)), 100, 50), SyncPlan::UpToDate);
    }

    #[test]
    fn new_mail_since_last_sync_fetches_from_the_old_uid_next() {
        assert_eq!(
            sync_decision(Some((100, 50)), 100, 80),
            SyncPlan::FetchFrom { first_new_uid: 50 }
        );
    }

    #[test]
    fn changed_uid_validity_forces_a_full_resync_regardless_of_uid_next() {
        assert_eq!(sync_decision(Some((100, 50)), 200, 50), SyncPlan::Resync);
        assert_eq!(sync_decision(Some((100, 50)), 200, 5), SyncPlan::Resync);
    }

    #[test]
    fn report_mailbox_state_wipes_cached_messages_on_uid_validity_change() {
        let conn = test_conn();
        index_mail(&conn, "acc", "INBOX", &test_header(1), "body").unwrap();
        report_mailbox_state(&conn, "acc", "INBOX", 100, 50).unwrap();

        let plan = report_mailbox_state(&conn, "acc", "INBOX", 200, 50).unwrap();
        assert_eq!(plan, SyncPlan::Resync);

        let count: i64 = conn.query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0)).unwrap();
        assert_eq!(count, 0);
        let body_count: i64 = conn.query_row("SELECT COUNT(*) FROM bodies", [], |r| r.get(0)).unwrap();
        assert_eq!(body_count, 0);
    }

    #[test]
    fn report_mailbox_state_persists_what_it_saw_for_next_time() {
        let conn = test_conn();
        report_mailbox_state(&conn, "acc", "INBOX", 100, 50).unwrap();
        let plan = report_mailbox_state(&conn, "acc", "INBOX", 100, 50).unwrap();
        assert_eq!(plan, SyncPlan::UpToDate);
    }
}

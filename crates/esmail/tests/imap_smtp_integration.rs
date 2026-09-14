//! Drives esmail's real `ImapActor`/`SmtpActor` (the exact command/event
//! channels `main.rs` wires up) against `mail-mock-server`, an in-process
//! IMAP + SMTP server built for this purpose -- see
//! `crates/mail-mock-server/README.md` for why a hand-rolled server and how
//! its TLS trust works.
//!
//! Needs the bundled test CA trusted (see that README); tests here check
//! for `ESMAIL_TEST_CA_TRUSTED=1` and skip with an explanatory message
//! rather than failing confusingly on a machine that hasn't set that up.
//! CI does the trust step then sets the var -- see `.github/workflows/ci.yml`.
//!
//! Fast, correctness-focused tests run by default; `#[ignore]`d
//! concurrent-load stress tests run with `-- --ignored`.

use std::time::Duration;

use esmail::compose::ComposeState;
use esmail::imap::{ImapActor, ImapCommand, ImapEvent};
use esmail::smtp::{SmtpAccount, SmtpActor, SmtpCommand, SmtpEvent};
use secrecy::SecretString;
use tokio::sync::mpsc;
use tokio::time::timeout;

use mail_mock_server::fixtures::{TEST_PASSWORD, TEST_USER};

const RECV_TIMEOUT: Duration = Duration::from_secs(20);

fn ca_trusted() -> bool {
    std::env::var("ESMAIL_TEST_CA_TRUSTED").is_ok()
}

macro_rules! skip_unless_ca_trusted {
    () => {
        if !ca_trusted() {
            eprintln!(
                "skipping: set ESMAIL_TEST_CA_TRUSTED=1 once mail-mock-server/certs/ca.crt is trusted \
                 (see crates/mail-mock-server/README.md)"
            );
            return;
        }
    };
}

/// A running mock server plus a connected `ImapActor`'s command/event
/// handles -- everything a test needs to drive esmail's client exactly as
/// `main.rs` does.
struct Harness {
    server: mail_mock_server::RunningServer,
    imap_cmd: mpsc::Sender<ImapCommand>,
    imap_evt: mpsc::Receiver<ImapEvent>,
}

async fn start_harness(inbox_count: u32) -> Harness {
    let store = mail_mock_server::new_store();
    {
        let mut guard = store.lock().unwrap();
        mail_mock_server::fixtures::seed(&mut guard, inbox_count);
    }
    let server = mail_mock_server::start(store).await.expect("start mock servers");

    let (imap_cmd_tx, imap_cmd_rx) = mpsc::channel(32);
    let (imap_evt_tx, mut imap_evt_rx) = mpsc::channel(32);
    ImapActor::spawn(imap_cmd_rx, imap_evt_tx);

    imap_cmd_tx
        .send(ImapCommand::Connect {
            host: "localhost".to_string(),
            port: server.imap_addr.port(),
            username: TEST_USER.to_string(),
            password: SecretString::from(TEST_PASSWORD),
        })
        .await
        .unwrap();

    match timeout(RECV_TIMEOUT, imap_evt_rx.recv()).await {
        Ok(Some(ImapEvent::Connected)) => {}
        other => panic!("expected Connected, got {other:?} (is the test CA actually trusted?)"),
    }

    Harness { server, imap_cmd: imap_cmd_tx, imap_evt: imap_evt_rx }
}

#[tokio::test]
async fn connect_and_fetch_mailboxes() {
    skip_unless_ca_trusted!();
    let mut h = start_harness(0).await;

    h.imap_cmd.send(ImapCommand::FetchMailboxes).await.unwrap();
    match timeout(RECV_TIMEOUT, h.imap_evt.recv()).await.unwrap().unwrap() {
        ImapEvent::Mailboxes(names) => {
            assert!(names.contains(&"INBOX".to_string()));
            assert!(names.contains(&"Sent".to_string()));
        }
        other => panic!("expected Mailboxes, got {other:?}"),
    }
}

#[tokio::test]
async fn fetch_headers_paginates_the_seeded_inbox() {
    skip_unless_ca_trusted!();
    // 2 fixture messages + 120 plain ones = 122, which at 50/page is 3 pages.
    let mut h = start_harness(120).await;

    h.imap_cmd.send(ImapCommand::FetchHeaders { mailbox: "INBOX".to_string(), page: 1, req_id: 1 }).await.unwrap();
    match timeout(RECV_TIMEOUT, h.imap_evt.recv()).await.unwrap().unwrap() {
        ImapEvent::Headers { headers, total_pages, mailbox_state, .. } => {
            assert_eq!(headers.len(), 50);
            assert_eq!(total_pages, 3);
            assert!(mailbox_state.uid_next > 122);
        }
        other => panic!("expected Headers, got {other:?}"),
    }

    h.imap_cmd.send(ImapCommand::FetchHeaders { mailbox: "INBOX".to_string(), page: 3, req_id: 2 }).await.unwrap();
    match timeout(RECV_TIMEOUT, h.imap_evt.recv()).await.unwrap().unwrap() {
        ImapEvent::Headers { headers, page, .. } => {
            assert_eq!(page, 3);
            assert_eq!(headers.len(), 22); // 122 - 2*50
        }
        other => panic!("expected Headers, got {other:?}"),
    }
}

#[tokio::test]
async fn fetch_body_renders_html_resolves_cid_and_finds_the_attachment() {
    skip_unless_ca_trusted!();
    let mut h = start_harness(0).await;

    h.imap_cmd.send(ImapCommand::FetchHeaders { mailbox: "INBOX".to_string(), page: 1, req_id: 1 }).await.unwrap();
    let headers = match timeout(RECV_TIMEOUT, h.imap_evt.recv()).await.unwrap().unwrap() {
        ImapEvent::Headers { headers, .. } => headers,
        other => panic!("expected Headers, got {other:?}"),
    };
    let report = headers.iter().find(|h| h.subject.contains("Report with image")).expect("fixture message present");

    h.imap_cmd.send(ImapCommand::FetchBody { mailbox: "INBOX".to_string(), uid: report.uid, req_id: 2 }).await.unwrap();
    match timeout(RECV_TIMEOUT, h.imap_evt.recv()).await.unwrap().unwrap() {
        ImapEvent::Body { html, attachments, .. } => {
            // render.rs resolves cid: to an inline data: URL rather than
            // leaving the original cid: reference in the markup.
            assert!(html.contains("data:image/png"), "expected an inlined cid: image, got: {html}");
            assert!(!html.contains("cid:pixel@example.com"));
            assert_eq!(attachments.len(), 1);
            assert_eq!(attachments[0].filename, "notes.txt");
        }
        other => panic!("expected Body, got {other:?}"),
    }
}

#[tokio::test]
async fn fetch_body_decodes_an_rfc2047_encoded_unicode_subject() {
    skip_unless_ca_trusted!();
    let mut h = start_harness(0).await;

    h.imap_cmd.send(ImapCommand::FetchHeaders { mailbox: "INBOX".to_string(), page: 1, req_id: 1 }).await.unwrap();
    let headers = match timeout(RECV_TIMEOUT, h.imap_evt.recv()).await.unwrap().unwrap() {
        ImapEvent::Headers { headers, .. } => headers,
        other => panic!("expected Headers, got {other:?}"),
    };
    let unicode = headers.iter().find(|h| h.subject.contains("Café")).expect("unicode fixture present, RFC 2047 decoded");
    assert!(unicode.subject.contains("日本語"), "subject was: {:?}", unicode.subject);
}

#[tokio::test]
async fn bulk_download_streams_every_message_with_progress() {
    skip_unless_ca_trusted!();
    let mut h = start_harness(10).await; // 2 fixtures + 10 = 12 total

    h.imap_cmd.send(ImapCommand::BulkDownload { mailbox: "INBOX".to_string() }).await.unwrap();

    let mut mail_data_count = 0;
    let final_progress = loop {
        match timeout(RECV_TIMEOUT, h.imap_evt.recv()).await.unwrap().unwrap() {
            ImapEvent::MailData { .. } => mail_data_count += 1,
            ImapEvent::DownloadProgress { current, total } => {
                assert_eq!(total, 12);
                if current == total {
                    break current;
                }
            }
            other => panic!("unexpected event during bulk download: {other:?}"),
        }
    };
    assert_eq!(mail_data_count, 12);
    assert_eq!(final_progress, 12);
}

/// The real round trip this whole server exists to make testable: esmail's
/// `SmtpActor` sends a message, and esmail's `ImapActor` can then see it.
#[tokio::test]
async fn send_via_smtp_then_see_it_over_imap() {
    skip_unless_ca_trusted!();
    let mut h = start_harness(0).await; // 2 fixtures in INBOX to start

    let (smtp_cmd_tx, smtp_cmd_rx) = mpsc::channel(8);
    let (smtp_evt_tx, mut smtp_evt_rx) = mpsc::channel(8);
    SmtpActor::spawn(smtp_cmd_rx, smtp_evt_tx);

    let account = SmtpAccount {
        host: "127.0.0.1".to_string(),
        port: h.server.smtp_addr.port(),
        tls: esmail::config::TlsMode::None,
        username: TEST_USER.to_string(),
        password: SecretString::from(TEST_PASSWORD),
        from_address: TEST_USER.to_string(),
    };
    let compose = ComposeState {
        to: TEST_USER.to_string(),
        subject: "Sent from the integration test".to_string(),
        body: "This message was sent over SMTP by the test.".to_string(),
        ..Default::default()
    };

    smtp_cmd_tx.send(SmtpCommand::Send { account, compose }).await.unwrap();
    match timeout(RECV_TIMEOUT, smtp_evt_rx.recv()).await.unwrap().unwrap() {
        SmtpEvent::Sent => {}
        SmtpEvent::Error(e) => panic!("send failed: {e}"),
    }

    h.imap_cmd.send(ImapCommand::FetchHeaders { mailbox: "INBOX".to_string(), page: 1, req_id: 1 }).await.unwrap();
    match timeout(RECV_TIMEOUT, h.imap_evt.recv()).await.unwrap().unwrap() {
        ImapEvent::Headers { headers, mailbox_state, .. } => {
            assert_eq!(headers.len(), 3); // 2 fixtures + the one just sent
            assert!(headers.iter().any(|h| h.subject == "Sent from the integration test"));
            assert!(mailbox_state.uid_next >= 4);
        }
        other => panic!("expected Headers, got {other:?}"),
    }
}

#[tokio::test]
async fn disconnected_session_reconnects_and_serves_the_next_request() {
    skip_unless_ca_trusted!();
    let mut h = start_harness(0).await;

    // `ImapActor` clears `self.session` on any command error and
    // reconnects on the next command via `ensure_connected` -- simulate
    // that by fetching a mailbox that doesn't exist (a `NO` response,
    // which `fetch_headers` turns into an `Err`), then confirming a normal
    // request right after still succeeds.
    h.imap_cmd.send(ImapCommand::FetchHeaders { mailbox: "NoSuchMailbox".to_string(), page: 1, req_id: 1 }).await.unwrap();
    match timeout(RECV_TIMEOUT, h.imap_evt.recv()).await.unwrap().unwrap() {
        ImapEvent::Error(_) => {}
        other => panic!("expected Error for a nonexistent mailbox, got {other:?}"),
    }

    h.imap_cmd.send(ImapCommand::FetchMailboxes).await.unwrap();
    // ensure_connected reports Disconnected before it starts reconnecting.
    let mut saw_disconnected = false;
    loop {
        match timeout(RECV_TIMEOUT, h.imap_evt.recv()).await.unwrap().unwrap() {
            ImapEvent::Disconnected => saw_disconnected = true,
            ImapEvent::Connected => {}
            ImapEvent::Mailboxes(names) => {
                assert!(names.contains(&"INBOX".to_string()));
                break;
            }
            other => panic!("unexpected event while reconnecting: {other:?}"),
        }
    }
    assert!(saw_disconnected);
}

mod stress {
    use super::*;

    /// Seeds a large inbox and fires every page fetch + every body fetch
    /// concurrently against one `ImapActor` (one real TLS session), then
    /// does the same for a burst of concurrent SMTP sends against a fresh
    /// `SmtpActor` per send (mirroring one connection per send, same as
    /// `smtp.rs::build_transport` does today -- no pooling). Not a
    /// benchmark, just a "does this fall over under concurrent load"
    /// check; run explicitly since it's slower than the rest of the suite.
    #[tokio::test]
    #[ignore]
    async fn many_concurrent_header_and_body_fetches_all_complete() {
        skip_unless_ca_trusted!();
        const INBOX_COUNT: u32 = 400;
        let mut h = start_harness(INBOX_COUNT).await;

        // ImapActor serializes commands (one mpsc receiver, one session),
        // so "concurrent" here means "fired without waiting for replies in
        // between" -- exercising req_id-based reply matching under a
        // saturated queue, which is exactly what B2's req_id bookkeeping
        // was for.
        let total_pages = (INBOX_COUNT + 2).div_ceil(50);
        for page in 1..=total_pages {
            h.imap_cmd.send(ImapCommand::FetchHeaders { mailbox: "INBOX".to_string(), page, req_id: page as u64 }).await.unwrap();
        }

        let mut headers_by_req = std::collections::HashMap::new();
        for _ in 0..total_pages {
            match timeout(RECV_TIMEOUT, h.imap_evt.recv()).await.unwrap().unwrap() {
                ImapEvent::Headers { req_id, headers, .. } => {
                    headers_by_req.insert(req_id, headers);
                }
                other => panic!("unexpected event: {other:?}"),
            }
        }
        assert_eq!(headers_by_req.len(), total_pages as usize);
        let total_headers: usize = headers_by_req.values().map(|h| h.len()).sum();
        assert_eq!(total_headers as u32, INBOX_COUNT + 2);

        // Now hammer FetchBody for every UID we just learned about. Sending
        // and receiving must happen concurrently here: both the command and
        // event mpsc channels are bounded (32, matching `main.rs`'s own
        // sizing), so sending all ~400 commands before draining any events
        // deadlocks -- the actor blocks trying to push the 33rd event into
        // a full channel nobody's reading yet, which stops it from ever
        // draining the next command, which is exactly the 33rd send this
        // loop is itself blocked on.
        let all_uids: Vec<u32> = headers_by_req.values().flatten().map(|h| h.uid).collect();
        let sender_cmd_tx = h.imap_cmd.clone();
        let uids_to_send = all_uids.clone();
        let sender = tokio::spawn(async move {
            for (i, uid) in uids_to_send.iter().enumerate() {
                sender_cmd_tx.send(ImapCommand::FetchBody { mailbox: "INBOX".to_string(), uid: *uid, req_id: 1000 + i as u64 }).await.unwrap();
            }
        });
        let mut bodies_seen = 0;
        for _ in 0..all_uids.len() {
            match timeout(Duration::from_secs(60), h.imap_evt.recv()).await.unwrap().unwrap() {
                ImapEvent::Body { .. } => bodies_seen += 1,
                other => panic!("unexpected event: {other:?}"),
            }
        }
        sender.await.unwrap();
        assert_eq!(bodies_seen, all_uids.len());
    }

    #[tokio::test]
    #[ignore]
    async fn many_concurrent_smtp_sends_all_land_in_the_inbox() {
        skip_unless_ca_trusted!();
        let h = start_harness(0).await;
        const SEND_COUNT: usize = 50;

        let sends = (0..SEND_COUNT).map(|i| {
            let smtp_addr = h.server.smtp_addr;
            async move {
                let (cmd_tx, cmd_rx) = mpsc::channel(1);
                let (evt_tx, mut evt_rx) = mpsc::channel(1);
                SmtpActor::spawn(cmd_rx, evt_tx);
                let account = SmtpAccount {
                    host: "127.0.0.1".to_string(),
                    port: smtp_addr.port(),
                    tls: esmail::config::TlsMode::None,
                    username: TEST_USER.to_string(),
                    password: SecretString::from(TEST_PASSWORD),
                    from_address: TEST_USER.to_string(),
                };
                let compose = ComposeState {
                    to: TEST_USER.to_string(),
                    subject: format!("Stress message #{i}"),
                    body: format!("Body of stress message #{i}"),
                    ..Default::default()
                };
                cmd_tx.send(SmtpCommand::Send { account, compose }).await.unwrap();
                match timeout(Duration::from_secs(30), evt_rx.recv()).await {
                    Ok(Some(SmtpEvent::Sent)) => true,
                    other => {
                        eprintln!("send #{i} failed: {other:?}");
                        false
                    }
                }
            }
        });

        let results = futures::future::join_all(sends).await;
        let succeeded = results.iter().filter(|ok| **ok).count();
        assert_eq!(succeeded, SEND_COUNT, "{succeeded}/{SEND_COUNT} concurrent sends succeeded");

        // `seed()` always delivers its 2 hand-built fixture messages into
        // INBOX regardless of `inbox_count`, so INBOX has 2 + SEND_COUNT,
        // not just SEND_COUNT -- walk every page rather than assuming a
        // fixed page count, and check by subject rather than by total
        // count alone so a wrong delivery target would still be caught.
        let (imap_cmd_tx, imap_cmd_rx) = mpsc::channel(32);
        let (imap_evt_tx, mut imap_evt_rx) = mpsc::channel(32);
        ImapActor::spawn(imap_cmd_rx, imap_evt_tx);
        imap_cmd_tx
            .send(ImapCommand::Connect {
                host: "localhost".to_string(),
                port: h.server.imap_addr.port(),
                username: TEST_USER.to_string(),
                password: SecretString::from(TEST_PASSWORD),
            })
            .await
            .unwrap();
        assert!(matches!(timeout(RECV_TIMEOUT, imap_evt_rx.recv()).await.unwrap().unwrap(), ImapEvent::Connected));

        let mut stress_subjects_seen = std::collections::HashSet::new();
        let mut total_messages = 0u32;
        let mut page = 1;
        loop {
            imap_cmd_tx.send(ImapCommand::FetchHeaders { mailbox: "INBOX".to_string(), page, req_id: page as u64 }).await.unwrap();
            let (headers, total_pages) = match timeout(RECV_TIMEOUT, imap_evt_rx.recv()).await.unwrap().unwrap() {
                ImapEvent::Headers { headers, total_pages, .. } => (headers, total_pages),
                other => panic!("expected Headers, got {other:?}"),
            };
            total_messages += headers.len() as u32;
            for h in &headers {
                if h.subject.starts_with("Stress message #") {
                    stress_subjects_seen.insert(h.subject.clone());
                }
            }
            if page >= total_pages {
                break;
            }
            page += 1;
        }

        assert_eq!(total_messages, 2 + SEND_COUNT as u32, "2 fixtures + {SEND_COUNT} stress sends");
        assert_eq!(stress_subjects_seen.len(), SEND_COUNT, "every stress send should show up exactly once");
    }
}

//! Protocol-level smoke tests: does a real `async-imap`/`lettre` client
//! actually parse this server's responses? These drive the client crates
//! directly (not through esmail), which is faster to debug than going
//! through esmail's actor layer when the wire format itself is wrong --
//! see `crates/esmail/tests/imap_smtp_integration.rs` for the esmail-level
//! round trip.

use futures::StreamExt;
use mail_mock_server::fixtures::{TEST_PASSWORD, TEST_USER};

fn install_ca_or_skip() -> bool {
    // These tests need the bundled CA (crate root `certs/ca.crt`) trusted
    // by the OS so native-tls's default validation succeeds -- see this
    // crate's README. Rather than fail confusingly on every dev machine
    // that hasn't done that, skip with a clear message.
    std::env::var("ESMAIL_TEST_CA_TRUSTED").is_ok()
}

#[tokio::test]
async fn imap_login_list_examine_fetch_round_trip() {
    if !install_ca_or_skip() {
        eprintln!("skipping: set ESMAIL_TEST_CA_TRUSTED=1 once mail-mock-server/certs/ca.crt is trusted (see README.md)");
        return;
    }

    let store = mail_mock_server::new_store();
    {
        let mut guard = store.lock().unwrap();
        mail_mock_server::fixtures::seed(&mut guard, 5);
    }
    let server = mail_mock_server::start(store).await.expect("start servers");

    let tls_connector = tokio_native_tls::native_tls::TlsConnector::new().unwrap();
    let tokio_tls_connector = tokio_native_tls::TlsConnector::from(tls_connector);
    let stream = tokio::net::TcpStream::connect(server.imap_addr).await.unwrap();
    let tls_stream = tokio_tls_connector.connect("localhost", stream).await.unwrap();
    let mut client = async_imap::Client::new(tls_stream);
    let _ = client.read_response().await;

    let mut session = client.login(TEST_USER, TEST_PASSWORD).await.map_err(|(e, _)| e).expect("login");

    let mut names = Vec::new();
    let mut list = session.list(Some(""), Some("*")).await.unwrap();
    while let Some(n) = list.next().await {
        names.push(n.unwrap().name().to_string());
    }
    drop(list);
    assert!(names.contains(&"INBOX".to_string()), "names: {names:?}");

    let mailbox = session.examine("INBOX").await.unwrap();
    // 2 hand-built fixture messages + 5 plain ones from `seed(.., 5)`.
    assert_eq!(mailbox.exists, 7);
    assert!(mailbox.uid_next.unwrap() > mailbox.exists);

    let fetches: Vec<_> = session.fetch("1:7", "(UID ENVELOPE)").await.unwrap().collect().await;
    assert_eq!(fetches.len(), 7);
    let first = fetches[0].as_ref().unwrap();
    assert!(first.uid.is_some());
    assert!(first.envelope().is_some());

    // Fetch the full body of one message by UID and confirm it round-trips.
    let uid = first.uid.unwrap();
    {
        let mut body_fetch = session.uid_fetch(uid.to_string(), "RFC822").await.unwrap();
        let msg = body_fetch.next().await.unwrap().unwrap();
        let body = msg.body().expect("RFC822 body");
        assert!(!body.is_empty());
    }

    session.logout().await.unwrap();
}

#[tokio::test]
async fn smtp_send_then_imap_fetch_round_trip() {
    if !install_ca_or_skip() {
        eprintln!("skipping: set ESMAIL_TEST_CA_TRUSTED=1 once mail-mock-server/certs/ca.crt is trusted (see README.md)");
        return;
    }

    let store = mail_mock_server::new_store();
    {
        let mut guard = store.lock().unwrap();
        mail_mock_server::fixtures::seed(&mut guard, 0);
    }
    let server = mail_mock_server::start(store).await.expect("start servers");

    use lettre::transport::smtp::authentication::Credentials;
    use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};

    let transport = AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&server.smtp_addr.ip().to_string())
        .port(server.smtp_addr.port())
        .credentials(Credentials::new(TEST_USER.to_string(), TEST_PASSWORD.to_string()))
        .build();

    let message = Message::builder()
        .from(format!("Sender <sender@example.com>").parse().unwrap())
        .to(TEST_USER.parse().unwrap())
        .subject("Round trip test")
        .body("Hello from the smoke test.".to_string())
        .unwrap();

    transport.send(message).await.expect("smtp send");

    // Now fetch it back over IMAP.
    let tls_connector = tokio_native_tls::native_tls::TlsConnector::new().unwrap();
    let tokio_tls_connector = tokio_native_tls::TlsConnector::from(tls_connector);
    let stream = tokio::net::TcpStream::connect(server.imap_addr).await.unwrap();
    let tls_stream = tokio_tls_connector.connect("localhost", stream).await.unwrap();
    let mut client = async_imap::Client::new(tls_stream);
    let _ = client.read_response().await;
    let mut session = client.login(TEST_USER, TEST_PASSWORD).await.map_err(|(e, _)| e).expect("login");

    let mailbox = session.examine("INBOX").await.unwrap();
    assert_eq!(mailbox.exists, 3); // 2 fixtures + this delivery

    let fetches: Vec<_> = session.fetch("1:3", "(UID ENVELOPE)").await.unwrap().collect().await;
    let subjects: Vec<String> = fetches
        .iter()
        .map(|f| {
            let env = f.as_ref().unwrap().envelope().unwrap();
            String::from_utf8_lossy(env.subject.as_ref().unwrap()).to_string()
        })
        .collect();
    assert!(subjects.iter().any(|s| s.contains("Round trip test")), "subjects: {subjects:?}");

    session.logout().await.unwrap();
}

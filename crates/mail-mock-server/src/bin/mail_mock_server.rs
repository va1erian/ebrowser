//! Standalone binary form of `mail-mock-server`: run it under Docker (see
//! `../../Dockerfile`) or directly for manual/exploratory testing of esmail
//! against a real (if fake-data) IMAP + SMTP server, rather than only in
//! esmail's own automated integration tests.
//!
//! Env vars:
//! - `IMAP_BIND` (default `0.0.0.0:1993`)
//! - `SMTP_BIND` (default `0.0.0.0:1025`)
//! - `INBOX_COUNT` (default `60`) -- extra plain messages seeded into INBOX,
//!   beyond the three hand-built fixture messages; large enough to exercise
//!   `imap.rs`'s 50-per-page pagination.
//! - `CA_OUT` -- if set, the CA certificate (PEM) is written to this path on
//!   startup, so a caller (a CI job, a docker-compose healthcheck) can pick
//!   it up and install it as a trusted root without needing this crate's
//!   source tree.

use std::io::Write;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let imap_bind = std::env::var("IMAP_BIND").unwrap_or_else(|_| "0.0.0.0:1993".to_string());
    let smtp_bind = std::env::var("SMTP_BIND").unwrap_or_else(|_| "0.0.0.0:1025".to_string());
    let inbox_count: u32 = std::env::var("INBOX_COUNT").ok().and_then(|s| s.parse().ok()).unwrap_or(60);

    if let Ok(ca_out) = std::env::var("CA_OUT") {
        let mut f = std::fs::File::create(&ca_out)?;
        f.write_all(mail_mock_server::CA_CERT_PEM)?;
        log::info!("wrote CA certificate to {ca_out}");
    }

    let store = mail_mock_server::new_store();
    {
        let mut guard = store.lock().unwrap();
        mail_mock_server::fixtures::seed(&mut guard, inbox_count);
    }

    let identity = tokio_native_tls::native_tls::Identity::from_pkcs12(mail_mock_server::SERVER_PKCS12, mail_mock_server::SERVER_PKCS12_PASSWORD)?;
    let acceptor: tokio_native_tls::TlsAcceptor = tokio_native_tls::native_tls::TlsAcceptor::new(identity)?.into();
    drop(acceptor); // constructed only to fail fast here if the bundled cert is bad

    let imap_addr = mail_mock_server::imap_server::spawn(store.clone(), &imap_bind, mail_mock_server::SERVER_PKCS12, mail_mock_server::SERVER_PKCS12_PASSWORD).await?;
    let smtp_addr = mail_mock_server::smtp_server::spawn(store.clone(), &smtp_bind).await?;

    log::info!("IMAP (implicit TLS) listening on {imap_addr}");
    log::info!("SMTP (plaintext, matches esmail's TlsMode::None) listening on {smtp_addr}");
    log::info!("account: {} / {}", mail_mock_server::fixtures::TEST_USER, mail_mock_server::fixtures::TEST_PASSWORD);

    // Runs until killed -- this is a long-lived test fixture process, not a
    // one-shot tool.
    std::future::pending::<()>().await;
    Ok(())
}

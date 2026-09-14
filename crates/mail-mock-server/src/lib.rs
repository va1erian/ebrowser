//! An in-process IMAP + SMTP server for testing esmail without a real
//! mailbox provider. See `crates/esmail/tests/imap_smtp_integration.rs` for
//! how it's driven, and this crate's `README.md` for the design rationale
//! (why a hand-rolled server rather than a real one like GreenMail, and why
//! TLS trust is handled the way it is).

pub mod fixtures;
pub mod imap_server;
pub mod smtp_server;
pub mod store;

use std::sync::{Arc, Mutex};

pub use store::Store;

/// The test-only TLS identity the IMAP server presents, bundled into the
/// binary so nothing needs to be generated at runtime. See
/// `certs/regen.sh` to rotate it, and `certs/ca.crt` (the half to install
/// as a trusted root) is exposed via [`CA_CERT_PEM`].
pub const SERVER_PKCS12: &[u8] = include_bytes!("../certs/server.p12");
pub const SERVER_PKCS12_PASSWORD: &str = "esmail-test-only";
pub const CA_CERT_PEM: &[u8] = include_bytes!("../certs/ca.crt");

pub struct RunningServer {
    pub imap_addr: std::net::SocketAddr,
    pub smtp_addr: std::net::SocketAddr,
    pub store: store::SharedStore,
}

/// Starts both servers bound to `127.0.0.1:0` (an OS-assigned free port
/// each) with `store` as their shared backing state, and returns the
/// addresses they ended up on. Both run for the lifetime of the process --
/// there is no shutdown handle, since every caller (tests, the CLI binary)
/// only ever needs one that lives until the process exits.
pub async fn start(store: store::SharedStore) -> anyhow::Result<RunningServer> {
    let imap_addr = imap_server::spawn(store.clone(), "127.0.0.1:0", SERVER_PKCS12, SERVER_PKCS12_PASSWORD).await?;
    let smtp_addr = smtp_server::spawn(store.clone(), "127.0.0.1:0").await?;
    Ok(RunningServer { imap_addr, smtp_addr, store })
}

pub fn new_store() -> store::SharedStore {
    Arc::new(Mutex::new(Store::new()))
}

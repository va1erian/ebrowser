//! Default seed data loaded into a freshly started mock server, chosen to
//! exercise the parts of esmail that only a real IMAP round trip can verify:
//! pagination (`imap.rs::fetch_headers`'s 50-per-page math), HTML rendering
//! and `cid:` resolution (`render.rs`), attachment extraction (`render.rs`),
//! and RFC 2047 encoded-word decoding (`imap.rs::decode_rfc2047`).

use lettre::message::{Attachment, Message, MultiPart, SinglePart, header::ContentType};

use crate::store::Store;

pub const TEST_USER: &str = "alice@example.com";
pub const TEST_PASSWORD: &str = "hunter2";

/// Populates `store` with a test account and its mail. `inbox_count` controls
/// how many plain messages land in INBOX (beyond the three hand-built ones
/// below) -- callers doing pagination/stress testing pass a larger number
/// than callers just checking rendering.
pub fn seed(store: &mut Store, inbox_count: u32) {
    store.add_user(TEST_USER, TEST_PASSWORD);

    let html_uid = deliver_fixture(store, "INBOX", html_with_inline_image_and_attachment());
    let unicode_uid = deliver_fixture(store, "INBOX", unicode_subject_message());
    log::info!("seeded fixture messages: html/cid/attachment uid={html_uid}, unicode-subject uid={unicode_uid}");

    for i in 0..inbox_count {
        deliver_fixture(store, "INBOX", plain_message(i));
    }

    deliver_fixture(store, "Sent", plain_message(9999));
}

fn deliver_fixture(store: &mut Store, mailbox: &str, message: Message) -> u32 {
    let raw = message.formatted();
    let envelope = crate::store::parse_envelope(&raw);
    store.mailboxes.entry(mailbox.to_string()).or_insert_with(|| new_mailbox(mailbox)).append(raw, envelope)
}

fn new_mailbox(name: &str) -> crate::store::Mailbox {
    // Mirrors `Store::add_user`'s special-use mailboxes; only reached if a
    // caller delivers into a mailbox name that hasn't been created yet.
    crate::store::Mailbox { name: name.to_string(), messages: Vec::new(), uid_validity: 1, uid_next: 1 }
}

fn plain_message(i: u32) -> Message {
    Message::builder()
        .from(format!("Sender {i} <sender{i}@example.com>").parse().unwrap())
        .to(TEST_USER.parse().unwrap())
        .subject(format!("Test message #{i}"))
        .message_id(Some(format!("<msg-{i}@example.com>")))
        .body(format!("This is the body of test message #{i}.\n\nLorem ipsum dolor sit amet."))
        .unwrap()
}

fn unicode_subject_message() -> Message {
    Message::builder()
        .from("Bjørn Øyvind <bjorn@example.com>".parse().unwrap())
        .to(TEST_USER.parse().unwrap())
        // lettre RFC-2047-encodes non-ASCII subjects automatically, which is
        // exactly what `imap.rs::decode_rfc2047` is meant to decode back.
        .subject("Café résumé — 日本語のテスト")
        .message_id(Some("<unicode@example.com>".to_string()))
        .body("Plain-text body with unicode: café, naïve, 日本語.".to_string())
        .unwrap()
}

/// A `multipart/mixed` message whose body is `multipart/related` (HTML +
/// one inline image referenced by `cid:`) plus one real attachment --
/// exercises both `render.rs`'s `cid:` resolution and its attachment
/// extraction in a single fetch.
fn html_with_inline_image_and_attachment() -> Message {
    // Smallest possible valid PNG (1x1 transparent pixel).
    const PNG_1X1: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F,
        0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x64,
        0x60, 0x60, 0x60, 0x00, 0x00, 0x00, 0x05, 0x00, 0x01, 0x5A, 0x8E, 0xC8, 0x84, 0x00, 0x00,
        0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];

    let html_body = SinglePart::html(
        "<p>Hello, this message has an inline image and an attachment.</p>\
         <p><img src=\"cid:pixel@example.com\"></p>"
            .to_string(),
    );
    let inline_image = Attachment::new_inline("pixel@example.com".to_string())
        .body(PNG_1X1.to_vec(), ContentType::parse("image/png").unwrap());
    let related = MultiPart::related().singlepart(html_body).singlepart(inline_image);

    // `application/octet-stream`, not `text/plain`: `render.rs::extract_attachments`
    // deliberately excludes `text/plain`/`text/html` parts, since those are
    // the *body* candidates `find_html`/`find_text` already consider, not
    // attachments in their own right -- a real `.txt` attachment shares that
    // gap (see `smtp.rs::guess_mime_type`, which maps `.txt` to
    // `text/plain` too). Using a generic binary type here tests the
    // attachment pipeline as it's meant to behave, not that specific edge.
    let attachment = Attachment::new("notes.txt".to_string())
        .body(b"These are some plain-text notes attached to the message.".to_vec(), ContentType::parse("application/octet-stream").unwrap());

    Message::builder()
        .from("Reporter <reporter@example.com>".parse().unwrap())
        .to(TEST_USER.parse().unwrap())
        .subject("Report with image and attachment")
        .message_id(Some("<report@example.com>".to_string()))
        .multipart(MultiPart::mixed().multipart(related).singlepart(attachment))
        .unwrap()
}

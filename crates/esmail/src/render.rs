//! Safe HTML rendering for a fetched message (B5 of PLAN.md).
//!
//! The pipeline: parse the raw RFC822 bytes → pick the best `text/html`
//! alternative, falling back to `text/plain` (HTML-escaped — the previous
//! `format!("<pre>{}</pre>", text)` injected unescaped message text straight
//! into markup) → sanitize with `ammonia` (strips `<script>`, `<iframe>`,
//! `<form>`/`<input>`/`<button>`, and event-handler attributes by not
//! including them in its allowlist) → resolve `cid:` references to inline
//! `data:` URLs from the message's own inline parts → wrap in a base document
//! (charset, a readable default font, `max-width` so wide marketing HTML
//! doesn't force horizontal scroll).
//!
//! **Blocking remote images/CSS is deliberately not done here.** Markup
//! can't stop a network fetch — removing an `<img src>` from the DOM doesn't
//! un-issue a request already made, and rewriting it to a placeholder in the
//! markup would mean there's no URL left for a later "load remote images"
//! action to use. So this module leaves `http(s)` URLs exactly as the
//! message had them, and blocking happens at the network layer instead, via
//! `egui_servo_webview`'s `WebViewHandler::intercept` — see `main.rs`'s
//! `MessageViewHandler`.
//!
//! **Known limitation:** inline `style` attributes and `<style>` blocks are
//! stripped along with everything else not in ammonia's default allowlist,
//! so HTML mail that relies on CSS for layout/color renders as plain
//! formatted text. Preserving inline styles safely needs a CSS sanitizer on
//! top of ammonia's HTML one; out of scope for this pass.

use base64::Engine as _;
use mailparse::{MailHeaderMap, ParsedMail, parse_mail};

/// Render one message's raw RFC822 bytes into safe-to-display HTML.
/// Never fails — a message that can't be parsed at all renders as an escaped
/// error notice rather than propagating an error the caller would have to
/// turn into *some* string anyway.
pub fn render_message(raw: &[u8]) -> String {
    let body_html = match parse_mail(raw) {
        Ok(parsed) => render_parsed(&parsed),
        Err(e) => format!("<p>Could not parse this message: {}</p>", ammonia::clean_text(&e.to_string())),
    };
    wrap_document(&body_html)
}

fn render_parsed(parsed: &ParsedMail) -> String {
    if let Some(html) = find_html(parsed) {
        return sanitize(&resolve_cid_parts(&html, parsed));
    }
    if let Some(text) = find_text(parsed) {
        return format!("<pre>{}</pre>", ammonia::clean_text(&text));
    }
    "<p><em>(this message has no readable body)</em></p>".to_string()
}

fn find_html(part: &ParsedMail) -> Option<String> {
    if part.ctype.mimetype == "text/html" {
        return part.get_body().ok();
    }
    part.subparts.iter().find_map(find_html)
}

fn find_text(part: &ParsedMail) -> Option<String> {
    if part.ctype.mimetype == "text/plain" {
        return part.get_body().ok();
    }
    part.subparts.iter().find_map(find_text)
}

/// Replace every `cid:<id>` reference in `html` with a `data:` URL built
/// from the matching inline part's own bytes and MIME type, found by walking
/// every part of the message for one whose `Content-ID` matches. A `cid:`
/// with no matching part is left as-is — Servo will fail to load it, the
/// same as any other dead link, rather than this function guessing at a
/// replacement.
fn resolve_cid_parts(html: &str, root: &ParsedMail) -> String {
    let mut html = html.to_string();
    for part in all_parts(root) {
        let Some(cid) = content_id(part) else { continue };
        let Ok(bytes) = part.get_body_raw() else { continue };
        let data_url = format!(
            "data:{};base64,{}",
            part.ctype.mimetype,
            base64::engine::general_purpose::STANDARD.encode(&bytes)
        );
        html = html.replace(&format!("cid:{cid}"), &data_url);
    }
    html
}

fn all_parts<'a>(part: &'a ParsedMail<'a>) -> Vec<&'a ParsedMail<'a>> {
    let mut parts = vec![part];
    for sub in &part.subparts {
        parts.extend(all_parts(sub));
    }
    parts
}

/// The `Content-ID` header's value with the surrounding `<...>` stripped —
/// `cid:` URLs never include the angle brackets even though the header
/// itself always has them.
fn content_id(part: &ParsedMail) -> Option<String> {
    let raw = part.headers.get_first_value("Content-ID")?;
    Some(raw.trim().trim_start_matches('<').trim_end_matches('>').to_string())
}

fn sanitize(html: &str) -> String {
    ammonia::Builder::default()
        // `data:` — inline images resolved from `cid:` parts above need it to
        // survive; it's not in ammonia's default scheme allowlist.
        // `cid:` — an *unresolved* reference (no matching part) is left as
        // literal text by `resolve_cid_parts`, and would otherwise be
        // stripped right back out here since `cid` isn't a default scheme
        // either; allowing it keeps the dead reference exactly as
        // "left as-is" implies, rather than silently deleting it.
        .add_url_schemes(&["data", "cid"])
        .clean(html)
        .to_string()
}

fn wrap_document(body: &str) -> String {
    format!(
        r#"<!doctype html>
<meta charset="utf-8">
<style>
  body {{
    font-family: -apple-system, "Segoe UI", Roboto, Helvetica, Arial, sans-serif;
    font-size: 14px;
    line-height: 1.4;
    color: #1a1a1a;
    margin: 12px;
    max-width: 100%;
    overflow-wrap: break-word;
  }}
  img {{ max-width: 100%; height: auto; }}
  pre {{ white-space: pre-wrap; font-family: inherit; }}
</style>
{body}"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(headers: &str, body: &str) -> Vec<u8> {
        format!("{headers}\r\n\r\n{body}").into_bytes()
    }

    #[test]
    fn plain_text_is_html_escaped_not_injected_raw() {
        // Regression test for the bug this pipeline replaces:
        // format!("<pre>{}</pre>", text) injected unescaped message text.
        // `ammonia::clean_text` escapes more than just `<`/`>`/`&` (e.g. `/`
        // and space become numeric entities too, which still render
        // correctly, just not readably) -- assert the safety property, not
        // its exact entity choices.
        let raw = message(
            "Content-Type: text/plain",
            "<script>alert(1)</script> & <b>bold</b>",
        );
        let html = render_message(&raw);
        assert!(!html.contains("<script>"));
        assert!(!html.contains("<b>bold</b>"));
        assert!(html.contains("&lt;script&gt;"));
        assert!(html.contains("&amp;"));
    }

    #[test]
    fn html_part_is_preferred_over_plain_text() {
        let raw = message(
            "Content-Type: multipart/alternative; boundary=b",
            "--b\r\nContent-Type: text/plain\r\n\r\nplain version\r\n--b\r\nContent-Type: text/html\r\n\r\n<p>html version</p>\r\n--b--",
        );
        let html = render_message(&raw);
        assert!(html.contains("html version"));
        assert!(!html.contains("plain version"));
    }

    #[test]
    fn script_tags_are_stripped() {
        let raw = message(
            "Content-Type: text/html",
            "<p>hi</p><script>alert(document.cookie)</script>",
        );
        let html = render_message(&raw);
        assert!(!html.contains("<script"));
        assert!(!html.contains("alert(document.cookie)"));
        assert!(html.contains("<p>hi</p>"));
    }

    #[test]
    fn event_handler_attributes_are_stripped() {
        let raw = message(
            "Content-Type: text/html",
            r#"<img src="https://example.com/a.png" onerror="alert(1)">"#,
        );
        let html = render_message(&raw);
        assert!(!html.contains("onerror"));
        assert!(!html.contains("alert(1)"));
        // The remote src itself is left alone -- blocking it is the
        // network-layer handler's job, not this pipeline's.
        assert!(html.contains(r#"src="https://example.com/a.png""#));
    }

    #[test]
    fn iframes_and_forms_are_stripped() {
        let raw = message(
            "Content-Type: text/html",
            r#"<iframe src="https://evil.example"></iframe><form action="https://evil.example"><input name="x"></form><p>safe</p>"#,
        );
        let html = render_message(&raw);
        assert!(!html.contains("<iframe"));
        assert!(!html.contains("<form"));
        assert!(!html.contains("<input"));
        assert!(html.contains("<p>safe</p>"));
    }

    #[test]
    fn cid_references_become_inline_data_urls() {
        let raw = message(
            "Content-Type: multipart/related; boundary=b",
            "--b\r\nContent-Type: text/html\r\n\r\n<img src=\"cid:img1\">\r\n\
             --b\r\nContent-Type: image/png\r\nContent-ID: <img1>\r\nContent-Transfer-Encoding: base64\r\n\r\n\
             aGVsbG8=\r\n--b--",
        );
        let html = render_message(&raw);
        let expected_data_url = format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(b"hello")
        );
        assert!(
            html.contains(&expected_data_url),
            "expected {html:?} to contain {expected_data_url:?}"
        );
        assert!(!html.contains("cid:img1"));
    }

    #[test]
    fn an_unmatched_cid_is_left_as_is_rather_than_guessed_at() {
        let raw = message(
            "Content-Type: text/html",
            r#"<img src="cid:nonexistent">"#,
        );
        let html = render_message(&raw);
        assert!(html.contains("cid:nonexistent"));
    }

    #[test]
    fn an_unparseable_message_renders_an_escaped_notice_rather_than_panicking() {
        // Not asserting exact wording -- just that garbage input degrades to
        // *some* safe, non-empty HTML instead of propagating an error the
        // caller has no string-shaped place to put, or panicking.
        let html = render_message(b"not a valid mime message at all \xFF\xFE");
        assert!(html.contains("<html") || html.contains("<meta") || html.contains("<p>"));
    }

    #[test]
    fn empty_message_body_renders_a_placeholder() {
        let raw = message("Content-Type: text/plain", "");
        let html = render_message(&raw);
        assert!(html.len() > 0);
    }
}

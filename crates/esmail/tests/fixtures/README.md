# Message fixtures

Real-world messages kept as test cases for rendering conformance and
performance. `tests/render_fixtures.rs` runs every `*.eml` here through
`render::render_message` and the litehtml webview.

| File | Why it is here |
|---|---|
| `meilleurtaux.eml` | Salesforce Marketing Cloud newsletter: 108 tables nested up to 17 deep, 200+ inline styles, 13 remote images. Layout used to be exponential in the nesting depth and never finished; also exercises image sizing/positioning. |

## Adding one

1. In esMail, open the message and click **Export...** to save the raw `.eml`.
2. `ESMAIL_PREVIEW=path/to/message.eml cargo run -p esmail` renders it with no
   account (add `ESMAIL_SCREENSHOT=out.png` for a PNG).
3. **Redact it before committing.** These files live in a public repository.
   Real newsletters carry the recipient in several places:
   - the recipient address (`To`, `Delivered-To`, and the `Received ... for <addr>` line);
   - delivery-route and signature headers (`Received`, `X-Received`, `ARC-*`,
     `DKIM-Signature`, `Authentication-Results`, `Received-SPF`), which also
     embed the recipient and go stale once anything is edited;
   - per-recipient tokens: `List-Unsubscribe` (a JWT holding the subscriber id),
     `Return-Path`/`Reply-To` bounce tokens, `Message-ID`, `Feedback-ID`;
   - tracking tokens in the body: click links (`?qs=...`), the open-tracking
     pixel, and personalization blobs (`datasClientMTX=...`).

   Replace the address with `recipient@example.com`, drop the route/signature
   headers, and replace token values with `REDACTED`. Edit bytes, not text
   (the files are UTF-8 with CRLF and marked `-text` in `.gitattributes`).
   `fixtures_carry_no_personal_identifiers_or_tracking_tokens` fails if the
   obvious ones are left in, but it is a backstop, not a substitute for reading
   the file.

## Measuring

(Full guide, including the stack-level profiler: `docs/PERFORMANCE.md`.)

```text
RUST_LOG=egui_litehtml_webview=debug \
  cargo test -p esmail --test render_fixtures --release -- --nocapture --include-ignored
```

prints wall-clock time per fixture and width, and (with `RUST_LOG`) the
parse / layout / paint split for each pass. Debug builds are 5-10x slower
because the C++ layout engine is built unoptimized.

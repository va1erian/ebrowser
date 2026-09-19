# Handoff

Read this before touching anything. [PLAN.md](PLAN.md) is the full design; this
is what you need to actually work, plus the mistakes already made so you do not
repeat them.

**The webview engine changed: Servo is gone, replaced by litehtml.** See
[PLAN.md](PLAN.md)'s "Track C — Servo → litehtml migration" section for the
full story. `crates/egui-servo-webview` no longer exists — it was deleted,
not feature-flagged, in favor of `crates/egui-litehtml-webview`, a JS-less
HTML/CSS renderer (no legitimate mail client executes JS in HTML email, and
dropping the JS engine — the single largest component of any full browser
engine — is what makes the binary dramatically smaller: ~12.2 MiB release
`esmail.exe`, vs. Servo's 100-300MB+DLLs). **Everything below this notice
that mentions Servo, `egui-servo-webview`, A3/A5/A6's resource-interception/
input-forwarding/rendering-path work, or the overlay scrollbar (issue
#15/#22) describes that now-deleted crate** — kept for historical reasoning
per this file's own established convention (nothing here has ever been
deleted just because it stopped being current work), not as work remaining.
Track C's own "what landed / what did not" writeup in PLAN.md is the
current source of truth for the webview layer.

**Webview rendering now happens on a worker thread** (after the migration
above). `egui-litehtml-webview`'s `WebView` no longer parses/lays out/draws or
fetches images on the UI thread: each view owns a render thread that holds the
`PixbufContainer`, fetches remote images in parallel, and sends finished
frames back (see that crate's module doc). Consequences worth knowing:
`WebViewHandler` is now `Send + Sync` with `&self` methods (flip
`allow_remote` through an atomic, not a `RefCell`); link clicks arrive a
frame or more after the click; `ESMAIL_SCREENSHOT` waits for the webview to
finish rendering. Image drawing (`<img width=..>`, `max-width`,
`background-size`) and table layout speed both live in `litehtml-rs` `master`
(the `draw_image` fix, and a C++ litehtml bump that memoizes table cell
measurements -- without it layout time is exponential in table nesting depth
and a 17-deep marketing mail never finished); don't pin `Cargo.toml` to an
older commit. The message action bar has an **Export...** button that saves
the raw RFC822 source as an `.eml`; `ESMAIL_PREVIEW=some.eml` renders such a
file with no account. Problem emails are kept as repeatable test cases in
`crates/esmail/tests/fixtures/` (redact them first -- see the README there;
`tests/render_fixtures.rs` checks conformance and layout time, and has an
`--include-ignored` benchmark). Known limit: a message is one texture, so a
message taller than the GL max texture side (8192-16384 px) cannot be shown.

**Your next task, now that Track C's core migration (Phases 0-3) has
landed:**
- **Track C Phase 4 — text selection.** Investigated and confirmed real:
  litehtml has a genuine, working, well-tested `Selection` API
  (`start_at`/`extend_to`/`selected_text`/`rectangles`, 20+ upstream unit
  tests) that Servo's pinned 0.1.0 never had working at all (see the old
  issue #21 this superseded). Not yet wired into
  `egui-litehtml-webview` — needs the one-shot-render design (a fresh
  `Document` per interaction) revisited, since a drag-select gesture needs
  the same `Document` alive across a sequence of frames. See PLAN.md's
  Track C section for the full detail.
- **Track C Phase 5 — CI/Docker/`rusqlite` cleanup.** The Servo-era apt
  package lists in `.github/workflows/*.yml`/`Dockerfile.*` (nasm, Mesa/EGL
  dev packages, GStreamer, etc.) and the `rusqlite = "0.37"` pin (which
  existed only to unify with `servo-storage`'s own requirement) are both
  now-unnecessary leftovers.
- **Track C Phase 6 — rewrite this file's/PLAN.md's Servo-era framing**
  once Phase 4/5 land, so a fresh reader doesn't have to mentally
  find-and-replace "Servo" with "litehtml" through the older sections below.
- **Everything below this notice from the pre-migration phase-7 backlog is
  still open and unaffected by the webview swap** (it's all IMAP/SMTP/DB
  layer work, orthogonal to which engine renders a message body) — pick
  whichever is most useful:
  - **B7's `\Sent`/`\Trash`/`\Archive` wiring.** B8 built the special-use
    discovery infrastructure B7 was waiting on (`imap::SpecialUse`, from real
    `LIST` attributes with a name-based fallback) but did not wire it into
    `main.rs`'s hardcoded `SENT_MAILBOX`/`TRASH_MAILBOX`/`ARCHIVE_MAILBOX`
    constants — that's now a small, mechanical follow-up rather than a
    research question. Drafts (`APPEND` with `\Draft`), a real retry queue,
    rich-text composing, and recipient autocomplete are still open too.
  - **B9's per-operation progress** (generalizing `download_progress`) —
    deliberately left for its own commit until B8's concurrent `main.rs`
    changes landed; they now have, so this is unblocked.
  - **B8's IDLE-on-selected-mailbox.** B11's `idle_watch` already covers
    "new-mail push" but stays INBOX-only, same as B10 — see §B8 for the full
    "what landed / what didn't" writeup. A real collapsible mailbox-tree
    widget (currently a flat indented list) and wiring `is:unread` into
    search (B4's gap) are noted there too.
  - Any of B3/B4/B6/B7's still-open live-IMAP-protocol pieces (UID-based
    cache paging, server-side `UID SEARCH`, lazy `BODY.PEEK[n]`) — all
    verifiable now via `crates/mail-mock-server`, see below.

  (A6's zero-copy GL blit and A3/A5's Servo-specific input/resource-hook
  work are no longer applicable at all — that engine is gone.)

**There is now a real (mock) IMAP + SMTP server to verify live-protocol
code against** — `crates/mail-mock-server`, merged in from a separate branch
(see §5's table). This is what unblocked B11 (IMAP `IDLE`/push), B2's
worker-session split, B3's incremental-fetch half, and B7's `APPEND`-to-Sent
half landing here without the "no server to verify this against" caveat
every other live-IMAP deferral above still carries. If you pick up any of
those next — B3's UID-based cache-paging half, B4's server-side
`UID SEARCH`, B6's lazy `BODY.PEEK[n]`, B7's special-use discovery/drafts —
this is the tool to verify them with; see `crates/mail-mock-server/README.md`
for how to trust its test CA locally (already done once in this worktree)
and `crates/esmail/tests/imap_smtp_integration.rs` for the pattern to follow
(drive `ImapActor`/`SmtpActor`/`idle_watch` directly against a freshly seeded
in-process server, no live account needed — including a concurrency test,
`bulk_download_does_not_block_a_concurrent_header_fetch`, worth reading as a
template if your change also needs to prove two things run independently
rather than just that each individually returns the right data). Also worth
knowing: `mail-mock-server`'s `UID FETCH` handler only supported a single
numeric UID with `RFC822` until B3 needed range + `ENVELOPE` support too,
and it had no `APPEND` at all until B7 needed it — if your change sends a
command/fetch shape not already covered (check `imap_server.rs`'s module
doc for the current list), expect to extend the mock server itself first,
the same way B3/B7/B11 each did.

**B2's worker-session split (the "session pool" PLAN.md §B2 called for) is
now done too** — see [PLAN.md](PLAN.md) §B2. `imap.rs` gained
`spawn_body_worker`, a second independent IMAP connection (its own
connect/reconnect loop, `ensure_worker_connected`) that `FetchBody`/
`BulkDownload` are routed to, so they can no longer block `FetchHeaders`/
`FetchMailboxes` on `ImapActor`'s own session. Combined with B11's
`idle_watch.rs`, an account now normally holds three independent IMAP
connections at once (primary/header session, body worker, IDLE watch) —
worth knowing if you're debugging something that looks like "why are there
multiple sockets to the same server."

**B10 (new-mail notifications, Windows only) and B11 (IMAP push) are both
done** — see [PLAN.md](PLAN.md) §B10/§B11. B10 was developed in a separate
worktree/branch and merged in as commit `988beb9`; B11 (this worktree) adds
`idle_watch.rs`, a dedicated `IDLE` connection whose pushes make B10's
`spawn_new_mail_watch` check for new mail immediately instead of waiting for
its 60-second poll timer — the timer still runs unconditionally as a
fallback, so nothing regresses on a server without `IDLE` support. B11 also
added `IDLE` support to `mail-mock-server` itself (`Store::notify`, a
`tokio::sync::broadcast` fed on every delivery; `imap_server.rs`'s `IDLE`
handler subscribes to it while a client is idling).

---

## 1. The environment

Work in the worktree, never `cd` to the parent repo:

```
C:\Users\hadri\Documents\repos\ebrowser\src\.claude\worktrees\imap-mail-client-egui-736b94
```

Branch `claude/esmail-implementation-plan-a05e38` (this worktree's own —
`imap-mail-client-egui-736b94`, named in the rest of this section's original
text, was an earlier worktree whose work is long since merged). The layout:

```
Cargo.toml                               workspace root
crates/egui-litehtml-webview/src/lib.rs  the widget (litehtml-backed; see
                                          PLAN.md's Track C for why this
                                          replaced the Servo-backed
                                          egui-servo-webview)
crates/esmail/src/main.rs                the app
crates/esmail/src/imap.rs                IMAP actor
crates/esmail/src/idle_watch.rs          dedicated IMAP IDLE connection (B11)
crates/esmail/src/db.rs                  SQLite actor
crates/esmail/src/screenshot.rs          screenshot dumps
crates/mail-mock-server/                 in-process IMAP+SMTP server for tests
crates/esmail/tests/imap_smtp_integration.rs  drives the app against it
```

Commands, with real timings on this machine:

```bash
cargo check --workspace          # ~2s warm, ~2min cold
cargo test --workspace           # ~5s warm; run this, not just the widget's own tests
cargo build --bin esmail         # ~25s warm, well under a minute cold now that
                                  # the widget is litehtml-backed, not Servo
```

`cargo test --workspace` needs `mail-mock-server`'s test CA trusted once per
machine and `ESMAIL_TEST_CA_TRUSTED=1` set to actually run the integration
suite against it rather than skip with a note — see
`crates/mail-mock-server/README.md`; already done in this worktree.

A cold build no longer compiles Servo (see PLAN.md's Track C) — litehtml is
a much smaller C++ dependency, and a cold build should be minutes faster
than the old "Servo takes minutes" baseline this line used to warn about.
Still worth running long builds in the background rather than blocking on
them out of habit.

---

## 2. How to verify visual work — use this, it has already caught a bug

`cargo check` passing means nothing for rendering or input. Verify like this:

```bash
ESMAIL_PREVIEW=demo ESMAIL_SCREENSHOT="$PWD/shot.png" ESMAIL_SCREENSHOT_FRAMES=90 \
  ./target/debug/esmail.exe
```

Then **open `shot.png` and look at it** with the Read tool. The app renders one
page full-window with no IMAP account, captures after 90 frames, and exits on
its own — no human, no credentials, no window to close.

- `ESMAIL_PREVIEW` takes `demo` (a built-in page exercising accented text,
  links, a table, a text input and a tall scrollable block), a path to an HTML
  file, or a URL.
- `ESMAIL_SCREENSHOT_FRAMES` defaults to 30; use 90 for Servo to finish
  painting.
- Without `ESMAIL_PREVIEW` you get the login screen, which does **not** draw the
  webview at all — so it is useless for checking the widget.
- F12 dumps a screenshot during a normal interactive run.

`shot-*.png` and `esmail-screenshot-*.png` are gitignored.

**This is not optional ceremony.** A change that compiled, passed all 13 tests,
and left the warning counts identical still silently broke page layout. Only
comparing screenshots caught it. Take a screenshot before and after any change
to `show()`, the delegate, or sizing.

---

## 3. Landmines

Each of these cost real time. They are not hypothetical.

### 3.1 Servo's hooks fail OPEN — no longer applies

Kept for the reasoning (and because the bug it describes is a good general
lesson), but this is Servo-specific: litehtml has no navigation/resource
hooks at all to fail open or closed (see PLAN.md's Track C — Servo → litehtml
migration, "`WebViewHandler::navigation`/`NavigationPolicy` were dropped
entirely"). Nothing here is live guidance for the current code.

`NavigationRequest::drop` sends **allow**. An unhandled `WebResourceLoad` sends
**DoNotIntercept**. Dropping either permits the thing you meant to block, and
nothing warns you.

This already shipped as a bug: `drop(request)` was allowing every link
navigation *and* emitting `LinkClicked`, so links opened twice. Fixed in
`06db971` by calling `request.deny()` explicitly.

A3 is precisely about doing this properly. Every path through your handler must
end in an explicit `allow()` / `deny()` / `intercept()` / `cancel()`.

### 3.2 Do NOT resize the offscreen rendering context — no longer applies

Kept for the reasoning, but Servo-specific: litehtml's `pixbuf` backend has
no `OffscreenRenderingContext` at all (it renders into an in-memory pixel
buffer, not a GPU-backed rendering context), so there is nothing analogous
to double-resize here. Nothing here is live guidance for the current code.

`WebView::resize` already calls `resize_rendering_context` internally
(`servo-0.1.0/webview.rs:393`). Calling `offscreen_ctx.resize()` yourself first
makes Servo's own resize a no-op, because `OffscreenRenderingContext::resize`
early-outs on an unchanged size — and the page stays laid out at the old width.

This was tried, looked plausible, compiled, changed no warning counts, and was
caught only by the screenshot. There is a comment at the call site in
`show()`. Leave it alone.

### 3.3 `stop()` does not exist — no longer applies

`servo::WebView` in 0.1.0 has `load`, `reload`, `can_go_back`, `go_back(amount)`,
`can_go_forward`, `go_forward(amount)` — but **no** `stop()`. Do not plan around
cancelling an in-flight load. The only control points are up front.

Kept for the reasoning, but Servo-specific: litehtml's rendering is
synchronous (no async page load to be mid-flight when a `stop()` might be
called), so there is no analogous gap in the current API.

### 3.4 Field declaration order is drop order — no longer strictly matters

In `EsMailApp`, `web_view` is declared **before** `web_view_host` on purpose: the
view must be torn down before the engine backing it. Do not reorder.

Servo-specific reasoning, now historical: litehtml's `WebViewHost` owns no
engine/GL state any view actually depends on at drop time, so this ordering
no longer matters functionally — see the comment at the field declaration in
`crates/esmail/src/main.rs` (`EsMailApp`), which keeps the same order anyway
for minimal diff churn rather than because it is still load-bearing.

### 3.5 `egui::Panel::top` is current; `TopBottomPanel` is deprecated

This is the reverse of what you may assume. The existing `egui::Panel::*` call
sites are correct.

### 3.6 Files on disk have CRLF line endings

If you edit with a Python script, multi-line string matches fail silently
against `\n`. Normalize first:

```python
s = io.open(p, encoding="utf-8", newline="").read().replace("\r\n", "\n")
# ... edits ...
io.open(p, "w", encoding="utf-8", newline="\n").write(s)
```

Prefer the Edit tool where you can.

### 3.7 `rusqlite` — the `0.37` pin no longer applies

*(Resolved by deletion, not just historical: the constraint this section
described is gone, and so is the pin itself.)*

This used to require staying at exactly `0.37`: `servo-storage` depended on
`rusqlite ^0.37`, which pinned `libsqlite3-sys ^0.35`, and `libsqlite3-sys`
sets `links = "sqlite3"` so Cargo permits exactly one copy in the graph —
bumping rusqlite made the workspace unresolvable. Now that servo/
servo-storage are gone from the dependency tree entirely (confirmed via
`cargo tree -i libsqlite3-sys`, which shows only `esmail` depending on
`rusqlite`), that constraint no longer applies. The root `Cargo.toml` now
declares `rusqlite = { version = ">=0.37", ... }` with no upper bound; `cargo
update -p rusqlite` resolves to a newer version (0.40.2 as of this writing)
and the workspace builds and tests clean against it.

### 3.8 Servo's log noise is not yours — no longer applies

Six `webrender::device::gl` "Cropping texture upload" warnings during the first
two paints are its GPU cache warming up. Seven `profile_traits::mem`
"Disconnected" warnings at exit are Servo tearing itself down — they appear even
when no view is ever drawn. Both were investigated and are not embedder bugs.

Kept for the reasoning, but Servo-specific and already resolved at the
source rather than just historical: neither `webrender` nor `profile_traits`
is linked any more, so `init_logging()` in `main.rs` dropped the filtering
for them (see that function's own doc comment for what replaced it —
`fontdb`, a `cosmic-text`/litehtml dependency, is the one thing that can
still be noisy, and is a different, unrelated crate).

### 3.9 GL/EGL runtime DLLs — no longer applies

*(Resolved by deletion: this section described a real runtime requirement
under Servo that has no litehtml equivalent, not just historical color, so
it is removed rather than kept — see PLAN.md's working convention of
preserving reasoning only where it still explains something about the
current code.)*

This used to require copying `libEGL.dll`/`libGLESv2.dll` next to
`esmail.exe` before `ESMAIL_PREVIEW` screenshotting would work (Servo's
`surfman` GL/EGL rendering path). litehtml's `pixbuf` backend is pure
CPU/software — there is no GL/EGL dependency to ship, confirmed during the
migration by running the same `ESMAIL_PREVIEW=demo` screenshot check
successfully with neither DLL present next to the binary (see PLAN.md's
Track C verification notes). The untracked-DLL packaging question this
section pointed at in PLAN.md's Risks section is resolved the same way — see
that section.

---

## 4. Servo API facts, already researched — do not re-derive

Verified against the vendored source at
`C:\Users\hadri\.cargo\registry\src\index.crates.io-1949cf8c6b5b557f\`.

**Resource interception exists and is the intended mechanism** —
`servo-0.1.0/webview_delegate.rs:1016`:

```rust
fn load_web_resource(&self, _webview: WebView, _load: WebResourceLoad) {}

// WebResourceRequest: method, headers, url, is_for_main_frame, is_redirect
impl WebResourceLoad {
    fn request(&self) -> &WebResourceRequest;
    fn intercept(self, response: WebResourceResponse) -> InterceptedWebResourceLoad;
}
impl InterceptedWebResourceLoad {
    fn send_body_data(&mut self, data: Vec<u8>);
    fn finish(self);
    fn cancel(self);   // a network error — this is how you block
}
```

`WebResourceResponse::new(url)` with `.status_code()`, `.headers()`,
`.status_message()` builders. Types live in `servo-embedder-traits-0.1.0/lib.rs`
(637–713), re-exported by `servo`. Exercised by servo's own test
`test_web_resource_load` (`webview_delegate.rs:1160`).

**Navigation** — `NavigationRequest { pub url: Url, .. }` with `.allow()` and
`.deny()`. No "ignore".

**Notification hooks on `WebViewDelegate`** (all exist):
`notify_url_changed`, `notify_page_title_changed`, `notify_status_text_changed`,
`notify_load_status_changed`, `notify_favicon_changed` (no payload — re-read via
`WebView::favicon()`), `notify_history_changed`, `notify_traversal_complete`.

**Getters on `servo::WebView`:** `load_status()`, `url()`, `status_text()`,
`page_title()`, `favicon()`.

**`InputEvent` variants available and currently unused:** `Ime`,
`MouseLeftViewport`, `Wheel`, `EditingAction`. See [PLAN.md](PLAN.md) §A5 —
scroll is on the wrong API today, and text input cannot produce capital letters.

**`UserContentManager` cannot express a CSP** — only `add_script` /
`add_stylesheet`. Block remote content through interception, not injected CSS.

**Zero-copy compositing is available:**
`OffscreenRenderingContext::render_to_parent_callback()` at
`servo-paint-api-0.1.0/rendering_context.rs:748`, and eframe here already uses
the `glow` renderer. That is §A6.

---

## 5. What is done

| Commit | What |
|---|---|
| `0bbfd2d` | WIP from the `mail` checkout committed; manifest fixed (dup `rusqlite` key, and 0.39 → 0.37 for the `links = "sqlite3"` conflict) |
| `5afc2a1` | 4 egui deprecations cleared |
| `e674ba3` | Prior-art survey |
| `621bfbf` | Crate scoped to internal use |
| `11c815f` | **A1** — Cargo workspace split |
| `06db971` | **A2** — `WebViewHost` / `WebView` split, N views, `Result` constructors, per-view textures, fail-open navigation fixed, drop order fixed |
| `fd0448a` | A3/A4 settled against the vendored source |
| `a8cd40c` | Screenshot dumps; A5/A6 rewritten from servoshell research |
| `5825a0c` | Preview mode |
| `9143154` | Size derived from the painted rect; 13 unit tests; log filtering |
| `237fc42` | Mark phases 0-1 done in the plan; add HANDOFF.md |
| `3e908f7` | **A3, A4** — `NavigationPolicy` / `WebViewHandler` (real resource interception via `load_web_resource`; every `notify_*` hook now emits a `WebViewEvent`), full navigation API (`reload`, `go_back`/`go_forward`, `url`/`page_title`/`status_text`/`favicon`/`load_status`), `WebViewSource::HtmlWithBase` for relative links |
| `be05ebf` | **B1** — `config.rs`/`secrets.rs`: TOML `Config` in the platform config dir, passwords in the OS keyring, legacy `esmail_config.txt` migration, inline saved-accounts list on the login screen |
| `e3b695f` | **B2 (partial)** — `req_id` on `FetchHeaders`/`FetchBody`, dropped when stale; `Disconnected` event; `ensure_connected` auto-reconnects with backoff using remembered credentials. Session pool + IDLE not done — see PLAN.md §B2 |
| `f8cd3f9` | **B3 (partial)** — real `mailboxes`/`messages`/`bodies` schema, LRU-capped bodies, FTS5 fixed (was building its mailbox filter with `format!()` — SQL injection, now a bound param), `sync_decision` (pure, tested) fed by UIDVALIDITY/UIDNEXT `fetch_headers` already had. Nothing acts on a `FetchFrom`/`Resync` yet — see PLAN.md §B3. **B4 (partial)** — `search_query.rs`'s DSL parser + FTS5 `MATCH` builder, wired into the search box. `since:`/`before:`/`is:unread`/`has:attachment` parse but aren't applied; server-side `UID SEARCH` not wired — see PLAN.md §B4. Also: untracked the accidentally-committed `mails.db`. |
| `85f7d05` | **B5** — `render.rs`'s parse→sanitize→resolve-`cid:` pipeline (`ammonia`, 9 tests), replacing the duplicated `find_html`/`find_text` in `imap.rs` and the unescaped `format!("<pre>{}</pre>", text)` fallback. `egui-servo-webview`'s `WebViewHandler::intercept` gained a real `Block` outcome (it could only Allow/Serve before — a gap A3 left, closed here); `MessageViewHandler` in `main.rs` uses it to block remote `http(s)` requests by default, with a "Load remote images" button per message. Per-sender allowlist not done — see PLAN.md §B5. |
| `7199ea3` | **B6 (partial)** — `render::extract_attachments` (6 tests), a chip row (filename/MIME/size) with `Save…` (`rfd`)/`Open` (temp file + `opener`) per attachment. Only wired for direct message opens, not cached search results; lazy `BODY.PEEK[n]` fetch not done — see PLAN.md §B6. Self-review before committing caught `open_attachment` joining the message's own (attacker-controlled) filename onto a path unsanitized — a crafted `"../../../x"` could write outside the temp dir; fixed with `safe_attachment_filename` (4 tests) before this landed. |
| `785c2da` | **B7 (partial)** — `smtp.rs` (`SmtpActor` + `lettre`) sends plain-text mail, with attachments as `multipart/mixed`; `compose.rs` derives Reply/Reply All/Forward (subject prefixing, `In-Reply-To`/`References` from a new `MailHeader.message_id`, plain-text quoting) — 9+9 unit tests. Compose window, Reply/Reply All/Forward buttons, and SMTP Host/Port login fields wired into `main.rs`. IMAP `APPEND` to Sent/Drafts, a real retry queue, rich-text composing, and recipient autocomplete not done — see PLAN.md §B7. Caught in self-review: `messages.message_id`'s `CREATE TABLE IF NOT EXISTS` migration would have silently no-opped against this session's own pre-B7 local `mails.db`, breaking `index_mail` at runtime; fixed with an idempotent `ALTER TABLE` step (2 tests) before this landed. |
| `a54653e` | **A5 (partial)** — real character input (`text_to_keyboard_events` from `egui::Event::Text`, replacing the lowercase-only guess from `egui::Key`), focus now via `request_focus`/`has_focus` instead of hover, `MouseLeftViewport` on pointer exit, right/middle mouse buttons. `Scroll::Delta`→`Wheel` migration deliberately held back — see PLAN.md §A5 on the sign-convention risk. IME/cursor/clipboard not done. |
| `7599249`, merged as `988beb9` | **B10** — Windows tray icon + toast notifications for new mail (a background agent's work, merged into this branch). `notify.rs` (pure watermark/toast-text logic), `tray.rs` (Windows-only), `imap.rs`'s `PollMailbox`/`FetchNewHeaders`, `spawn_new_mail_watch` polling INBOX every 60s, minimize-to-tray. See PLAN.md §B10 for the full scope and what didn't land. |
| `e590d59`, `ec7bdf4` (merged from another branch/PR) | **`crates/mail-mock-server`** — an in-process IMAP4rev1 + SMTP server (`LOGIN`/`LIST`/`EXAMINE`/`FETCH`/`UID FETCH`/`LOGOUT`, plaintext SMTP with `AUTH PLAIN`), a committed throwaway TLS test CA, seed fixtures, and `crates/esmail/tests/imap_smtp_integration.rs` driving `ImapActor`/`SmtpActor` against it. `esmail` gained a `lib.rs` so the integration test crate can import it. Also fixed: `safe_attachment_filename` (B6) now splits on `/`/`\` manually instead of `std::path::Path`, since `Path`'s separator handling is host-OS-dependent and silently failed to strip a Windows-style path on Linux. This is what unblocked B11. |
| `405851f` | **B11** — IMAP `IDLE`/push (see PLAN.md §B11). `idle_watch.rs`: a dedicated always-on IDLE connection, independent of `ImapActor`'s session, that sends a wake signal on any server push; wired into `main.rs` so `spawn_new_mail_watch` (B10) polls immediately on a push instead of waiting for its 60s timer, which keeps running as a fallback. Added `IDLE` support to `mail-mock-server` itself (`Store::notify` broadcast channel, `imap_server.rs`'s `IDLE` handler) plus an integration test proving a push arrives in low single-digit seconds. Merged into `main` via PR #6. |
| `de711a1` (PR #7) | **B2 (worker-session split)** — see PLAN.md §B2. `imap.rs` gained `spawn_body_worker`, a second independent IMAP connection (own connect/reconnect loop, `ensure_worker_connected`) that `FetchBody`/`BulkDownload` are routed to instead of `ImapActor`'s own session, so they can't block `FetchHeaders`/`FetchMailboxes` behind them any more. Verified with a real concurrency test (`bulk_download_does_not_block_a_concurrent_header_fetch`), not just unit tests. |
| `6db4c18` (PR #7) | **B3 (incremental fetch acted on)** — see PLAN.md §B3. A `DbEvent::SyncPlan::FetchFrom`/`Resync` now triggers a new `ImapCommand::FetchHeadersFrom` (envelope-only `UID FETCH`, kept separate from B10's `FetchNewHeaders` so the cache-sync path can't spuriously trigger a new-mail toast), indexed metadata-only via a new `DbCommand::IndexHeaders`/`index_headers`. Along the way, fixed a real gap in `mail-mock-server`'s `UID FETCH` handler: it only ever supported a single numeric UID with `RFC822`, not the `first:*` range + `ENVELOPE` this needed (and `FetchNewHeaders`/B10 needed too, apparently never exercised against this server until now). UID-based cache paging for the header list itself did not land — see PLAN.md §B3 for why that's scoped as separate follow-on work. |
| `27bbc8c` | **B7 (`APPEND` to Sent)** — see PLAN.md §B7. `smtp.rs`'s `SmtpEvent::Sent` now carries the exact raw bytes that were sent; `main.rs` follows a successful send with `ImapCommand::Append { mailbox: "Sent", raw }`, indexed via a new `ImapEvent::AppendFailed` (kept separate from `Error` so a save failure can't overwrite the "Message sent" status). Added `APPEND` support to `mail-mock-server` (it had none), delivering into the same `Store::deliver` `smtp_server.rs`'s `DATA` handler uses. `\Sent` special-use-flag discovery and drafts still not done — `SENT_MAILBOX` in `main.rs` is a hardcoded `"Sent"`. |
| `254c6bc` | **A6 (partial)** — see PLAN.md §A6. Gated the CPU `read_to_image` readback on a `frame_dirty` flag set by `notify_new_frame_ready`, so `show()` only pays for a full-surface GPU→CPU copy when Servo actually painted a new frame. The zero-copy GL blit (`OffscreenRenderingContext::render_to_parent_callback`) was implemented, compiled, and then reverted once the mandatory screenshot check came back blank — root cause was an architecture mismatch (Servo's `WindowRenderingContext` is its own independent GL context, never shared with `eframe`/`glutin`'s, so the blit had no valid framebuffer to write into), not a coding slip. Full reasoning in PLAN.md §A6 for whoever picks up real GL context sharing next. |
| `a7a7bcb` | **A7 — DONE.** See PLAN.md §A7. `impl egui::Widget for &mut WebView` (a thin wrapper over a new private `show_impl`), `#![warn(missing_docs)]` at the crate root (passes clean), `README.md` (the `WebViewHost`/`WebView` split, fail-open warning, rendering path post-A6, runtime DLL setup), `examples/two_views.rs` (one `WebViewHost`, two independent `WebView`s — proves A2's multi-instance split holds; compiles, not run interactively, no display in this environment). `cargo doc` CI and a polished `examples/minimal.rs` deliberately not added, matching the plan. |
| *(this branch)* | **B9 (partial) — Polish** — see PLAN.md §B9. Error banners (`main.rs`'s new `Banner`/`push_banner`, replacing `status = format!("Error: {e}")`/`"DB Error: {e}"` and giving `AppendFailed` a visible-but-non-clobbering notice for the first time), a Dark/Light/System theme toggle persisted via `config::ThemeMode`, window-geometry persistence (`config::WindowGeometry`, tracked from `egui::ViewportInfo::outer_rect` and saved once on a real close, read back by `main()` before the window is created), and a first-run provider-table wizard (`config::provider_for_email` — gmail.com/outlook.com/yahoo.com/icloud.com/fastmail.com/gmx.com/zoho.com — autofilling the login form's host/port/SMTP fields from just an email address, distinct from B7's mechanical `derive_smtp_host`). Added `crates/esmail/README.md` with the OAuth2-out-of-scope/app-password note the plan calls for. Per-operation progress (generalizing `download_progress`) deliberately deferred — see PLAN.md §B9 for why (B8 was concurrently landing in the same busy part of `main.rs`). |
| *(this branch)* | **A5 (finished)** — see PLAN.md §A5. The `Scroll::Delta`→`InputEvent::Wheel` migration landed: the sign convention was resolved from three vendored doc comments/call sites (`WheelDelta`'s own doc comment, Servo's own `webview_renderer.rs` negating a wheel delta into `Scroll::Delta`, and egui's `ScrollArea` applying `smooth_scroll_delta` as `offset -= delta`), pinned by two new unit tests on the extracted `WebView::scroll_to_wheel_delta` helper rather than needing to be watched live after all. IME (`egui::Event::Ime` → `InputEvent::Ime` via a new pure `egui_ime_to_servo_ime` helper), the cursor-icon delegate hook (`notify_cursor_changed` → `servo_cursor_to_egui_cursor_icon`, applied every frame the pointer hovers the widget since egui resets to `Default` otherwise), and Ctrl/Cmd+C/X/V → `InputEvent::EditingAction` also landed — the OS clipboard itself was confirmed already free (`servo`'s `clipboard` feature is in its `default` list, installing a real `arboard`-backed delegate), so only the shortcut-to-action wiring was missing. 5 new unit tests on top of A5's existing 18 (23 total in the crate). |
| *(this branch)* | **B8 (partial)** — see PLAN.md §B8. Flags (`\Seen` with a 1.2s mark-as-read delay, `\Flagged` star toggle, mark-unread) via a new `ImapCommand::StoreFlags`/`ImapEvent::FlagsUpdated`, finally populating `messages.flags` (the column B3 added and left unpopulated); delete-to-Trash/Archive via `ImapCommand::MoveMessage` (`MOVE` first, `COPY`+`STORE \Deleted`+`EXPUNGE` fallback — only the fallback is verified, since `mail-mock-server` has no `MOVE`); the mailbox tree (`imap::MailboxInfo`/`mailbox_tree`/`flatten_tree`, RFC 6154 special-use attributes with a name-based fallback, INBOX-then-Sent-then-Drafts-then-Archive-then-Junk-then-Trash-then-alphabetical sort); per-mailbox unread counts via `STATUS (UNSEEN)`; multi-select (ctrl toggles, shift range-selects via a pure `select_range` helper); keyboard shortcuts (`j`/`k`/`Enter`/`r`/`a`/`f`/`Del`/`Ctrl+F`/`Ctrl+N`). Extended `mail-mock-server` with `STORE`/`UID STORE`, `COPY`/`UID COPY`, `EXPUNGE`/`UID EXPUNGE`, `STATUS`, `FLAGS` on every envelope fetch, and special-use `LIST` attributes — same "extend the mock server first" pattern B3/B7/B11 each followed. **Not done:** IDLE tied to the *selected* mailbox specifically (B11's `idle_watch` already covers "new-mail push," but stays INBOX-only); wiring the new `SpecialUse` infrastructure into `main.rs`'s hardcoded `SENT_MAILBOX`/`TRASH_MAILBOX`/`ARCHIVE_MAILBOX` (B7's still-open special-use-discovery gap — the data now exists, the wiring doesn't); a real collapsible tree widget (always-expanded flat list instead); `is:unread` in search (B4's gap, now mechanical given `messages.flags` but not reached into). |

State: `cargo check --workspace` clean (no warnings), `cargo test --workspace`
all passing (18 `egui-servo-webview` unit tests, 102 `esmail` lib unit tests
+ 7 `main.rs` unit tests, 14 `imap_smtp_integration` tests plus 2
`#[ignore]`d stress tests, run with `ESMAIL_TEST_CA_TRUSTED=1`), app builds,
runs, screenshots (`ESMAIL_PREVIEW=demo`) and exits cleanly.

---

## 6. Working agreement

- **One phase per commit**, with a message explaining *why*, not just what.
- **Verify before claiming.** Run the command, look at the screenshot, paste the
  real output. Do not report something as working because it compiled.
- **Say what you did not do.** If part of a task is blocked or skipped, state it
  plainly rather than quietly narrowing scope.
- **Do not re-litigate settled decisions**: the crate stays internal (not
  published), `rusqlite` stays at 0.37, and the Servo API facts in §4 are
  verified — trust them.
- When a plan item turns out to be wrong once you see the code, **fix the plan
  in the same commit** rather than silently diverging from it. That has happened
  three times already and each correction is recorded in a commit message.

## 7. One loose end

None currently open. (This section previously tracked resetting the user's
`mail` checkout against an early WIP commit — resolved long ago; both that
branch and the later `mail-mock-server` branch are merged into `origin/main`.
Kept as a placeholder section number since §6 and this file's cross-references
assume it.)

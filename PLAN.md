# Plan: reusable `egui-servo-webview` crate + full IMAP/SMTP mail client

Three tracks. **Track 0 gets the tree building again** — nothing else can be
verified until it does. Track A extracts the Servo webview into a genuinely
reusable egui widget crate. Track B builds the mail client on top of it.

A and B share one hard dependency: the message viewer needs the webview's
navigation-policy and resource-interception hooks (A3) before HTML mail can be
rendered safely, so A3 must land before B5.

---

## Where things actually stand

| Location | State |
|---|---|
| worktree `imap-mail-client-egui-736b94` @ `f46d6ad` | clean; 1009 lines across `lib.rs` / `imap.rs` / `main.rs` |
| main checkout `repos/ebrowser` | uncommitted WIP: `Cargo.toml`, `src/imap.rs`, `src/lib.rs`, `src/main.rs` modified; `src/db.rs` (183 lines) untracked |

The WIP in the main checkout is a half-landed feature set — a `DbActor` over
SQLite for local indexing and search, an `ImapCommand::BulkDownload` that pulls
every message in a mailbox, a mouse-move ordering fix and a real `Code` mapping
in the webview. It is worth keeping — and it compiles fine; only the manifest
around it is broken.

### Track 0 — confirmed build blockers

**The entire breakage is two lines of `Cargo.toml`. No Rust code is broken.**
Confirmed by compiling both the committed tree and the full WIP in a scratch
copy.

1. **Duplicate manifest key.** `ebrowser/Cargo.toml` declares `rusqlite` twice —
   `0.31` at line 15 and `0.39.0` at line 37, both with `bundled`. Cargo rejects
   the manifest outright, before any compilation:

   ```
   error: duplicate key
     --> Cargo.toml:37:1
   ```

2. **`rusqlite 0.39` cannot coexist with Servo.** Behind the duplicate key sits
   the real conflict. Servo already depends on rusqlite itself — `servo-storage`
   requires `rusqlite ^0.37`, which pins `libsqlite3-sys ^0.35`, while
   `rusqlite 0.39` demands `libsqlite3-sys ^0.37`. `libsqlite3-sys` carries
   `links = "sqlite3"`, so Cargo permits exactly one copy in the graph:

   ```
   package `libsqlite3-sys` links to the native library `sqlite3`,
   but it conflicts with a previous package which links to `sqlite3` as well
   ```

   **Fix: delete line 15, and change line 37 to `rusqlite = { version = "0.37",
   features = ["bundled"] }`** so it unifies with Servo's copy. With that single
   edit the whole WIP — `db.rs`, `BulkDownload`, the `lib.rs` input changes —
   compiles with only four deprecation warnings:

   ```
   main.rs:173  egui::menu::bar            -> egui::MenuBar::new().ui(
   main.rs:318  egui::TopBottomPanel       -> Panel::top / Panel::bottom
   main.rs:177  Ui::close_menu             -> ui.close() / ui.close_kind(..)
   main.rs:186  Ui::close_menu             -> ui.close() / ui.close_kind(..)
   ```

   Note the direction of that second one: in this egui version `Panel::top` is
   the *current* API and `TopBottomPanel` is the deprecated alias — the existing
   `egui::Panel::*` usage elsewhere in `main.rs` is correct and should not be
   "fixed" backwards.

   Pinning to `0.37` couples the cache to whatever rusqlite Servo tracks. When
   B3 lands, isolate the cache in its own Servo-free crate (see Risks) so the two
   can diverge again.

3. **Worktrees inherit the broken manifest.** Because the git root is
   `repos/ebrowser` but the package lives in `src/`, and worktrees land under
   `src/.claude/worktrees/`, Cargo walks *up* from any worktree and finds
   `ebrowser/Cargo.toml` as a potential workspace parent — so a broken manifest
   in the main checkout breaks every worktree too:

   ```
   error: failed searching for potential workspace package manifest
   invalid potential workspace manifest: C:\Users\hadri\Documents\repos\ebrowser\Cargo.toml
   ```

   I appended `[workspace]` to this worktree's `Cargo.toml` to cut the search.
   The durable fix is Track A1 (a real workspace at the repo root), which makes
   the parent manifest legitimate instead of accidental.

4. **Committed HEAD is already clean.** `cargo check` at `f46d6ad` in this
   worktree finishes green in 2m15s, so nothing in the committed tree needs
   fixing and phase 0 is a one-commit manifest change, not a triage exercise.
   Clear the deprecation warnings in the same commit while they are only four.

5. **`find_html` / `find_text` are now defined twice** — once in `fetch_body`,
   once in the WIP `bulk_download`. Legal, but lift them to module scope as part
   of B5 rather than letting the copy drift.

6. **`db.rs` is untracked.** Commit it (or explicitly discard it) before
   branching further work, or it will be lost to a stray `git clean`.

---

### Prior art (checked 2026-09-13)

**No egui + Servo webview crate exists.** A crates.io search for `egui webview`
and `servo webview` returns nothing that embeds Servo in egui, so this crate
would be the first. Four things are worth reading before writing A2–A6:

- **[`iced_servo`](https://docs.rs/iced_servo/)** — the closest analogue: Servo
  embedded in Iced through an offscreen rendering context. It independently
  arrived at exactly the A2 split — `ServoRuntime` (shared engine, `Rc`, cheap
  to clone, "passing one to multiple tabs does not duplicate the underlying
  engine") + a per-view `ServoWebViewController` + a `WebViewConfig`. Strong
  confirmation that `WebViewHost` / `WebView` / `WebViewConfig` is the right
  shape. It also has `LoadStatus`, JS evaluation, and a generic `FrameSource`
  trait worth stealing. It does **not** expose navigation policy, resource
  interception, or back/forward — so A3 and A4 are where this crate would go
  further, and there is no reference implementation to copy for them.
- **[servoshell](https://github.com/servo/servo)** — Servo's own demo browser is
  *already egui-based*, making it the single most direct reference for egui
  input forwarding and compositing. Servo upstream actively maintains this
  integration (e.g. servo/servo#45290, forwarding all mouse motion events to
  egui so tooltips dismiss correctly — the same class of bug the WIP mouse-move
  reordering in `lib.rs` fixes). Read its input handling before doing A5.
- **[servo-gtk](https://servo.org/made-with/)**, **Servo-as-a-Qt-widget**, and
  the **Slint WebView component** — three more toolkit embeddings to compare
  API surfaces against.
- **[`tauri-runtime-servo`](https://crates.io/crates/tauri-runtime-servo)** — a
  different embedding style (whole-runtime replacement rather than a widget);
  useful mainly as a check on how much Servo setup can be hidden.

**One concrete correction to A6 from `iced_servo`:** it reads back via
`read_to_image` and uploads into a *persistent* texture. Our code calls
`ui.ctx().load_texture(..)` every frame ([src/lib.rs:207](src/lib.rs:207)),
which allocates a fresh texture each frame rather than reusing one. Retaining a
`TextureHandle` and calling `set()` on it is a cheap, portable win that should
land before any GL-sharing experiment — quite possibly making the `glow-direct`
feature unnecessary.

### Track A blockers in [src/lib.rs](src/lib.rs)

1. **Single-instance by construction.** `ESWebView::new` builds its own `Servo`
   *and* its own `WindowRenderingContext` from `cc`'s raw window handle
   ([src/lib.rs:113](src/lib.rs:113)). Two widgets means two engines fighting
   over one surface. Window size is hardcoded to 1280×720
   ([src/lib.rs:123](src/lib.rs:123)).
2. **Navigation is broken by design.** The delegate allows exactly the first
   navigation and silently drops every later one
   ([src/lib.rs:70](src/lib.rs:70)). In-page links can never load, the host
   cannot express an allow/deny policy, and there is no back/forward/reload/stop.
3. **No lifecycle surface.** `ESWebViewEvent` has one variant. No title, URL,
   load-started/finished, favicon, error, or JS-dialog events.
4. **Per-frame CPU round-trip.** `read_to_image` pulls the whole framebuffer to
   CPU and `load_texture` re-uploads it under the fixed id `"es_webview_fbo"`
   ([src/lib.rs:207](src/lib.rs:207)) — a full-surface copy every frame, and a
   name collision between any two instances.
5. **Input is partial.** `egui::Event::Text` is ignored, so typing into a page
   form produces no characters. Focus is approximated by `hovered() ||
   clicked()`. No clipboard, no IME, no right/middle buttons, no cursor-shape
   feedback. (The WIP already fixes `Code::Unidentified` → real `Code`, and
   reorders mouse-move after button events — fold that in, don't redo it.)
6. **Not a crate.** No separate `Cargo.toml`, no `impl egui::Widget`, no
   examples, no tests, no README, and `new()` panics through four `expect`s.

### Track B gaps

- No SMTP at all (no `lettre` in the tree), no compose, no reply/forward.
- Search exists only as the WIP `DbActor` local index; IMAP `SEARCH` is unused,
  so search can only find what `BulkDownload` already pulled.
- `keyring` is a declared dependency that is never called. The password lives in
  a plain `String` on `EsMailApp` ([src/main.rs:19](src/main.rs:19)) and config
  is a bare 3-line text file in `%APPDATA%`
  ([src/main.rs:264](src/main.rs:264)).
- One `ImapActor` with one session serialises every command — a body fetch
  blocks the header list, and `BulkDownload` blocks the entire UI's data flow
  for the duration of a full-mailbox sync. No IDLE, so no new-mail push. Every
  fetch re-`examine`s the mailbox ([src/imap.rs:206](src/imap.rs:206)).
- No flags (read/unread/starred/deleted), no threading, no attachments
  (`find_html`/`find_text` walk the tree and discard everything else), no `cid:`
  resolution, no drafts.
- **Privacy hole:** raw message HTML goes straight into Servo as a `data:` URL,
  so tracking pixels and remote CSS fire on every message open.

---

## Track A — `egui-servo-webview` as a real crate

### A1. Cargo workspace split
Make `repos/ebrowser/Cargo.toml` a real `[workspace]` with
`crates/egui-servo-webview` (the widget) and `crates/esmail` (the app). The
widget crate depends on `egui`, `servo`, `raw-window-handle`, `euclid`,
`keyboard-types`, `url`, `log` — **not** on `eframe`, `tokio`, or anything
mail-related. Keep the release profile at the workspace root. This also
permanently fixes Track 0 issue (2).

### A2. Decouple from eframe; support N instances
Split into two types:

- `WebViewHost` — owns the `Servo` instance and the `WindowRenderingContext`.
  Built once per window from `impl HasWindowHandle + HasDisplayHandle` plus a
  real size, returning `Result<_, WebViewError>` instead of panicking. An
  `eframe` feature gates a `WebViewHost::from_eframe(cc)` convenience
  constructor so the current call site stays one line.
- `WebView` — one per view, from `host.new_view(WebViewConfig)`, each with its
  own offscreen context and a texture id derived from a monotonic view id
  (fixes the `"es_webview_fbo"` collision).

`host.spin()` runs once per frame; `view.show(ui)` no longer drives the engine
loop, so N views cost one event-loop spin per frame.

### A3. Real delegate surface *(prerequisite for B5)*
Replace the one-shot boolean with a host-supplied policy:

```rust
pub enum NavigationPolicy { Allow, Deny, DelegateToHost }

pub trait WebViewHandler {
    fn navigation(&mut self, url: &Url, kind: NavigationKind) -> NavigationPolicy { .. }
    fn intercept(&mut self, url: &Url) -> Option<InterceptedResponse> { None }
}
```

`intercept` is what lets the mail client serve `cid:` parts from memory and
refuse remote hosts until the user clicks "load images". **Verify against the
pinned `servo` revision** which `WebViewDelegate` hooks can actually supply a
response body; if interception is not exposed at this version, fall back to
rewriting URLs during sanitisation (B5) and record the limitation in the README.

Emit a proper event enum: `LoadStarted`, `LoadFinished`, `TitleChanged`,
`UrlChanged`, `FaviconChanged`, `LinkClicked`, `NavigationBlocked`, `LoadError`,
`CursorChanged`.

### A4. Navigation API
`go_back` / `go_forward` / `can_go_back` / `can_go_forward` / `reload` / `stop` /
`load(source)` / `title()` / `url()`. `WebViewSource::Html` gains an optional
base URL — today it always base64s into a `data:` URL
([src/lib.rs:367](src/lib.rs:367)), which makes every relative link dead.

### A5. Input completeness
Handle `egui::Event::Text` for character input; add right/middle buttons and
double-click; route clipboard copy/cut/paste; adopt egui focus
(`response.request_focus()` on click, key events only while `has_focus()`);
apply `CursorChanged` to `ui.output_mut().cursor_icon`.

### A6. Rendering path
Keep `read_to_image` as the portable default, but (a) re-upload only when Servo
signals a new frame rather than every frame, and (b) investigate sharing the GL
texture with `egui_glow` via `PaintCallback` behind a `glow-direct` feature.
Measure before committing to (b) — the CPU path may be fine at mail-reading
sizes.

### A7. Packaging *(internal — not published)*
The crate stays in this repo; **crates.io publication is explicitly out of
scope**, which drops the need to pin `servo` for publishability, to choose a
redistribution license, or to keep a stable semver surface. "Reusable" here
means a clean boundary another crate in this workspace can depend on — not a
public crate.

What still earns its keep at that bar: `impl egui::Widget for &mut WebView`,
doc comments on the public API (`#![warn(missing_docs)]`, not `deny`), a short
README covering setup and the `WebViewHost` / `WebView` split,
`examples/two_views.rs` because it is the only real proof that A2's
multi-instance work holds, and unit tests for the pure helpers (key mapping,
`source_to_url`, coordinate transform) since those need no Servo build. Drop
the polished `examples/minimal.rs` browser and the `cargo doc` CI job — the
mail client is the demo.

---

## Track B — the mail client

### B1. Account model and secret storage
Replace the 3-line text file with a serde `Config` (TOML) in the platform config
dir (`directories` crate), holding multiple accounts: display name, IMAP
host/port/TLS mode, SMTP host/port/TLS mode, username, auth type. Passwords move
into the already-declared-but-unused `keyring`, keyed by
`(account_id, "imap"|"smtp")`, held as `SecretString` end to end. Add an
accounts dialog; migrate any existing `esmail_config.txt` on first run.

### B2. Session layer rework
Split `ImapActor` into a small pool: one long-lived control session per account
for IDLE and mailbox state, plus a worker session for fetches, so opening a large
message — or running `BulkDownload` — never freezes the header list. Give every
command a request id and echo it on the event, so late replies for a superseded
selection are dropped (today the guard is a mailbox-name string compare,
[src/main.rs:99](src/main.rs:99)). Add auto-reconnect with backoff, a
`Disconnected` event, and cancellation of in-flight work on mailbox change.

### B3. Local cache — finish and harden `db.rs`
The WIP `DbActor` is the right idea; give it the schema the rest of the plan
needs: `accounts`, `mailboxes` (with `uidvalidity` / `uidnext` /
`highestmodseq`), `messages` (envelope + flags + size + thread key), `bodies`
(cached RFC822 blobs, LRU-capped). Sync = compare `UIDVALIDITY` (wipe on
change), fetch UIDs above `uidnext`, refresh flags for the visible window. This
replaces `BulkDownload`'s unbounded "pull everything" with incremental sync, and
replaces the current position-range paging ([src/imap.rs:191](src/imap.rs:191))
with stable UID-based paging. It is also what makes the list render instantly and
makes offline reading possible.

### B4. Search
Two paths behind one search box:

- **Server-side:** `ImapCommand::Search { mailbox, query }` issuing
  `UID SEARCH`, using `ESEARCH` when advertised, with an all-mailboxes fan-out
  mode.
- **Local:** SQLite FTS5 over subject/from/body-text from B3, used when offline
  and for instant-as-you-type results.

Parse a small grammar — bare text → `OR SUBJECT x FROM x`, plus `from:`, `to:`,
`subject:`, `body:`, `since:`, `before:`, `is:unread`, `has:attachment` — into
IMAP search keys and the equivalent SQL. UI: search box above the message list
with result count and a clear button.

### B5. HTML rendering, safely *(depends on A3)*
The pipeline becomes: parse with `mailparse` → pick the best `text/html`
alternative (falling back to `text/plain`, **HTML-escaped** — the current
`format!("<pre>{}</pre>", text)` at [src/imap.rs:244](src/imap.rs:244) injects
unescaped message text into markup) → **sanitise with `ammonia`** (strip
`<script>`, event handlers, `<iframe>`, forms) → rewrite `cid:` references to
inline `data:` URLs from the related parts → rewrite remote `http(s)` image and
CSS URLs to a blocked placeholder → wrap in a base document setting charset, a
readable default font, and `max-width` so wide marketing mail does not force
horizontal scroll.

Add a per-message "Load remote images" bar that re-renders unblocked, plus a
per-sender allowlist. External link clicks keep opening in the system browser via
`LinkClicked`, now backed by a real `NavigationPolicy::Deny` so the view never
navigates itself away from the message.

### B6. Attachments
Enumerate non-inline parts during parse; show a chip row above the body with
filename, MIME type and size; save-as via `rfd`, open-with via `opener`. Fetch
lazily with `BODY.PEEK[n]` instead of pulling the whole `RFC822`
([src/imap.rs:227](src/imap.rs:227) always downloads everything).

### B7. Compose and send
Add `lettre` (`tokio1-native-tls`, `builder`). A compose window with To/Cc/Bcc
(chip entry completed from cached correspondents), Subject, attachment picker and
a body editor. Ship plain-text composing first, then a minimal rich-text layer
(bold / italic / link / list) emitting `multipart/alternative` with a generated
plain-text fallback. Do **not** attempt a WYSIWYG inside the Servo view — that is
a separate project.

Reply / Reply-all / Forward derive recipients, set `In-Reply-To` / `References`
from the source message, and quote the original. On send: submit over SMTP, then
IMAP `APPEND` to Sent (discovered via the `\Sent` special-use flag, name-based
fallback). Drafts autosave via `APPEND` with `\Draft`. Queue sends so a failure
retries rather than losing the message.

### B8. Flags and the rest of the reading experience
`\Seen` on open (with a mark-as-read delay), star/flag toggle, delete → Trash
(move, with `\Deleted` + `EXPUNGE` fallback), archive, mark-unread, multi-select
with shift/ctrl. Unread counts per mailbox. Render the flat `LIST` output
([src/imap.rs:135](src/imap.rs:135)) as a tree by splitting on the server's
delimiter, special-use folders sorted first. IDLE on the selected mailbox for
new-mail push. Keyboard shortcuts (j/k, Enter, r, a, f, Del, Ctrl+F, Ctrl+N).

### B9. Polish
Error banners instead of a status string ([src/main.rs:88](src/main.rs:88)),
per-operation progress (the WIP `download_progress` field generalises here),
dark/light theme, window-geometry persistence, and a first-run wizard that
guesses IMAP/SMTP settings from the email domain via a small built-in provider
table. OAuth2 is explicitly out of scope for v1 — note in the README that Gmail
and Outlook therefore need app passwords.

---

## Sequencing

| Phase | Content | Unblocks |
|---|---|---|
| **0** | **Manifest fix (drop dup key, `rusqlite` → `0.37`); clear 4 deprecations; commit `db.rs`** | **everything** |
| 1 | A1, A2 | all of A |
| 2 | A3, A4 | B5 |
| 3 | B1, B2 | B3, B7 |
| 4 | B3, B4 | B8 |
| 5 | B5, B6 | — |
| 6 | B7 | — |
| 7 | A5, A6, A7, B8, B9 | — |

Phases 2 and 3 are independent and can run in parallel. A5–A7 are deliberately
late: they improve the widget, but nothing in Track B waits on them.

## Risks

- **Servo API churn.** `servo 0.1` is a moving pre-release; A3 and A6 depend on
  hooks that may not exist or may change shape. Pin an exact revision, and design
  A3's interception so B5's sanitiser-rewrite fallback is sufficient alone.
- **Build cost dominates the loop.** Servo is a cold multi-hour build and already
  needs a long apt install in
  [.github/workflows/ci.yml](.github/workflows/ci.yml). The workspace split only
  buys fast test runs if the pure-logic parts (MIME parsing, sanitising,
  search-query parsing, cache) live in a third crate with no Servo dependency —
  worth doing when B3/B4 land. Add `sccache` / `Swatinem/rust-cache` to CI early.
- **`panic = "abort"` in the release profile** means any `expect` in the widget
  kills the app with no unwind. A2's `Result`-returning constructors matter more
  than they look.
- **Provider auth.** Gmail and Outlook have largely disabled password auth;
  without OAuth2 this is usable mainly with app passwords or IMAP-friendly and
  self-hosted providers. Say so up front.
- **The WIP now exists in two places.** It is committed on
  `claude/imap-mail-client-egui-736b94`, and the *same* changes are still sitting
  uncommitted in the `mail` checkout. Editing there diverges from the branch and
  will conflict in `lib.rs`, `imap.rs` and `main.rs`. Once the branch is
  confirmed good, reset the `mail` working tree rather than hand-merging the two.
  The manifest fix was applied to `mail` directly, so that checkout builds either
  way.
- **`libEGL.dll` and `libGLESv2.dll` are untracked and not ignored** at the repo
  root — Servo runtime libraries loose in the working tree. Decide whether they
  are build output (gitignore them) or required redistributables (commit them, or
  fetch them during the build) before they get committed by accident.

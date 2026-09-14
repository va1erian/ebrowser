# Plan: reusable `egui-servo-webview` crate + full IMAP/SMTP mail client

Three tracks. **Track 0 gets the tree building again** — nothing else can be
verified until it does. Track A extracts the Servo webview into a genuinely
reusable egui widget crate. Track B builds the mail client on top of it.

A and B share one hard dependency: the message viewer needs the webview's
navigation-policy and resource-interception hooks (A3) before HTML mail can be
rendered safely, so A3 must land before B5.

---

> **Status: phases 0-2 are done.** See [HANDOFF.md](HANDOFF.md) for how to
> pick this up, including the landmines that cost real time. Sections marked
> **DONE** below are kept for the reasoning, not as work remaining.

## Where things actually stand

*(Written before any work started; kept because the reasoning still explains
why the code looks the way it does.)*

| Location | State |
|---|---|
| worktree `imap-mail-client-egui-736b94` @ `f46d6ad` | clean; 1009 lines across `lib.rs` / `imap.rs` / `main.rs` |
| main checkout `repos/ebrowser` | uncommitted WIP: `Cargo.toml`, `src/imap.rs`, `src/lib.rs`, `src/main.rs` modified; `src/db.rs` (183 lines) untracked |

The WIP in the main checkout was a half-landed feature set — a `DbActor` over
SQLite for local indexing and search, an `ImapCommand::BulkDownload` that pulls
every message in a mailbox, a mouse-move ordering fix and a real `Code` mapping
in the webview. It was worth keeping — and it compiled fine; only the manifest
around it was broken. It is now committed on this branch as `0bbfd2d`.

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

5. ~~**`find_html` / `find_text` are now defined twice**~~ **DONE (B5)** — was
   once in `fetch_body`, once in the WIP `bulk_download`. Both now call
   `render::render_message`, which owns a single copy of each.

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

### A1. Cargo workspace split — **DONE** (`11c815f`)
Make `repos/ebrowser/Cargo.toml` a real `[workspace]` with
`crates/egui-servo-webview` (the widget) and `crates/esmail` (the app). The
widget crate depends on `egui`, `servo`, `raw-window-handle`, `euclid`,
`keyboard-types`, `url`, `log` — **not** on `eframe`, `tokio`, or anything
mail-related. Keep the release profile at the workspace root. This also
permanently fixes Track 0 issue (2).

### A2. Decouple from eframe; support N instances — **DONE** (`06db971`)
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

### A3. Real delegate surface *(prerequisite for B5)* — **DONE**
Replace the one-shot boolean with a host-supplied policy:

```rust
pub enum NavigationPolicy { Allow, Deny, DelegateToHost }

pub trait WebViewHandler {
    fn navigation(&mut self, url: &Url, kind: NavigationKind) -> NavigationPolicy { .. }
    fn intercept(&mut self, url: &Url) -> Option<InterceptedResponse> { None }
}
```

**Confirmed available in `servo 0.1.0` — no fallback needed.** The open question
from the first draft is settled: `WebViewDelegate` has a first-class
interception hook, exercised by servo's own `test_web_resource_load`.

```rust
// webview_delegate.rs:1016 — fires for every resource load in the view
fn load_web_resource(&self, _webview: WebView, _load: WebResourceLoad) {}

// WebResourceRequest gives: method, headers, url, is_for_main_frame, is_redirect
impl WebResourceLoad {
    fn request(&self) -> &WebResourceRequest;
    fn intercept(self, response: WebResourceResponse) -> InterceptedWebResourceLoad;
}
impl InterceptedWebResourceLoad {
    fn send_body_data(&mut self, data: Vec<u8>);  // serve bytes from memory
    fn finish(self);
    fn cancel(self);                              // network error == blocked
}
```

That is exactly what B5 needs: block a remote image by intercepting and
`cancel()`ing it, serve a `cid:` part by intercepting and `send_body_data()`ing
the attachment bytes. Both are the intended mechanism, not a workaround.

**Addendum from B5:** the A3 commit's actual `WebViewHandler::intercept`
signature was `fn intercept(&mut self, request: &WebResourceRequest) ->
Option<InterceptedResponse>` — `Some` to serve substitute bytes, `None` to let
the load through. That has no way to express the `cancel()` case this section
already called for, so it could serve or allow but never block. Fixed when B5
needed it: `intercept` now returns `InterceptOutcome { Allow, Block,
Serve(InterceptedResponse) }`, and `Block` calls `load.intercept(..).cancel()`
— the same primitives this section names, just actually reachable from a
`WebViewHandler` now.

**The safety-critical detail: both hooks fail OPEN.** `NavigationRequest`'s
`Drop` impl sends *allow*, and an unhandled `WebResourceLoad` sends
`DoNotIntercept`. Dropping either permits the thing you meant to block. Any
view showing untrusted mail must handle every load explicitly — the type
system will not remind you. (This already bit us: see the A2 commit, where
`drop(request)` was silently allowing link navigations.)

Emit a proper event enum. Every one of these is backed by a real hook —
`notify_url_changed`, `notify_page_title_changed`, `notify_status_text_changed`,
`notify_load_status_changed`, `notify_favicon_changed` (no payload; re-read via
`WebView::favicon()`), `notify_history_changed`, `notify_traversal_complete`.

### A4. Navigation API — **DONE**
All confirmed present on `servo::WebView`: `load`, `reload`, `can_go_back`,
`go_back(amount)`, `can_go_forward`, `go_forward(amount)` (the `can_*` are cheap
index checks on the in-memory back/forward list), plus getters for `url()`,
`page_title()`, `status_text()`, `favicon()` and `load_status()`.

**`stop()` does not exist in `servo 0.1.0`.** There is no way to cancel an
in-flight load once it has started — the only control points are up front, via
`request_navigation` and `load_web_resource`. Drop it from the planned API
rather than faking it, and say so in the README.

`WebViewSource::Html` gains an optional base URL — it currently always base64s
into a `data:` URL, which makes every relative link dead.

### A5. Input completeness
Every item below was checked against servoshell — Servo's own egui-based
browser — and against the vendored `InputEvent` enum, which has
`Keyboard`, `Ime`, `MouseButton`, `MouseMove`, `MouseLeftViewport`, `Wheel`,
`Touch` and `EditingAction`. Nothing here needs winit access: where servoshell
reads a raw winit event, egui has already translated the same thing for us.

- **Text input is the worst of it.** `egui_key_to_keyboard_types` maps
  `egui::Key::A..Z` to hardcoded *lowercase* characters, so the widget can
  never produce `@`, `É`, `€`, or any shifted symbol — capital letters
  included. servoshell never does this: winit hands it a fully layout- and
  shift-resolved string. Our equivalent is `egui::Event::Text(String)`, which
  egui-winit derives from exactly the same source. Forward `Event::Text` as
  `Key::Character(text)` and stop deriving characters from `egui::Key`, which
  carries no case information. Keep `Event::Key` for named keys, `code`, and
  modifiers.
- **IME** is a separate channel: forward `egui::Event::Ime` to
  `InputEvent::Ime(ImeEvent::Composition { .. })`, mirroring servoshell's
  Start / Update / End / Dismissed states. Without it, dead keys and any CJK
  input are impossible.
- **Focus:** servoshell routes keys to the page whenever *no egui widget* holds
  `ctx.memory().focused()`. Our `hovered() || clicked()` test leaks keystrokes
  into the page merely because the pointer is over it — type in a sibling text
  field with the mouse resting on the message body and the page gets the keys
  too. Call `request_focus(resp.id)` on click and gate on `has_focus()`.
- **Pointer exit:** send `InputEvent::MouseLeftViewport` when the pointer
  leaves the widget rect. We currently just stop sending moves, which leaves
  stale `:hover` state stuck in the page.
- **Scroll is on the wrong API.** We call
  `notify_scroll_event(Scroll::Delta(..))`, which is the *touch-pan* path
  servoshell uses only in its mobile port; desktop servoshell sends
  `InputEvent::Wheel(WheelEvent)`. The difference is visible to pages: our path
  never fires a DOM `wheel` event, so no page can `preventDefault()` it and any
  custom scroll handling silently breaks. Switch to `InputEvent::Wheel`. That
  also sidesteps the sign convention our code currently asserts in a comment
  and has never verified.
- **Cursor:** implement the cursor-change delegate hook and map Servo's
  `Cursor` onto `egui::CursorIcon` via `ctx.set_cursor_icon`. servoshell does
  the same thing through winit; egui exposes the equivalent.
- **Clipboard:** libservo ships a default `ClipboardDelegate` that talks to the
  OS clipboard, and servoshell relies on it rather than implementing its own.
  We likely get it for free — verify, then add servoshell's
  Ctrl/Cmd+X/C/V → `InputEvent::EditingAction(Cut/Copy/Paste)` shortcuts.
- Also add right/middle buttons and double-click, which nothing upstream
  needed to teach us.

### A6. Rendering path
**The zero-copy path exists and we are already on the backend it needs.**
servoshell does not read pixels back to the CPU at all; it registers an
`egui::PaintCallback` and lets Servo blit its framebuffer straight into egui's
GL context. The hook is real in our vendored version:

```rust
// servo-paint-api-0.1.0/rendering_context.rs:748
impl OffscreenRenderingContext {
    pub fn render_to_parent_callback(&self) -> Option<RenderToParentCallback>
}
```

It returns a closure that blits the offscreen framebuffer into a target rect
given a `glow` context — and our eframe is configured with the `glow` renderer,
so `painter.gl()` hands us exactly that. This replaces the whole
`read_to_image` → `ColorImage` → texture-upload block with a `PaintCallback`,
removing a full-surface GPU→CPU readback stall plus an RGBA copy every frame.

Keep the CPU readback behind a fallback path — `render_to_parent_callback`
returns `Option`, and a wgpu-backed eframe would need a different mechanism
entirely — but the GL path should be the default, not an experiment. (A2
already landed the cheap half of the old plan: the texture is allocated once
and reused rather than reallocated per frame.)

Still worth doing on the fallback path: only re-read when
`notify_new_frame_ready` has fired since the last blit, rather than
unconditionally every `show()`.

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

### B1. Account model and secret storage — **DONE**
Replace the 3-line text file with a serde `Config` (TOML) in the platform config
dir (`directories` crate), holding multiple accounts: display name, IMAP
host/port/TLS mode, SMTP host/port/TLS mode, username, auth type. Passwords move
into the already-declared-but-unused `keyring`, keyed by
`(account_id, "imap"|"smtp")`, held as `SecretString` end to end. Add an
accounts dialog; migrate any existing `esmail_config.txt` on first run.

**One divergence from the wording above:** "an accounts dialog" turned out to
be more than this phase needs — the login screen already had nowhere to put a
modal, and a separate window is real UI work with nothing else in the plan
depending on it yet. Landed instead as a saved-accounts list inline on the
login screen (pick one to prefill + pull its password from the keyring; a
small "x" to forget it) plus the `Config`/`secrets` modules a real dialog would
sit on top of later. `auth_type` also isn't in `AccountConfig` yet — password
auth is the only kind that exists, so a field with one legal value would be
dead weight; add it when B7/OAuth needs to distinguish. `smtp_host`/`smtp_tls`
are in the struct (defaulted) since B7 needs the field to exist, but nothing
reads them yet.

### B2. Session layer rework — **PARTIALLY DONE**
Split `ImapActor` into a small pool: one long-lived control session per account
for IDLE and mailbox state, plus a worker session for fetches, so opening a large
message — or running `BulkDownload` — never freezes the header list. Give every
command a request id and echo it on the event, so late replies for a superseded
selection are dropped (today the guard is a mailbox-name string compare,
[src/main.rs:99](src/main.rs:99)). Add auto-reconnect with backoff, a
`Disconnected` event, and cancellation of in-flight work on mailbox change.

**What actually landed, and what did not:** the request-id plumbing, the
`Disconnected` event, and auto-reconnect with exponential backoff are done —
`ImapCommand::FetchHeaders`/`FetchBody` now carry a `req_id` echoed on their
reply, `EsMailApp` only applies the reply matching its
`current_headers_req`/`current_body_req`, and `ImapActor::ensure_connected`
retries a dropped connection (5 attempts, 1s→16s backoff) using remembered
credentials before any command that needs a session.

**The single-session/worker-pool split and IDLE did not land.** There is
still exactly one `async_imap::Session`; a body fetch still blocks the header
list, and `BulkDownload` still blocks everything else for its duration. This
was cut deliberately rather than attempted blind: splitting into a
control+worker pool and adding IDLE is a large, failure-prone rewrite of live
network code, and this environment has no real IMAP server to test it
against — landing it un-verified risked a subtly broken actor that looks fine
in `cargo check`. "Cancellation of in-flight work on mailbox change" is
covered only in the sense that a stale reply is now dropped by `req_id`, not
in the stronger sense of interrupting an in-flight fetch (servo 0.1.0's
webview has the same limit — no `stop()` — noted at A4). The session split
is real, standalone work; pick it up as its own phase, ideally with a way to
exercise it against a live or mock IMAP server.

**One more simplification worth knowing about:** any error from
`fetch_mailboxes`/`fetch_headers`/`fetch_body`/`bulk_download` clears
`self.session`, not just IO/TLS-level failures. There is no clean way to tell
"the connection died" apart from "the server said no" once both have gone
through `anyhow`'s `?` a few layers up, so this errs toward self-healing: a
transient protocol error (e.g. a mailbox that no longer exists) now costs a
full reconnect instead of just an error message, which is wasteful but never
leaves the actor stuck. Worth revisiting once real error variants are threaded
through instead of `anyhow::Error`.

### B3. Local cache — finish and harden `db.rs` — **PARTIALLY DONE**
The WIP `DbActor` is the right idea; give it the schema the rest of the plan
needs: `accounts`, `mailboxes` (with `uidvalidity` / `uidnext` /
`highestmodseq`), `messages` (envelope + flags + size + thread key), `bodies`
(cached RFC822 blobs, LRU-capped). Sync = compare `UIDVALIDITY` (wipe on
change), fetch UIDs above `uidnext`, refresh flags for the visible window. This
replaces `BulkDownload`'s unbounded "pull everything" with incremental sync, and
replaces the current position-range paging ([src/imap.rs:191](src/imap.rs:191))
with stable UID-based paging. It is also what makes the list render instantly and
makes offline reading possible.

**What landed:** the real schema — `mailboxes` (`uid_validity`/`uid_next`/
`highest_modseq`), `messages` (envelope + size + a `flags`/`thread_key` column,
both unpopulated until B7/B8 need them), `bodies`, all keyed by
`(account_id, mailbox, uid)` — plus an LRU cap on `bodies` (2000 rows,
oldest `cached_at` evicted first) and `messages_fts`, a proper FTS5 mirror
kept in sync on every write instead of being the only table. `imap.rs`'s
`fetch_headers` already calls `session.examine()`, which parses
`UIDVALIDITY`/`UIDNEXT` off the server's own untagged response — that ride
along for free as `MailboxState`, and `db.rs`'s `sync_decision` (pure, unit
tested) turns "previous state, what the server just said" into
`UpToDate`/`FetchFrom`/`Resync`, wiping the cache on a `Resync` before the
caller can re-populate it.

**Also fixed in passing:** the local search's mailbox filter used to build
its `WHERE` clause with `format!("mailbox = '{}'", mb)` — an IMAP mailbox
name spliced straight into SQL, reachable from the server. It's a bound
parameter now (regression test:
`db::tests::search_mailbox_filter_does_not_allow_sql_injection`). Also:
`mails.db` was accidentally committed (an empty schema, no real data in it —
checked) as a stray runtime artifact; it's untracked and gitignored now.

**What did not land:** nothing yet *acts* on a `FetchFrom`/`Resync` decision.
`DbEvent::SyncPlan` is computed and logged, but no incremental UID fetch is
issued in response — `BulkDownload` is still the only way to pull more than
the current page, and still pulls the whole mailbox unconditionally every
time. Turning a `SyncPlan` into an actual `UID FETCH` request, and switching
header paging from sequence-number ranges to UID-based ranges served from the
local cache (so the list renders instantly and works offline), is real
follow-on work — the DB-side half above is what it needs to build on, but
doing the IMAP-side half blind (no live server here to verify it against)
felt like the wrong tradeoff, same reasoning as B2's deferred session split.

### B4. Search — **PARTIALLY DONE**
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

**What landed:** the grammar, in `search_query.rs` — `ParsedQuery::parse`
handles all of `from:`/`to:`/`subject:`/`body:`/`since:`/`before:`/
`is:unread`/`has:attachment` plus bare text and quoted phrases (with `""` as
an escaped literal quote, the usual SQL/FTS5 convention), fully unit tested.
`ParsedQuery::to_fts_match` turns the fielded and bare-text parts into an
FTS5 `MATCH` expression against `messages_fts` — bare text becomes
`(subject:x OR from_addr:x OR body:x)` per the plan's wording above, fielded
terms target only their own column, joined with `AND`.

**What did not land:** `since:`/`before:`/`is:unread`/`has:attachment` parse
correctly but are not applied to the query — `messages.date` is still a raw
IMAP envelope date string, not a comparable timestamp, and `messages.flags`
carries nothing yet (that's B8). A query made only of those (e.g.
`is:unread`) is currently equivalent to an empty query. The **server-side**
`UID SEARCH`/`ESEARCH` path is not wired at all: turning a `ParsedQuery` into
IMAP search keys is mechanical, but issuing it is live-network code with
nothing here to verify it against, so — again, matching B2 and B3's
reasoning — it waited rather than landing unverified. A result count next to
the (already-existing) Clear button is also still missing from the UI.

### B5. HTML rendering, safely *(depends on A3)* — **DONE** (allowlist deferred)
The pipeline becomes: parse with `mailparse` → pick the best `text/html`
alternative (falling back to `text/plain`, **HTML-escaped** — the current
`format!("<pre>{}</pre>", text)` at [src/imap.rs:244](src/imap.rs:244) injects
unescaped message text into markup) → **sanitise with `ammonia`** (strip
`<script>`, event handlers, `<iframe>`, forms) → rewrite `cid:` references to
inline `data:` URLs from the related parts → rewrite remote `http(s)` image and
CSS URLs to a blocked placeholder → wrap in a base document setting charset, a
readable default font, and `max-width` so wide marketing mail does not force
horizontal scroll.

Blocking happens in `load_web_resource` (A3), not via injected CSS: Servo's
`UserContentManager` can add user stylesheets and scripts but has **no CSP
API**, and CSS cannot stop a network fetch — hiding an `<img>` still loads it.
A real `Content-Security-Policy` header can be attached to the intercepted
main-document response if we want belt-and-braces, but interception alone is
sufficient and more precise.

Add a per-message "Load remote images" bar that re-renders unblocked, plus a
per-sender allowlist. External link clicks keep opening in the system browser via
`LinkClicked`, now backed by a real `NavigationPolicy::Deny` so the view never
navigates itself away from the message.

**What landed:** `render.rs` (parse → pick html/escaped-plain → sanitize →
resolve `cid:`), 9 unit tests covering script/iframe/form stripping, event-
handler stripping, plain-text escaping, and `cid:` resolution (including an
unmatched `cid:` staying inert rather than being guessed at). `find_html`/
`find_text` are no longer duplicated in `imap.rs`'s `fetch_body` and
`bulk_download` — both call `render::render_message` now (Track 0's
build-blocker list item 5). Blocking is real: `WebViewHandler::intercept`
gained a `Block` outcome (see A3's addendum above) that
`MessageViewHandler` in `main.rs` uses to cancel every `http(s)` request
unless the per-message "Load remote images" button (also landed) flipped it
open — the message's own `WebView` is reused for every message and reloaded
in place rather than rebuilt, so this is one shared, mutable handler rather
than a fresh one per message.

**One deliberate change from the wording above:** remote `http(s)` URLs are
**not** rewritten to a placeholder in the markup. That instruction predates
A3's settlement (the "Blocking happens in `load_web_resource`, not via
injected CSS" paragraph right above it) and the two now say different
things — markup rewriting would also delete the URL a later "load remote
images" click needs, so `render.rs` leaves every remote reference exactly as
the message had it and blocking is 100% the network-layer handler's job.

**Not done:** the per-sender allowlist ("always load images from this
sender") — only the per-message toggle landed. Also unexercised: `ammonia`
strips inline `style` attributes/blocks along with everything else outside
its default allowlist (no CSS sanitizer is wired in), so CSS-styled HTML mail
renders as plain formatted text; noted in `render.rs`'s module docs as a
known limitation, not silently accepted.

### B6. Attachments — **PARTIALLY DONE**
Enumerate non-inline parts during parse; show a chip row above the body with
filename, MIME type and size; save-as via `rfd`, open-with via `opener`. Fetch
lazily with `BODY.PEEK[n]` instead of pulling the whole `RFC822`
([src/imap.rs:227](src/imap.rs:227) always downloads everything).

**What landed:** `render::extract_attachments` (6 unit tests) walks the same
parsed structure `render_message` does for leaf parts that are neither the
chosen body nor already resolved into it via `cid:`, decoding each to bytes
in memory — `Content-Disposition: attachment` counts, and so does anything
else with no `cid:` reference, since a part that's neither the body nor
referenced inline has nothing else it could be. Wired into `fetch_body`
(the single-message-open path only — see below), with a chip row per
attachment (filename, MIME type, size) above the message and `Save…` (`rfd`
native file picker) / `Open` (write to a temp file, then `opener::open`) per
chip.

**What did not land:** the lazy `BODY.PEEK[n]` fetch. `imap.rs` still
downloads the whole `RFC822` for every message regardless of whether it has
attachments — `extract_attachments` runs on bytes already in hand, not a
separate targeted fetch. Doing this properly needs `BODYSTRUCTURE` first (to
learn which part numbers exist before fetching any of them), which is new
live-IMAP-protocol code with nothing here to verify it against — deferred for
the same reason as B2/B3/B4's own live-IMAP halves. Also: attachments only
appear when a message is opened via a direct `FetchBody` — a message opened
from a cached search result (`DbEvent::MailFetched`) shows none, because
`bodies` only caches the rendered HTML (B3), not the raw bytes attachments
are extracted from.

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

### B10. New-mail notifications — **DONE** (Windows only; scoped as below)
A new phase, not in the original plan wording above. Windows system-toast
notifications for new mail, landed as a bounded subset rather than the full
"watch every mailbox" feature, for the same reason B2/B3/B4/B6's live-IMAP
halves waited: there is no real or mock IMAP server in this environment to
verify a bigger change against, and this codebase's established pattern (see
those sections) is to ship the safely-scoped part and say plainly what's
left rather than land something unverified.

**The "window closed" problem.** esMail is a plain native eframe/winit app,
not Tauri — there is no framework-level "run in background" mode. A normal
window close ends the process, and a process that no longer exists cannot
show a toast five minutes later. The fix is minimize-to-tray: closing the
window is intercepted and turned into hiding it, with a tray icon (`tray-
icon` crate) offering "Show esMail" and "Quit" so the user can still get the
window back or actually exit. "Notifications work even with the window
closed" therefore really means: the *process* survives a window close (only
the window hides), and the polling/toast machinery is a plain tokio task
independent of whether any window is visible — see the mechanism below.

**Why `tray-icon` + `winrt-notification`, not the other candidates:**
- **`tray-icon`** talks to the OS tray directly (no winit coupling needed —
  it registers its own hidden window and pumps Win32 messages off the same
  thread's event loop that winit already runs), which is exactly the "works
  with eframe's winit loop without fighting it" property the task needed.
  Built with `default-features = false` to drop the Linux-only `gtk`/
  `libxdo` pulls, which are dead weight on a Windows-only feature.
- **`winrt-notification`**, not `notify-rust`: checked first, and as
  published on crates.io `notify-rust` no longer has a Windows backend at
  all (dbus/linux, bsd, mac only) — despite older docs and its own crate
  description implying otherwise. `winrt-notification` is a thin, Windows-
  only wrapper over the real WinRT toast API, and — checked by reading its
  source, not assumed — its `title()`/`text1()` builders run content through
  `xml::escape::escape_str_attribute` before splicing it into the toast's
  XML, so a crafted `Subject`/`From` header can't break out of the markup.
  It's old (pinned to `windows` 0.24.0) but built and worked without
  incident here; `windows` isn't a `links = "sqlite3"`-style singleton
  dependency, so an older pinned copy coexisting with whatever `windows`
  version anything else in the graph wants is not the hazard rusqlite would
  be (see PLAN.md's rusqlite note and HANDOFF.md §3.7).
- **Window-close interception**: `eframe::App` in 0.34.1 has no
  `on_close_event` hook (checked against the vendored source — that method
  doesn't exist on this version's `App` trait). The real, version-verified
  mechanism: check `ctx.input(|i| i.viewport().close_requested())` and, to
  cancel it, send `ViewportCommand::CancelClose` followed by
  `ViewportCommand::Visible(false)` — confirmed by reading
  `eframe-0.34.1/src/native/epi_integration.rs`, which checks exactly that
  command in the full-output it collects right after the frame callback
  runs. Tray "Quit" reverses this: set a flag and send
  `ViewportCommand::Close` again, this time *not* followed by
  `CancelClose`, letting the close proceed and the process exit normally
  through `eframe::run_native`'s return.
- **Keeping the tray responsive while hidden**: `eframe::App::logic()` is a
  real, documented trait method — "called once before each call to `Self::
  ui`, and additionally also called when the UI is hidden, but
  `egui::Context::request_repaint` was called" (its own doc comment, and
  confirmed against `glow_integration.rs`: `update()`/`ui()` are skipped
  when a viewport isn't visible, but the frame callback that eventually
  calls `logic()` is not). `EsMailApp::logic` (Windows-only) is where tray-
  click draining and the close-to-tray redirect live, specifically *because*
  it still runs when the window is hidden; it also calls
  `request_repaint_after(250ms)` on itself so it keeps getting invoked
  promptly instead of waiting for some unrelated event to wake the app up.

**Detection: polling, not IDLE.** IDLE is explicitly out of scope here (see
§B2) — this uses the `ImapActor`/`EsMailApp` pattern already established,
not a new protocol path. A background tokio task (`spawn_new_mail_watch` in
main.rs) sends `ImapCommand::PollMailbox { mailbox: "INBOX" }` on a 60-second
timer whenever connected. `PollMailbox` does a bare `EXAMINE` — the same
free ride `fetch_headers` already takes to read UIDVALIDITY/UIDNEXT off the
untagged response, just without the `(UID ENVELOPE)` fetch that pulls actual
headers — so a poll costs one round trip and no bandwidth for the header
list. **Scoped to INBOX only**, not every mailbox: watching N mailboxes on a
timer is N times the traffic and needs per-mailbox watermark state; INBOX is
the one folder every account has and the one "new mail" conventionally
means. Extending this to other mailboxes (or a user-configurable watch list)
is real follow-on work, not attempted here.

The poll result feeds `notify::update_watermark` — a pure function,
deliberately independent of `db.rs`'s own `sync_decision`/`SyncPlan`: that
machinery drives the SQLite cache and wipes it on a UIDVALIDITY change,
which is a different concern with a different failure mode than "should a
toast pop up," and a lightweight poll for notifications has no reason to
touch the DbActor at all. The watermark is kept in memory only, as a local
inside `spawn_new_mail_watch`'s own task (not a field on `EsMailApp`, not
persisted) — reset to a fresh baseline on every `Connected`/reconnect, which
is what stops a first login (or any reconnect) from "discovering" the whole
mailbox as new mail and firing a toast per message already sitting in the
inbox. Only a UIDNEXT advance under an *unchanged* UIDVALIDITY counts as new
mail; a UIDVALIDITY change resets the baseline instead of computing a
meaningless UID delta across two different numbering schemes.

On a `NewMail` verdict, `ImapCommand::FetchNewHeaders` fetches envelopes for
just the new UID range (`UID FETCH first_new_uid:* (UID ENVELOPE)`) to name
the sender/subject in the toast rather than showing a bare "you have new
mail." `notify::build_notification` turns that into a title/body: one
message names the sender and shows the subject, more than one collapses to
a count rather than naming every sender. Both `notify::update_watermark` and
`notify::build_notification` are pure and unit tested (12 tests) without a
live server, a Windows toast API, or a GUI — `imap.rs` gained a third
duplicate of the envelope-parsing block this needed (`fetch_headers`,
`bulk_download`, and now `fetch_new_headers` all built a `MailHeader` from
an IMAP envelope by hand); three copies was the point at which duplicating
it again stopped being the lower-risk option, so it's factored into
`ImapActor::parse_envelope_header` now, with no behavior change to the two
existing call sites.

**Self-review caught before landing:** a crafted `Subject`/`From` header is
attacker-controlled (the sender's own mail) and ends up verbatim in the
toast. `winrt-notification`'s XML escaping (see above) handles markup
injection, but not *content* — a header containing literal newlines/control
bytes could still display as extra toast lines or otherwise fight the
layout even once safely escaped. `notify::sanitize_toast_field` collapses
control characters (including `\n`/`\r`) to spaces, folds repeated
whitespace, and truncates to 120 characters with an ellipsis before a
subject/sender ever reaches `tray.rs` — covered by a regression test
(`newlines_and_control_characters_collapse_to_a_single_line`) using a
subject engineered to look like a second, fake toast line.

**What landed:** `notify.rs` (pure watermark + toast-text logic, 12 tests),
`tray.rs` (Windows-only: tray icon/menu, toast sending), `imap.rs`'s
`PollMailbox`/`FetchNewHeaders` commands and `MailboxPolled`/`NewHeaders`/
`PollFailed` events plus the `parse_envelope_header` refactor,
`spawn_new_mail_watch` (replacing the old event-forwarding bridge task in
`EsMailApp::new` with the same forwarding behavior plus the watch logic),
minimize-to-tray via `EsMailApp::logic`/`handle_tray`, and the `tray-icon`/
`winrt-notification` workspace dependencies (Windows-only in
`crates/esmail/Cargo.toml`, via `[target.'cfg(windows)'.dependencies]`, so
neither is even compiled on Linux — this repo builds and ships on both, see
`.github/workflows/build.yml`).

**What did not land:**
- **Watching more than INBOX.** See above — a real gap for anyone whose new
  mail lands somewhere else (a filter rule into a different folder, say).
- **The lazy `BODY.PEEK[n]`-style targeted fetch B6 also deferred** doesn't
  apply here since `FetchNewHeaders` only ever fetches envelopes, never
  bodies — but the same "no `BODYSTRUCTURE`-driven partial fetch" limitation
  applies in spirit: there's no way to ask for just-the-fields-a-toast-needs
  more cheaply than `(UID ENVELOPE)` already is.
- **Clicking a toast to open the message.** `winrt-notification` 0.5.1 has
  no activation/click-handling API (its own module doc lists "Actions" as a
  todo) — a toast here is informational only. Getting click-to-open would
  mean a lower-level WinRT toast API (`ToastNotificationManager` +
  activation callbacks) that this crate doesn't expose, a bigger change than
  this phase's scope.
- **A real AppUserModelID.** Toasts show under
  `winrt_notification::Toast::POWERSHELL_APP_ID` (that crate's own
  documented workaround for an app with no installer/Start-menu shortcut),
  so Windows attributes them to "Windows PowerShell" — wrong icon, wrong
  name in Focus Assist / Notification settings. Fixing this needs an
  installer that registers a real AUMID + shortcut, which is B9-adjacent
  packaging work, not part of this phase.
- **Per-viewer notification settings** (quiet hours, disabling toasts,
  choosing which mailboxes to watch) — no UI for any of this yet; the poll
  interval and mailbox are compile-time constants (`NEW_MAIL_POLL_INTERVAL`/
  `NEW_MAIL_POLL_MAILBOX` in main.rs).

**Verified:** `cargo check --workspace` clean (including the Windows-only
dependency edge — `tray-icon`/`winrt-notification` resolve and build
without conflict alongside the rest of the graph), all unit tests passing
(counted in HANDOFF.md's running total). **Not verified — needs a real
Windows machine:** the tray icon actually appearing and being clickable, a
toast actually appearing and looking right, and the close-to-tray/Quit
round trip end to end. This environment has no way to screenshot a native
Windows toast or interact with a live system tray (unlike the webview,
which HANDOFF.md §2's `ESMAIL_SCREENSHOT` mechanism can capture headlessly
— there is no equivalent for OS-chrome UI). A human should click through:
close the window (does it disappear instead of exiting, and does a tray
icon appear?), "Show esMail" from the tray menu (does the window come
back?), leave it running with new mail arriving in INBOX (does a toast show
up within ~60s, with the right sender/subject?), and "Quit" from the tray
(does the process actually exit?).

---

## Sequencing

| Phase | Content | Unblocks |
|---|---|---|
| ~~**0**~~ | ~~Manifest fix; clear 4 deprecations; commit `db.rs`~~ **DONE** | everything |
| ~~1~~ | ~~A1, A2~~ **DONE** | all of A |
| ~~2~~ | ~~A3, A4~~ **DONE** | B5 |
| 3 | ~~B1~~ **DONE**, B2 **partially done** (session pool/IDLE remain, see §B2) | B3, B7 |
| 4 | B3 **partially done** (see §B3), B4 **partially done** (see §B4) | B8 |
| 5 | ~~B5~~ **DONE** (allowlist deferred, see §B5), B6 **partially done** (lazy fetch deferred, see §B6) | — |
| **6** | **B7 — start here** | — |
| 7 | A5, A6, A7, B8, B9 | — |
| *(unordered)* | ~~B10~~ **DONE** (Windows only; INBOX-only polling, see §B10) — independent of B7/B8/B9, landed out of sequence alongside whichever of those another session was mid-way through | — |

Phases 2 and 3 are independent and can run in parallel. A5–A7 are deliberately
late: they improve the widget, but nothing in Track B waits on them.

**Landed outside the phase plan**, because verifying anything visual was
impossible without them:

- **Screenshot dumps** (`a8cd40c`) — F12, or `ESMAIL_SCREENSHOT=<path>` to
  capture non-interactively and exit.
- **Preview mode** (`5825a0c`) — `ESMAIL_PREVIEW=demo|<file>|<url>` renders one
  page full-window with no IMAP account, so the widget can be exercised without
  credentials.
- **13 unit tests + log filtering** (`9143154`) — covering the pure helpers, and
  quietening Servo's benign chatter from 19 lines to 2 per run.

## Risks

- **Servo API churn.** `servo 0.1` is a moving pre-release. The hooks A3 needs
  are confirmed to exist *today* (verified against the vendored 0.1.0 source),
  but they are young and unstable — `stop()` is already missing, and
  `notify_favicon_changed` carries no payload. Pin an exact version and expect
  the delegate signatures to move under us.
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

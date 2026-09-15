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

### A5. Input completeness — **DONE**
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

**What landed:** text input (the item this section calls "the worst of
it") — `egui_key_to_keyboard_types` no longer guesses a lowercase character
for letter/digit keys (it returns `Unidentified`, same as any other unmapped
key; `egui_key_to_code` is untouched, since physical `Code` was never
case-ambiguous), and a new `text_to_keyboard_events` turns each
`egui::Event::Text` into a Down/Up `KeyboardEvent` pair carrying the real,
shift/layout-resolved character. Focus now follows `resp.request_focus()` on
any button down and gates keyboard/text forwarding on `resp.has_focus()`,
replacing the `hovered() || clicked()` approximation that leaked keystrokes
into the page merely because the pointer rested over it. Pointer exit sends
`InputEvent::MouseLeftViewport` so `:hover` state doesn't stick when the
mouse leaves. Right and middle mouse buttons are forwarded alongside Primary
(double-click needed no special handling — two ordinary click sequences in
quick succession already reach the page exactly as two clicks did before,
and page/engine-side double-click detection is not this widget's job). 4 new
unit tests plus a rewrite of the one that documented the old lowercase-only
behavior as a known defect.

**The remaining four items (`Scroll::Delta`→`Wheel`, IME, cursor, clipboard
shortcuts) all landed in a follow-on pass.**

**1. The `Scroll::Delta` → `InputEvent::Wheel` migration**, the one item
above with a real regression risk (the two APIs' sign conventions are
opposite, so migrating naively could silently invert scroll direction, and
nothing in this environment — no synthetic input dispatch, only passive
screenshot rendering — could have caught that live). It was resolved by
reading the actual convention from the vendored source rather than guessing:
- `servo-embedder-traits-0.1.0/input_events.rs`'s `WheelDelta::y` doc
  comment: "A positive value means that the view scrolls up, revealing more
  content above the current viewport" (symmetric wording for `x`).
- `servo-paint-0.1.0/webview_renderer.rs`'s `notify_input_event_handled` is
  where Servo itself turns a *received* `Wheel` event into the `Scroll::Delta`
  that actually moves the page: `let scroll_delta = -wheel_event.delta;`
  (comment: "A scroll delta for a wheel event is the inverse of the wheel
  delta"). This confirms the two APIs are deliberately opposite-signed — the
  risk the deferral above named was real, not hypothetical.
- egui's own sign was the missing third data point. `egui-0.34.1/src/
  containers/scroll_area.rs`'s `ScrollArea` applies `smooth_scroll_delta` as
  `state.offset[d] -= scroll_delta`, and `state.offset` is "how far scrolled
  past the top/left" — so a positive `smooth_scroll_delta.y` *decreases* that
  offset, moving the viewport toward the top. That is the exact same
  direction `WheelDelta::y`'s doc comment describes for a positive value.
  egui's sign therefore already matches `WheelDelta`'s (both are the inverse
  of `Scroll::Delta`'s), so the new `WebView::scroll_to_wheel_delta` is a
  straight scale-to-device-pixels with **no negation on either axis** — the
  opposite of what the old `Scroll::Delta` path did (which negated both axes
  to convert into that API's opposite convention). Pinned by two new unit
  tests (`scroll_to_wheel_delta_does_not_negate_egui_s_sign`,
  `..._handles_negative_scroll_without_a_double_flip`) so a future regression
  here is caught mechanically rather than by eyeballing scroll direction
  again. Verified visually too (see below) — the demo page's tall scrollable
  block responds to `smooth_scroll_delta` correctly after the change,
  scrolling the same direction it did before the migration.
- Keyboard-driven scrolling (arrow keys, Page Up/Down, Home/End) was left on
  `Scroll::Delta`/`Scroll::Start`/`Scroll::End` — that's a distinct,
  already-working, already-shipped code path with no `preventDefault`
  argument for switching it, and PLAN.md's own wording above only ever called
  out the *wheel* path as being on the wrong API.

**2. IME.** `egui::Event::Ime` is forwarded via a new pure helper,
`egui_ime_to_servo_ime`, matching servoshell's Start/Update/End/Dismissed
states: `Enabled`→`Composition(CompositionState::Start)`,
`Preedit(text)`→`Composition(CompositionState::Update)`,
`Commit(text)`→`Composition(CompositionState::End)`,
`Disabled`→`Dismissed`. One subtlety worth recording: `Dismissed` is not a
fourth `keyboard_types::CompositionState` (that enum only has
`Start`/`Update`/`End`) — it's a sibling variant one level up on
embedder_traits' own `ImeEvent` (`Composition(CompositionEvent) |
Dismissed`), which is why `Enabled`/`Disabled` map onto two different Rust
enums rather than all four onto one. 4 new unit tests exercise all four
states through the real helper function.

**3. Cursor.** `WebViewDelegate::notify_cursor_changed` is implemented,
storing the mapped `egui::CursorIcon` in a `Rc<Cell<_>>` shared between the
`Delegate` and `WebView` (the same pattern `frame_dirty` already used).
`show_impl` calls `ctx.set_cursor_icon(..)` every frame the pointer is over
the widget (`resp.hovered()`) — egui resets the cursor to `Default` each
frame otherwise, so this can't be a one-shot "set once when it changes."
`servo_cursor_to_egui_cursor_icon` is an exhaustive match (a new upstream
`Cursor` variant fails it at compile time rather than silently falling back
to `Default`) — the two enums don't share naming conventions for the
diagonal/edge resize cursors (`NeswResize`↔`ResizeNeSw`,
`EwResize`↔`ResizeHorizontal`, etc.), which the mapping test exercises
specifically rather than only the obvious cases.

**4. Clipboard.** Verified, as PLAN.md suspected: `servo`'s `clipboard`
feature is in its `default` feature list
(`servo-0.1.0/Cargo.toml`), which installs a real `arboard`-backed
`DefaultClipboardDelegate` (`clipboard_delegate.rs`) whenever the embedder
doesn't supply its own — this crate doesn't, so the OS clipboard already
works for free, exactly as the plan guessed. What was actually missing was
telling Servo *when* to invoke it: a keydown alone doesn't imply "run the
copy/cut/paste editing command" the way a browser's own accelerator table
does. Added: Ctrl/Cmd+C/X/V (without Shift/Alt) now also dispatch
`InputEvent::EditingAction(Copy/Cut/Paste)` alongside the ordinary
`Keyboard` event already sent for that key. A small pure table
(`action_for` in the test) mirrors the `show_impl` match and is unit tested
independently of a live Servo view.

**Verification.** `cargo test -p egui-servo-webview` — 23 tests (up from 18),
all passing, doc-test included. Screenshotted per HANDOFF.md §2
(`ESMAIL_PREVIEW=demo`, 90 frames) before and after: the demo page (heading,
accented text, link, table, text input, tall scrollable block) renders
identically to the pre-change baseline — this phase touches `show_impl`'s
input-forwarding code, not its paint path, so an unchanged screenshot is the
expected (and confirmed) result, not a null result.

### A6. Rendering path — **PARTIALLY DONE**
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

**PARTIALLY DONE — the zero-copy GL blit was attempted and reverted; the
CPU-path re-read gating landed.** This is exactly the class of change
`HANDOFF.md` §2 warns about ("compiled clean, passed every test, silently
broke rendering"), and the screenshot workflow it insists on is what caught
it here too.

**What was tried:** register an `egui::PaintCallback` (via
`egui_glow::CallbackFn`, the type the `glow` backend downcasts to) whose body
calls `OffscreenRenderingContext::render_to_parent_callback()`'s closure.
Getting it to *compile* required fixing one real thing the plan above got
wrong: it says `painter.gl()` (`egui_glow::Painter::gl()`) "hands us exactly
that" `glow::Context`. It does not — `servo-paint-api` and `egui_glow` pin two
different major versions of the `glow` crate (0.16 vs 0.17, visible in
`Cargo.lock`), so the `&glow::Context` the callback wants is a different Rust
type than what `Painter::gl()` returns. The fix was
`OffscreenRenderingContext::glow_gl_api()` (via the already-imported
`RenderingContext` trait), which hands back servo-paint-api's own `Arc<glow::Context>`
(the 0.16 one), built by loading GL function pointers against whatever GL
context is current at the time. That part worked: it compiled, and `cargo
test -p egui-servo-webview` still passed (18 tests, none of which exercise
`show()` itself — see the note at the top of the test module on why).

**Why it was reverted anyway:** the screenshot workflow HANDOFF.md §2
describes (`ESMAIL_PREVIEW=demo` + `ESMAIL_SCREENSHOT`) came back a uniform,
near-black fill — no heading, no table, no link, no input box, nothing the
demo page renders. Root cause: `WebViewHost::new` builds Servo's
`WindowRenderingContext` with its own call to surfman
(`WindowRenderingContext::new`), which creates a **new, independent native GL
context** bound to the window handle -- it is never shared with the GL
context `eframe`/`glutin` already created for that same window, the one
`egui_glow`'s `Painter` actually compositing into. `glow_gl_api()` loading
function pointers against "whatever context is current" doesn't fix this: at
the moment our `PaintCallback` runs, the current context is `egui_glow`'s, but
`render_to_parent_callback`'s closure was built to blit **Servo's own**
framebuffer object id into **Servo's own** `parent_context`'s framebuffer
(`self.parent_context.surfman_context.framebuffer()`) — a framebuffer object
that lives in Servo's context, not egui_glow's. Framebuffer objects (unlike
textures/buffers) are never shared between GL contexts even when the contexts
share other resources, so binding Servo's FBO id while egui_glow's context is
current either errors or aliases onto whatever unrelated object shares that
numeric id there — consistent with rendering nothing.

This is a real architecture mismatch, not a coding slip: `render_to_parent_callback`
is designed for a caller like servoshell, where Servo's own
`WindowRenderingContext` *is* the window's one and only rendering context and
does the actual `present()`/swap itself. This crate instead sits Servo
underneath `eframe`, which already owns the real window and its GL context;
`WebViewHost`'s `WindowRenderingContext` only exists to be the parent for each
view's *offscreen* context, and was never meant to touch the screen directly.
Making the zero-copy path work for real would mean sharing GL objects between
Servo's context and eframe's `glutin` context at context-creation time (an
explicit share-list, set up before either context exists) — real, deep
surgery on both `WebViewHost::new` and the app's `eframe::NativeOptions`
setup, well past a "safely-scoped subset" for one phase. Left for whoever
picks this up next; the closing paragraph below is what actually landed
instead.

**What landed:** the CPU `read_to_image` path is now gated on a
`frame_dirty: Rc<Cell<bool>>` shared with the `Delegate`, set by
`notify_new_frame_ready` and cleared once the framebuffer has been re-read;
when not dirty, `show()` repaints the existing texture rather than
re-reading, so a `show()` with nothing new costs a texture upload's worth of
GPU work, not a full-surface CPU readback. Screenshot re-verified after the
revert: the demo page (heading, accented text, link, table, text input, tall
scrollable block) renders correctly, matching the pre-A6 baseline.

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

**DONE.** `impl egui::Widget for &mut WebView` is a thin wrapper over a new
private `show_impl` that both it and `WebView::show` call — `Widget::ui` can
only return an `egui::Response`, so it has nowhere to put the
`Vec<WebViewEvent>` `show` returns; that impl's doc comment says so and
points callers who need events back to `show` directly. `#![warn(missing_docs)]`
is on at the crate root and passes clean (no existing public item needed a
comment added beyond what A2–A6 already wrote). `README.md` covers the
`WebViewHost`/`WebView` split, the fail-open navigation/interception warning,
the rendering path (including the reverted GL attempt from A6 above), and
runtime setup (`libEGL.dll`/`libGLESv2.dll`, per `HANDOFF.md` §3.9).
`examples/two_views.rs` creates one `WebViewHost` and two independent
`WebView`s side by side, each on its own page, input, and scroll state, to
prove the A2 split holds — it compiles (`cargo check --example two_views -p
egui-servo-webview`) but was not run interactively in this environment (no
display to watch it on, same constraint noted elsewhere in this file for
input verification); the unit-test suite (18 tests, all pure helpers — key
mapping, `source_to_url`, the coordinate transform, the navigation/
interception default-policy test) already ran under `cargo test -p
egui-servo-webview` and continues to pass, doc-test included. `cargo doc` CI
and a polished `examples/minimal.rs` were not added, matching the plan.

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

### B2. Session layer rework — **PARTIALLY DONE** (session-pool split now landed)
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

**The worker-session split landed once `mail-mock-server` existed to verify
it against.** `ImapCommand::FetchBody`/`BulkDownload` are no longer handled
on `ImapActor`'s own session at all — `spawn_body_worker` (`imap.rs`) owns a
second, independent IMAP connection with its own connect/reconnect loop
(`ensure_worker_connected`, mirroring `ensure_connected`'s backoff shape but
reporting failures only on the specific request that hit them, not as a
global `Disconnected`/`Connected` — see the function's doc for why), spawned
fresh on every successful `Connect`. `ImapActor::run`'s own loop now just
forwards those two command variants to the worker's channel and immediately
goes back to `cmd_rx.recv()`, so `FetchHeaders`/`FetchMailboxes` never wait
behind a body fetch or a bulk download again. Verified with a real
concurrency test (`bulk_download_does_not_block_a_concurrent_header_fetch`
in `imap_smtp_integration.rs`): a `FetchHeaders` sent right after a
100-message `BulkDownload` is answered while the download is still in
progress, not queued behind it. This is the piece of B2 that was
specifically deferred through B7 and B11 for lack of anything to verify a
live-IMAP-protocol rework against — `mail-mock-server` (added since) is what
unblocked it, the same way it unblocked B11's `IDLE` support.

**What's still not the full "one control session per account for IDLE"
wording above:** B11 already gave IDLE its own dedicated connection
(`idle_watch.rs`), separately from this worker split — so there are now
*three* independent IMAP connections per account (primary/header session,
body worker, IDLE watch) rather than the two ("control" + "worker") this
section's original wording pictured. That turned out fine in practice (each
solves a narrower problem than a shared "control" session would have), but
it's worth naming as a divergence from the original plan text rather than
silently different. "Cancellation of in-flight work on mailbox change" is
still covered only in the sense that a stale reply is now dropped by
`req_id`, not in the stronger sense of interrupting an in-flight fetch
(servo 0.1.0's webview has the same limit — no `stop()` — noted at A4).

**One more simplification worth knowing about:** any error from
`fetch_mailboxes`/`fetch_headers` clears `ImapActor`'s own `self.session`
(and, symmetrically, any error from `fetch_body`/`bulk_download` clears the
body worker's local `session` variable — see `spawn_body_worker`), not just
IO/TLS-level failures. There is no clean way to tell "the connection died"
apart from "the server said no" once both have gone through `anyhow`'s `?` a
few layers up, so this errs toward self-healing: a transient protocol error
(e.g. a mailbox that no longer exists) now costs a full reconnect instead of
just an error message, which is wasteful but never leaves the actor (or the
worker) stuck. Worth revisiting once real error variants are threaded
through instead of `anyhow::Error`.

### B3. Local cache — finish and harden `db.rs` — **PARTIALLY DONE** (incremental sync now acted on)
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

**A `FetchFrom`/`Resync` decision is now acted on**, landed once
`mail-mock-server` existed to verify the IMAP-side half against — same
unblocking as B2/B11. `EsMailApp::handle_db_events`'s `SyncPlan` arm sends a
new `ImapCommand::FetchHeadersFrom { mailbox, first_uid }` (an envelope-only,
unpaged `UID FETCH first_uid:* (UID ENVELOPE)`, mechanically the same fetch
`FetchNewHeaders`/B10 already does — kept as a *separate* command/event pair
rather than reused, since `NewHeaders` also drives B10's new-mail toast, and
this fires far more often, including on the user's own routine "open
INBOX"/"hit refresh"; reusing it would toast the user for their own
actions). Its reply (`ImapEvent::HeadersFrom`) is indexed via a new
`DbCommand::IndexHeaders`/`index_headers` — metadata-only, deliberately
leaving `bodies`/`messages_fts` untouched (a row with no cached body
shouldn't become search-findable, nor clobber a real cached body a later
`BulkDownload`/`IndexMail` already wrote for the same UID; regression-tested
by `index_headers_does_not_clobber_an_already_cached_body_or_its_size`).

Finding this wiring required fixing a real, previously-latent gap in
`mail-mock-server` itself: its `UID FETCH` handler only ever supported a
single numeric UID with `RFC822` (enough for `imap.rs::fetch_body`, the only
thing exercising it before now) — not the `first_uid:*` range with
`(UID ENVELOPE)` that `fetch_new_headers`/`fetch_headers_from` actually
send. `FetchNewHeaders` (B10) had apparently never been exercised against
this server either, since nothing caught it until this phase's own
integration test (`fetch_headers_from_returns_envelopes_from_the_given_uid_onward`)
failed with an empty result. Fixed by extending `UID FETCH` to parse a
`start:end`/`start:*` range and an `ENVELOPE` fetch-item, sharing the
existing sequence-number `FETCH` handler's envelope-response building via a
new `envelope_fetch_response` helper instead of a third hand-copied block.

**What still did not land:** switching header-list *paging* from
sequence-number ranges served live from the network to UID-based ranges
served from the local cache (so the list renders instantly and works
offline) — the header list still always re-fetches from the server on every
page/mailbox change; the cache accumulates in the background but the UI
doesn't read from it yet. That's a genuinely separate, larger change (when
to trust the cache vs. re-fetch, pagination consistency, offline behavior)
from "does an incremental fetch happen at all", which is what this phase
scoped down to. Also still open: `FetchBody` (opening a single message) still
doesn't index anything into the cache — only `BulkDownload`/`IndexMail` and
now this phase's `FetchHeadersFrom`/`IndexHeaders` do, so a message opened
one at a time is never searchable until a bulk download also happens to
cover it.

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

### B7. Compose and send — **PARTIALLY DONE** (APPEND to Sent now lands)
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

**What landed:** `smtp.rs` (an `SmtpActor` following `imap.rs`/`db.rs`'s
existing actor-behind-an-mpsc-channel pattern) sends over SMTP via `lettre`,
picking implicit-TLS/STARTTLS/none from `AccountConfig::smtp_tls` (only `Ssl`
is reachable from the UI today — see below) and building either a plain
`SinglePart` or a `multipart/mixed` with `Attachment` parts when there are
attachments. `compose.rs` is the pure (9 unit test) half: `ComposeState` plus
`reply`/`reply_all`/`forward`, which derive the recipient(s), prefix the
subject (without piling up "Re: Re: Re:"), set `In-Reply-To`/`References`
from the original's `Message-ID` (a new field on `MailHeader`, fed from
`envelope.message_id` — already parsed by `async_imap`, same free-lunch as
`MailboxState` in B3), and quote the original body as plain text (stripped
from the already-rendered HTML via `ammonia::Builder::empty()`) with an
attribution line. A plain-text body editor, an attach-file button (`rfd`),
and Reply/Reply All/Forward buttons on the open message are wired into
`main.rs`'s compose window.

`AccountConfig::new` now also guesses `smtp_host` via the `imap.` → `smtp.`
convention (`config::derive_smtp_host`, tested), and the login screen grew
SMTP Host/Port fields (prefilled from that guess, editable) since there was
previously no way to set them at all.

**Caught while wiring `messages.message_id` into `db.rs`'s schema:** the new
column only reaches a *freshly created* `messages` table —
`CREATE TABLE IF NOT EXISTS` does nothing to one an earlier build already
made without it, which described this session's own leftover local
`mails.db` from B3–B6 testing exactly. Without a fix, the first
`index_mail` call against that file would have failed with "table messages
has no column named message_id". Fixed with an idempotent
`ALTER TABLE ... ADD COLUMN` migration step, tested both for idempotency and
against a hand-built pre-B7 `messages` table.

**`APPEND` to Sent landed once `mail-mock-server` existed to verify it
against** — same unblocking as B2/B3/B11. `smtp.rs`'s `SmtpEvent::Sent` now
carries the exact raw RFC822 bytes handed to the transport (`Message::
formatted()`, captured before the message is moved into `transport.send`,
so the appended copy is byte-identical to what was actually sent — not a
second, possibly-diverged call to `build_message`); `main.rs` follows a
successful send with `ImapCommand::Append { mailbox: "Sent", raw }`. A
failed `Append` is reported through a new `ImapEvent::AppendFailed`, kept
separate from the generic `Error` event specifically so it can't overwrite
the "Message sent" status the send's own success already set — the send
and the save are two independent steps, and a save failure shouldn't read
as if the send itself failed.

Landing this needed `mail-mock-server`'s `APPEND` support added alongside
(it had none): `imap_server.rs` reads the `APPEND "<mailbox>" {n}` command
line, sends the `+` continuation, reads exactly `n` literal bytes plus the
client's trailing CRLF, then delivers into `Store` via the same
`Store::deliver` `smtp_server.rs`'s `DATA` handler already uses — an
appended message becomes indistinguishable from one that arrived over SMTP,
which is the correct behavior for what a real server's Sent folder holds.
Verified end to end by a new integration test
(`append_saves_a_sent_copy_that_fetch_headers_can_then_see`): send over
SMTP, `Append` the returned raw bytes to Sent, then `FetchHeaders` on Sent
and confirm the message is there.

**Still not done: `\Sent` special-use-flag discovery, and drafts.**
`SENT_MAILBOX` in `main.rs` is a hardcoded `"Sent"`, not discovered via the
`\Sent` special-use flag (`LIST`'s `\HasNoChildren`/etc. attributes) with a
name-based fallback for servers that don't advertise it — an account whose
Sent folder is actually named something else (`Sent Items`, `Sent Mail`,
locale-dependent names) would get a *new* mailbox silently created next to
its real one, since `Store::deliver`/most real IMAP servers create-on-append
by default. There is also still no draft autosave (`APPEND` with `\Draft`)
— the `APPEND` machinery this phase added is the same primitive drafts
would need, but nothing calls it for that purpose yet.

**What else did not land, and why:**
- **Rich-text composing.** The plan itself sequences this after plain-text
  ("ship plain-text composing first, *then* ..."), so landing only the first
  half matches the plan's own ordering, not a cut corner.
- **A real send-retry queue.** A failed send reports the error and leaves the
  compose window open with everything the user typed intact, so nothing is
  lost — but there's no automatic retry-with-backoff, and nothing survives an
  app restart. Only half of "queue sends so a failure retries rather than
  losing the message" is there.
- **Chip-entry recipients completed from cached correspondents.** To/Cc/Bcc
  are plain comma-separated text fields; no autocomplete against anything
  `db.rs` has seen before.
- **TLS mode picker for SMTP.** `AccountConfig::smtp_tls` exists and
  `smtp.rs` honors it, but the login screen has no control to set it to
  anything but the `Ssl` default `AccountConfig::new` picks — `StartTls`
  accounts (port 587, common for non-Gmail-style providers) can't be
  configured through the UI yet.
- **Reply-All's Cc is best-effort.** `imap.rs`'s envelope parsing only ever
  kept the *first* From/To address (predates B7), so there's no captured
  multi-recipient list to Cc the rest of — an empty or single-address Cc is
  what that limitation looks like, documented in `compose.rs` rather than
  silently under-delivering.

### B8. Flags and the rest of the reading experience — **PARTIALLY DONE**
`\Seen` on open (with a mark-as-read delay), star/flag toggle, delete → Trash
(move, with `\Deleted` + `EXPUNGE` fallback), archive, mark-unread, multi-select
with shift/ctrl. Unread counts per mailbox. Render the flat `LIST` output
([src/imap.rs:135](src/imap.rs:135)) as a tree by splitting on the server's
delimiter, special-use folders sorted first. IDLE on the selected mailbox for
new-mail push. Keyboard shortcuts (j/k, Enter, r, a, f, Del, Ctrl+F, Ctrl+N).

**What landed:** everything in this section's first sentence through
"multi-select with shift/ctrl", plus unread counts and the mailbox tree,
verified against `mail-mock-server` (extended for this phase — see below) the
same way B2/B3/B7/B11 verified their own live-IMAP halves.

- **Flags.** `imap.rs` gained `ImapCommand::StoreFlags`/`ImapEvent::
  FlagsUpdated`/`FlagsUpdateFailed` (`ImapActor::store_flags`: `SELECT`s the
  mailbox — `STORE` needs write access, unlike the `EXAMINE` every read-only
  fetch uses — then issues `UID STORE +FLAGS`/`-FLAGS`, one round trip per
  non-empty side since IMAP has no single verb that both adds and removes
  different flags at once). `MailHeader` gained a `flags: Vec<String>` field
  (plus `is_seen()`/`is_flagged()`) populated by adding `FLAGS` to every
  envelope fetch's item list (`fetch_headers`/`fetch_new_headers`/
  `bulk_download`) — this is the column `db.rs`'s schema has carried since B3
  ("unpopulated until B7/B8 need them") and now actually writes, in both
  `index_mail` and the new `DbCommand::UpdateFlags` (fired once a `StoreFlags`
  is server-confirmed, so the cache doesn't wait for a full re-fetch).
  `\Seen` on open uses a real delay (`MARK_SEEN_DELAY`, 1.2s): `open_message`
  records `(uid, Instant::now())` in `pending_mark_seen`, and
  `handle_mark_seen_delay` (checked once per frame) only fires the `StoreFlags`
  once that's elapsed *and* the same message is still open — arrowing past
  several messages with `j`/`k` faster than that never marks any of them read.
  Star toggle and mark-unread are both `StoreFlags` calls with the flag
  flipped (`\Flagged`/`\Seen` respectively), available both per-message (in
  the open message's own toolbar) and as bulk actions.
- **Delete → Trash / Archive.** `ImapCommand::MoveMessage`/`ImapActor::
  move_message`: `SELECT`s the mailbox, tries the real `MOVE` extension
  (`Session::uid_mv`) first, and on any failure falls back to `COPY` +
  `STORE +FLAGS.SILENT \Deleted` + a bare `EXPUNGE` — the same three steps
  `MOVE` is defined to be equivalent to. **Only the fallback path is verified
  against `mail-mock-server`**, since this mock has no `MOVE` at all (real
  `uid_mv` against it always fails, exercising exactly the fallback branch) —
  a server that *does* support `MOVE` takes the untested-here direct path,
  trusted on the strength of `async_imap`'s own implementation rather than
  this project's own testing. The fallback's `EXPUNGE` is unscoped (not
  `UID EXPUNGE <uid>`, which needs the `UIDPLUS` extension this client
  doesn't check for) — safe under this client's own usage (nothing else here
  marks a message `\Deleted` without immediately expunging it) but would
  expunge *every* `\Deleted` message in the mailbox on a server where some
  other client left one lying around, a real edge case worth naming.
- **Mailbox tree.** `imap::MailboxInfo` (name, delimiter, a best-effort
  `SpecialUse` — from LIST's RFC 6154 attributes when the server advertises
  them, else a name-based fallback for `INBOX`/`Sent`/`Drafts`/`Trash`/
  `Archive`/`Junk`, the same "hardcoded name, documented gap" trade
  `SENT_MAILBOX` already made in B7) replaces the bare `Vec<String>`
  `ImapEvent::Mailboxes` used to carry. `imap::mailbox_tree` (pure, 6 unit
  tests) splits each name on its delimiter and sorts INBOX first, then
  Sent/Drafts/Archive/Junk/Trash, then everything else alphabetically;
  `imap::flatten_tree` turns that into an owned, depth-tagged `Vec` for the
  left panel's immediate-mode list (indented by depth, a plain label for a
  hierarchy node that exists only because a deeper mailbox implies it and
  nothing ever `LIST`ed it directly). `mail-mock-server`'s `LIST` handler now
  advertises `\Sent`/`\Drafts`/`\Trash`/`\Archive`/`\Junk` for its well-known
  mailbox names, so the attribute path (not just the name fallback) has real
  coverage (`connect_and_fetch_mailboxes`, extended).
- **Unread counts.** `ImapCommand::FetchUnreadCounts`/`ImapEvent::
  UnreadCounts`: one `STATUS (UNSEEN)` per mailbox, sent right after every
  `Mailboxes` reply, shown as `"Name  (N)"` in the tree. Kept fresh
  incrementally after that — a `FlagsUpdated`/`Moved` event adjusts the
  affected mailbox's count in place (±1) rather than waiting for the next
  full `FetchUnreadCounts` round trip. Needed `STATUS` support added to
  `mail-mock-server` (it had none), backed by a new `Mailbox::unseen_count`
  on the store side.
- **Multi-select with shift/ctrl.** `selected_uids: BTreeSet<u32>` +
  `select_anchor: Option<u32>` on `EsMailApp`. Plain click replaces the
  selection; ctrl/cmd-click toggles one UID in/out of it (seeding it from the
  previously-single-selected message on the first ctrl-click, so it doesn't
  silently start empty); shift-click extends it to every message between the
  anchor and the click, via a pure `select_range` helper (position-based
  over the currently displayed list, so it degrades to `{uid}` rather than
  panicking if the anchor scrolled off a page that's no longer loaded).
  Every bulk action (Mark read/unread, Star/Unstar, Archive, Delete) reads
  `action_targets()`: the multi-selection when non-empty, else the single
  open message.
- **Keyboard shortcuts.** `handle_keyboard_shortcuts`, called once per frame
  when connected: `j`/`k` move the selection and open it (see below for why
  this folds `Enter`'s job in), `Enter` re-opens the current selection,
  `r` replies to it, `a`/`Del`/`Backspace` archive/delete it (or the whole
  multi-selection), `f` toggles star, `Ctrl+F` focuses the search box
  (`request_focus` on its id, captured where the box is drawn — egui ids are
  scoped to their enclosing panel, so a shortcut handler outside that closure
  needs the actual `Id`, not a freshly-hashed guess at one), `Ctrl+N` opens
  compose. Disabled while the compose window is open or the search box has
  focus, so its own text fields get every keystroke.

**One deliberate simplification from the literal wording above:** `j`/`k`
both move *and* open (fetch) the target message immediately, rather than
moving a lightweight "cursor" that `Enter` then commits to opening. That
makes `Enter` mostly redundant (it just re-opens whatever's already open) —
named directly rather than silently dropped, since the section explicitly
lists `Enter` as its own shortcut. A real move/open split would need a
second, visually-distinct "keyboard cursor" state independent of
`selected_uid`/`selected_uids`, which felt like scope creep for what this
phase needed to prove; left as real, cheap follow-on work if a fetch-per-
keystroke ever turns out to be too chatty in practice.

**What did not land, and why:**
- **IDLE tied to the *selected* mailbox**, as this section's literal wording
  asks for. B11's `idle_watch` already gives push-based new-mail detection —
  but scoped to a single hardcoded mailbox (`NEW_MAIL_POLL_MAILBOX`,
  `"INBOX"`), feeding B10's toast/watermark logic, not "refresh whatever
  mailbox the user currently has open." Making IDLE follow mailbox selection
  would mean tearing down and respawning `idle_watch`'s connection on every
  mailbox switch (a second, independent connection lifecycle beyond what
  `idle_watch::spawn`'s single call site in `main.rs` manages today) and
  deciding what a push to a *non-INBOX* mailbox should even do in the UI —
  auto-refresh the header list the user is looking at, most likely, which is
  a real, separate feature (live list updates) beyond this phase's flags/
  tree/select/shortcuts scope. Judged already-substantially-covered for the
  "new-mail push" half of B8's wording (that's what B11 *is*), with the
  "on the selected mailbox" half named here as the real, unclosed gap.
- **`\Sent`/Trash/Archive special-use discovery wired into where mail
  actually gets sent/moved.** The infrastructure this phase built
  (`imap::SpecialUse`, populated from real `LIST` attributes) is exactly
  what B7 named as its own missing piece ("`\Sent` special-use-flag
  discovery" — see §B7) — but `main.rs`'s `SENT_MAILBOX`/`TRASH_MAILBOX`/
  `ARCHIVE_MAILBOX` are still the same hardcoded-name constants B7 left
  behind, not yet reading `self.mailbox_rows`' `special_use` field to pick a
  real target. Wiring them up is now mechanical (the data is there) but
  needs a documented fallback for the moment before `FetchMailboxes`' first
  reply arrives (nothing to look up yet) and wasn't attempted here to avoid
  touching B7's already-shipped Sent-on-send path under this phase's own
  time budget.
- **A real recursive/collapsible tree widget.** The left panel renders
  `flatten_tree`'s output as an always-fully-expanded indented list, not a
  `CollapsingHeader`-per-node tree a user could fold shut. Fine for the
  handful of levels a typical account has; a deep, wide hierarchy would want
  real collapse state.
- **Local/offline unread counts.** `FetchUnreadCounts` is a live `STATUS`
  round trip; nothing reads `messages.flags` from the cache to compute a
  count while offline, even though the data (now populated) would support
  it.
- **`is:unread` in search** still doesn't filter (§B4's gap, unchanged) —
  `messages.flags` existing now makes this mechanical, but wiring
  `search_query.rs`'s parsed `is:unread` term into `db.rs::search`'s SQL is a
  B4-shaped change this phase didn't reach into.
- **Confirmation before Delete.** Clicking Delete (or pressing `Del`) moves
  straight to Trash with no "are you sure" — matches most real mail clients
  (Trash is itself the undo), but worth naming since nothing here prompts.
- **Cancelling an in-flight fetch on mailbox change** — still the same gap
  §B2 already documents (no `stop()`-equivalent on the primary IMAP session);
  unrelated to this phase specifically, not attempted here either.
- **Visual verification.** HANDOFF.md §2's screenshot workflow only exercises
  the no-account `ESMAIL_PREVIEW=demo` page (a login screen doesn't even draw
  the webview) — it was re-run to confirm nothing about B8's `main.rs`
  changes broke that baseline, but the actual mailbox tree/unread badges/
  multi-select rows/keyboard shortcuts, which only exist behind a live
  account, have no headless way to be screenshotted in this environment
  (same limitation B10 names for its tray icon and toasts). Verified instead
  by `cargo check --workspace`, `cargo test --workspace`, and new integration
  tests against `mail-mock-server` (`store_flags_adds_and_removes_in_one_call`,
  `move_message_falls_back_to_copy_store_expunge_and_the_message_relocates`,
  `fetch_unread_counts_reflects_seen_flags`) that exercise the exact command/
  event pairs `main.rs` sends and consumes.

**Fixed in a post-merge review pass** (a multi-angle review of the merged
A5+B8+B9 diff against `main`, before opening the PR): a missing DB migration
(`messages.flags` never got the same `ALTER TABLE ... ADD COLUMN` treatment
`message_id` did in B7, so a pre-B8 local `mails.db` would break on the first
`IndexHeaders`/`IndexMail` — fixed with `add_flags_column_if_missing`,
mirroring `add_message_id_column_if_missing`, plus a regression test); the
`f`/star-toolbar toggle deciding one shared add/remove direction from a
single message's flag state and applying it to the whole multi-selection
(fixed with a new `toggle_star_on_selection`/`is_flagged_uid` that decides
each target's own direction independently); `FlagsUpdated`/`Moved` never
updating `self.search_results` (a message starred/archived from a search
view stayed stale or pointed at a since-moved UID until the search was
re-run — fixed by mirroring every `self.headers` mutation onto
`search_results` when present); `Moved` never crediting the destination
mailbox's unread count (fixed by incrementing `dest`'s count alongside
decrementing the source's); `FlagsUpdateFailed`/`MoveFailed` writing to
`self.status` instead of `push_banner` like every sibling error path added
in the same wave of work (fixed — a failed flag/move action no longer
flashes for one frame and vanishes under the next routine status update);
and a same-numbered-UID-in-a-different-mailbox unread-count skew (the
`was_seen` lookup for a `FlagsUpdated` event used to search `self.headers`
regardless of whether that list actually belonged to the event's own
mailbox — fixed by guarding the whole `self.headers`/`search_results`/
`unread_counts` mutation on `mailbox == self.selected_mailbox`, matching
how the DB write below it was already correctly scoped). Also fixed in
`mail-mock-server`: a real bug in the new `STORE`/replace-mode path
(`Mailbox::store_flags` applied `add` before `remove`, and the replace-mode
caller passes the new flags as both `add` and, via a wildcard, `remove` —
so a plain, non-`+`/`-` `STORE FLAGS` always ended up stripping the very
flags it just added, leaving the message with an empty flag set; fixed by
reordering to remove-then-add, with two new unit tests pinning both the
replace-mode fix and that `+FLAGS`/`-FLAGS` are unaffected by the reorder).

**Still open, named rather than fixed in that same pass** (real but out of
scope for a review-driven fix — each is closer to a small feature than a
one-line correction): bulk flag/move actions still issue one `SELECT` +
one `STORE`/`COPY`+`STORE`+`EXPUNGE` sequence *per selected message*
(`imap.rs`'s `store_flags`/`move_message` are UID-singular) rather than
using IMAP's UID-set syntax to cover a whole multi-selection in one round
trip — archiving 20 messages costs up to 80 serialized round trips instead
of one per verb; the theme toggle (`apply_theme` in `main.rs`, B9) calls
`Config::save()` synchronously on the UI thread inside the click handler,
unlike every other piece of I/O in this app which goes through the async
`imap_tx`/`db_tx` channels — a slow/contended disk stalls the whole frame
for a purely cosmetic write; and B9's saved window position/size is applied
at startup with no validation against which monitors are actually
connected, so undocking a second monitor after closing esmail there can
place the window off every visible display on next launch (no obvious
recovery short of deleting `config.toml` — a real fix needs monitor
enumeration before window creation, which `eframe`/`winit` doesn't
straightforwardly expose at that point in startup).

**Mock server extensions this phase needed** (following the pattern B3/B7/B11
each established): `STORE`/`UID STORE` (mutating a new `StoredMessage::flags`
field via `Mailbox::store_flags`, returning `FETCH (FLAGS (...))`), `COPY`/
`UID COPY` (`Store::copy_message`), `EXPUNGE`/`UID EXPUNGE` (`Mailbox::
expunge`, dropping `\Deleted`-flagged messages), `STATUS` (`Mailbox::
unseen_count`), `FLAGS` added to every `ENVELOPE` fetch response, and RFC
6154 special-use attributes on `LIST` for the well-known mailbox names
`Store::add_user` seeds.

### B9. Polish — **PARTIALLY DONE**
Error banners instead of a status string ([src/main.rs:88](src/main.rs:88)),
per-operation progress (the WIP `download_progress` field generalises here),
dark/light theme, window-geometry persistence, and a first-run wizard that
guesses IMAP/SMTP settings from the email domain via a small built-in provider
table. OAuth2 is explicitly out of scope for v1 — note in the README that Gmail
and Outlook therefore need app passwords.

**What landed:** four of the five sub-items, in the order the plan's own
notes suggested prioritizing them (error banners, theme, window geometry,
wizard) — per-operation progress (generalizing `download_progress`) did not,
see below.

- **Error banners.** A new `Banner { id, message }` (`main.rs`) replaces the
  old pattern of clobbering `EsMailApp::status` with `format!("Error: {e}")`/
  `format!("DB Error: {e}")` on `ImapEvent::Error`/`DbEvent::Error` — which
  lost whatever the status string was showing before (e.g. "Page 3 of 9")
  the instant an unrelated background error arrived, and could only ever
  show the single most recent one. `status` itself is untouched and still
  carries transient, non-error progress text ("Connecting...", "Page 3 of
  9") — only the error half of that field's old job moved. Banners are
  additive (a `Vec<Banner>`, each independently dismissable via an "x"
  button) and rendered in their own panel below the top bar. This also let
  `ImapEvent::AppendFailed` (B7's "couldn't save a copy to Sent") gain a
  visible banner for the first time — it was previously log-only specifically
  *because* the single `status` string had nowhere to put it without
  overwriting "Message sent"; a banner has no such conflict, which is exactly
  the problem banners solve. `smtp::SmtpEvent::Error` still reports through
  `compose_status` inside the compose window rather than a banner — that's
  deliberate, not an oversight: it's contextual to the window the user is
  actively looking at, arguably better placed there than in a top-level
  banner.
- **Dark/light theme.** `config::ThemeMode` (`Dark`/`Light`/`System`, its own
  type rather than reusing `egui::ThemePreference` so `config.rs` keeps zero
  egui dependency) persists in `config.toml`, applied to the `egui::Context`
  once at startup (before the first frame paints, to avoid a dark-then-light
  flash) and again on every click of a new "Theme: <mode>" button in the top
  bar, which cycles Dark → Light → System and saves immediately.
- **Window-geometry persistence.** `config::WindowGeometry { x, y, width,
  height }` is tracked every frame from `egui::ViewportInfo::outer_rect` and
  written to `config.toml` once, when a real close is going through (gated
  the same way B10's tray hide-to-tray redirect is — see `ui()`'s comment —
  so a plain window close on Windows, which the tray intercepts into "hide"
  rather than exit, doesn't spuriously save mid-session and a *real* close
  reliably does). `main()` reads `config.toml` a second time before building
  `NativeOptions` (the window has to exist with the right size *before*
  `EsMailApp::new` — which also loads config, for the account list and
  theme — ever runs) and seeds `ViewportBuilder::with_inner_size`/
  `with_position` from it when present; a first run with no saved geometry
  keeps the existing 1280×720 default.
- **First-run wizard / provider table.** `config::provider_for_email` (a
  small `&[(&str, ProviderSettings)]` table — gmail.com, googlemail.com,
  outlook.com/hotmail.com/live.com, yahoo.com, icloud.com/me.com,
  fastmail.com, gmx.com, zoho.com) looks up complete IMAP+SMTP host/port
  settings by the domain half of an email address. This is a different axis
  from B7's `config::derive_smtp_host`: that one mechanically transforms an
  IMAP host the user already typed (`imap.` → `smtp.`) and stays exactly as
  it was, still the fallback for any domain not in the table; this one goes
  from just an email address to a complete guess, which is what a provider
  like Outlook needs (`outlook.office365.com`/`smtp.office365.com`, port
  587/STARTTLS — nothing about that is reachable by string-transforming
  `outlook.com`). Wired into the login screen as an "Email address" field,
  shown only when `config.accounts` is empty (first run — a returning user
  picking a saved account, or editing an already-filled host, has nothing
  useful for this to guess); typing a recognized domain fills in
  host/port/SMTP host/SMTP port and the username, but only while the host
  field still looks untouched (empty, or still the generic
  `imap.gmail.com`/`993` `EsMailApp::new` seeds a blank form with) — it never
  clobbers a host the user actually edited.
- **README note on OAuth2.** Added: Gmail and Outlook need an app password
  for v1, consistent with the provider table above guessing their connection
  settings correctly while still not being able to authenticate against
  either without one.

**What did not land, and why:**
- **Per-operation progress (generalizing `download_progress`).** The field
  is still exactly what B10/B6 left it: `Option<(u32, u32)>` fed only by
  `ImapEvent::DownloadProgress`, shown as one progress bar tied to bulk
  mailbox download. Generalizing it to cover more than one concurrent
  operation (e.g. an `IndexHeaders` batch alongside a `BulkDownload`) means
  either a `HashMap<OperationKind, (u32, u32)>` or a small `Vec` of named
  progress entries, plus a matching new event shape from `imap.rs`/`db.rs`
  and a render loop instead of the current single `if let Some(...)` block —
  a real, if small, redesign of that state rather than a wire-through.
  Deliberately not attempted in the same session as the other four
  sub-items: this is also one of the two places (along with the Reply/Reply
  All/Forward buttons and the mailbox list) `main.rs` is busiest, and B8 (see
  PLAN.md) is concurrently landing flags/unread-counts/multi-select in a
  separate worktree touching the same file — a broader progress-state
  refactor is exactly the kind of change likely to conflict line-for-line
  with whatever B8 does to the message-list panel, so it's left for its own
  follow-on commit once both land and the merge has settled rather than
  risked here.
- **Rich theme customization.** Only Dark/Light/System — no accent-color
  picker or custom palette; "dark/light theme" in the plan's own wording is
  satisfied by the three-way toggle.
- **Window-geometry edge cases.** `egui::ViewportInfo::outer_rect` is `None`
  on Android/Wayland (documented on the field itself) — `window_geometry`
  simply stays `None` there and nothing is persisted, which degrades to
  today's un-persisted behavior rather than erroring. Multi-monitor DPI
  changes between runs aren't specially handled either; a geometry saved on
  one monitor layout is applied verbatim on the next launch, same as most
  native apps that do this at all.
- **A dedicated "wizard" flow/modal.** The plan says "a first-run wizard";
  what landed is a single autofill field on the existing login screen rather
  than a separate multi-step dialog. Chosen deliberately over a modal:
  `main.rs` is shared, actively-touched ground with B8's concurrent work
  (see above), and a new top-level window/dialog is a much bigger footprint
  for the same practical outcome ("typing your email fills in the right
  settings") than one conditionally-shown text field plus a lookup function.
  If a real multi-step wizard (confirm the guessed settings, test the
  connection, etc.) is wanted later, `provider_for_email` and
  `apply_provider_wizard` are the two pieces such a UI would call into.

**Verification:** `cargo check --workspace` and
`ESMAIL_TEST_CA_TRUSTED=1 cargo test --workspace` both clean (98 `esmail::lib`
unit tests including the new `config::tests` for `ThemeMode`,
`WindowGeometry`'s TOML round-trip, and `provider_for_email`; 7 `main.rs`
tests; 11 passing / 2 ignored integration tests; all unaffected by this
change). Screenshotted both `ESMAIL_PREVIEW=demo` (pixel-identical to the
pre-B9 baseline — nothing here touches the webview/`show()`/sizing path) and
the login screen with `ESMAIL_SCREENSHOT` (no `ESMAIL_PREVIEW`), which
confirmed the new "Theme: System" button renders correctly in the top bar
without disturbing the existing layout. The first-run wizard's empty-state
screenshot (no saved accounts) was attempted but not captured cleanly: this
environment has a **pre-existing, unrelated** interaction between B10's
tray-icon hide-to-tray redirect (a plain window close on Windows only exits
when `exit_requested` was set by the tray's own "Quit," which nothing sets
during an automated `ESMAIL_SCREENSHOT` run) and this dev machine's leftover
`Config::migrate_legacy` source file (`%APPDATA%\esmail_config.txt`, from
pre-B1 testing) repopulating a saved account the instant `config.toml` is
removed to simulate a first run — neither is a B9 regression, and the wizard
field's gating logic (`self.config.accounts.is_empty()`) was verified by
direct code reading instead.

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

### B11. IMAP push (`IDLE`) — **DONE**
A new phase, added once `crates/mail-mock-server` (below) existed to verify
it against — the "IDLE did not land" cuts in §B2 and §B10's "detection:
polling, not IDLE" were both explicit placeholders for exactly this.

**Why a separate connection, not a session-pool rework.** §B2 originally
called for "a small pool: one long-lived control session per account for
IDLE and mailbox state, plus a worker session for fetches" — a real rework
of `ImapActor`'s single-session design. `async_imap::extensions::idle::
Handle`'s own doc comment explains why IDLE needs *some* dedicated
connection at all: "As long as a `Handle` is active, the mailbox cannot be
otherwise accessed" — sharing `ImapActor`'s session would mean interrupting
IDLE (send `DONE`, do the fetch, re-issue `IDLE`) around every header/body
fetch. This phase gets the push behavior without the full pool rework: a
new module, `idle_watch.rs`, opens and keeps alive its *own* IMAP
connection that does nothing but `EXAMINE` one mailbox and sit in `IDLE`,
completely independent of `ImapActor`. The full control+worker pool (so a
*fetch* session, not just IDLE, gets its own connection — letting a body
fetch stop blocking the header list) is still real, separate follow-on
work; this phase only closes the "no push, only polling" gap.

**What a push means.** IMAP's `IDLE` reports "something changed" without
saying what — new mail, an expunge, a flag change all look identical from
outside. `idle_watch` makes no attempt to tell them apart: on any server
push it sends a bare `MailboxChanged` signal, which `main.rs`'s
`spawn_new_mail_watch` (B10) treats exactly like its own poll-timer tick —
send `ImapCommand::PollMailbox`, and let the existing `notify::
update_watermark` logic decide whether a UIDNEXT advance actually happened.
No new "what changed" parsing was needed because B10's watermark-based
detection already answers that question the same way regardless of what
triggered the check.

**Push augments the poll timer; it doesn't replace it.** `spawn_new_mail_
watch`'s `NEW_MAIL_POLL_INTERVAL` (60s) timer keeps running unconditionally
alongside `idle_watch`'s pushes. This is deliberate, not a leftover: it's
what keeps new-mail detection working at all if the server doesn't support
`IDLE` (`idle_watch`'s connection then just fails to `IDLE`, backs off, and
retries forever, silently contributing nothing — see below), or while
`idle_watch`'s connection is mid-reconnect. `IDLE` only ever makes new mail
show up *sooner* than the timer would have; nothing regresses versus B10's
original poll-only behavior on a server (or network path) where `IDLE`
doesn't work.

**Connection lifecycle.** `idle_watch::spawn` is called from the same
"Connect" button click that sends `ImapCommand::Connect`, with the same
host/port/username/password — there was nowhere else in the UI those
credentials are known. It runs its own independent connect → login →
`EXAMINE` → `IDLE`-loop → (on any error) backoff-and-retry cycle forever,
completely decoupled from `ImapActor`'s own connection state; there is no
ordering requirement between the two connecting, since `idle_watch` simply
has nothing to push until its own login succeeds. Each `IDLE` round trip is
capped at 29 minutes (`IDLE_ROUND_TRIP`) and re-issued before that, per RFC
2177's recommendation to avoid a server-side inactivity timeout silently
dropping the connection. Backoff on failure is exponential (1s → 30s cap),
reset back to 1s once a connection has stayed up for at least 60 seconds —
so a connection that was genuinely working and then dropped doesn't inherit
a maxed-out backoff from an unrelated earlier flapping period, but a
connection that fails immediately and repeatedly (bad credentials, a server
with no `IDLE` support that answers `BAD`) doesn't hammer the server either.

**Mock server support, added alongside the client.** `mail-mock-server`
previously had no `IDLE` at all (its own README listed it under "no
`SEARCH`, `IDLE`, `APPEND`..."). `imap_server.rs` now handles `IDLE`:
responds `+ idling`, subscribes to a new `Store::notify` broadcast channel
(`tokio::sync::broadcast<String>`, the changed mailbox's name) that
`Store::deliver` sends on after every append, and pushes an untagged
`* N EXISTS` for any delivery into the client's selected mailbox until it
sends `DONE`. This only ever reports the current message count — no
`EXPUNGE`, no flag-change `FETCH` — since nothing in this mock server
removes a message or changes a flag; `idle_watch`'s "any push means go
re-check" handling doesn't care which kind a real server would send, so the
narrower mock is still a faithful test of that contract. `CAPABILITY` now
advertises `IDLE` too, for realism (nothing here currently checks it before
using the command).

**What landed:** `idle_watch.rs` (the always-on IDLE connection + reconnect
loop), its wiring into `main.rs` (an `idle_wake_tx`/`idle_wake_rx` channel
threaded from `EsMailApp::new` through the "Connect" button to
`spawn_new_mail_watch`'s `select!` loop), and `mail-mock-server`'s `IDLE`
support (`store.rs`'s `notify` broadcast channel, `imap_server.rs`'s `IDLE`
handler) plus an integration test
(`idle_push_notifies_of_new_mail_without_polling`) proving a push arrives
in low single-digit seconds rather than needing the 60s poll timer —
verified against the real mock server, not asserted from unit tests alone,
since this is exactly the kind of live-protocol code this session's whole
established pattern (B2/B3/B4/B6/B7's deferrals) was waiting on something
to verify against.

**What did not land:**
- **The full B2 control+worker session-pool rework.** As explained above,
  this phase deliberately scopes down to "one more dedicated connection for
  IDLE", not "fetches get their own connection too" — a body fetch still
  blocks the header list, and `BulkDownload` still blocks everything else
  for its duration. Real, separate follow-on work.
- **Watching more than INBOX.** Same scope cut as B10's polling had, for
  the same reason — `idle_watch::spawn` is called with a single hardcoded
  mailbox (`NEW_MAIL_POLL_MAILBOX`, "INBOX"), not a per-mailbox or
  user-configurable list. A second `idle_watch::spawn` call per additional
  watched mailbox is the mechanical extension, once there's a UI for
  choosing which mailboxes to watch.
- **Distinguishing push types.** As above — every push is treated as "go
  re-check", which is correct but leaves no way to (for example) react
  differently to an `EXPUNGE` than to new mail, if a future feature wanted
  to.
- **A "connected"/"watching" indicator in the UI.** There's no visible
  sign of whether `idle_watch`'s connection is currently up, reconnecting,
  or has given up (it never gives up — it retries forever — but nothing
  shows the user its current state). A status string or icon would be
  B9-adjacent polish, not part of this phase.

---

## Sequencing

| Phase | Content | Unblocks |
|---|---|---|
| ~~**0**~~ | ~~Manifest fix; clear 4 deprecations; commit `db.rs`~~ **DONE** | everything |
| ~~1~~ | ~~A1, A2~~ **DONE** | all of A |
| ~~2~~ | ~~A3, A4~~ **DONE** | B5 |
| 3 | ~~B1~~ **DONE**, B2 **partially done** (worker-session split now done; cancellation-of-in-flight-work still doesn't interrupt an active fetch, see §B2) | B3, B7 |
| 4 | B3 **partially done** (incremental fetch now acted on; UID-based cache paging still not, see §B3), B4 **partially done** (see §B4) | B8 |
| 5 | ~~B5~~ **DONE** (allowlist deferred, see §B5), B6 **partially done** (lazy fetch deferred, see §B6) | — |
| 6 | B7 **partially done** (APPEND to Sent now lands; special-use discovery/drafts/retry-queue/rich-text still deferred, see §B7) | — |
| 7 | ~~A5~~ **DONE** (Wheel migration, IME, cursor, clipboard all landed, see §A5); ~~A7~~ **DONE**, A6 **partially done** (see §A6); B8 **partially done** (flags/mailbox-tree/unread-counts/multi-select/shortcuts landed, IDLE-on-selected-mailbox and special-use-flag wiring for Sent/Trash/Archive deferred, see §B8); B9 **partially done** (error banners, theme, window geometry, provider-table wizard land; per-operation progress deferred, see §B9) — phase 7 is now essentially complete; remaining work is the small follow-ups named in HANDOFF.md | — |
| *(unordered)* | ~~B10~~ **DONE** (Windows only; INBOX-only polling, see §B10) — independent of B7/A5/B8/B9, landed out of sequence alongside whichever of those another session was mid-way through | — |
| *(unordered)* | ~~B11~~ **DONE** (IMAP `IDLE`/push, augments B10's poll timer rather than replacing it, see §B11) — depended on `mail-mock-server` existing, independent of everything else in this table | — |

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

## Track C — Servo → litehtml migration (Phases 0-3)

Everything in Tracks A/B above describes `egui-servo-webview`, which this
migration replaces outright with `egui-litehtml-webview`
(`crates/egui-litehtml-webview`) — a JS-less HTML/CSS renderer via
[litehtml](https://github.com/litehtml/litehtml) (through
`va1erian/litehtml-rs`'s Rust bindings, a fork carrying a Windows/MSVC build
fix found and merged in a prior session). No legitimate mail client executes
JS in HTML email, so the entire Servo engine — and everything Track A above
spent seven phases getting right (A3's resource interception, A5's input
forwarding, A6's rendering path, the overlay scrollbar) — turned out to be in
service of a JS engine nothing needed. This is a clean cutover, not a
dual-path migration: `egui-servo-webview` is deleted, not feature-flagged.

**Approved plan's phases 0-3, what landed:**

| Phase | Content | Status |
|---|---|---|
| 0 | Workspace setup: root `Cargo.toml` swaps the `servo`/`dpi`/`http`/`raw-window-handle`/`euclid`/`keyboard-types` block for a `litehtml` git dependency (`features = ["pixbuf", "email"]`), adds `ureq` (Misc section), widens `image`'s features to `["png", "jpeg", "gif"]` | **DONE** |
| 1 | New crate `egui-litehtml-webview` (Cargo.toml exactly as specified: `egui`/`litehtml`/`url`/`log` deps, `eframe`/`env_logger` dev-deps, `two_views` example) | **DONE** |
| 2 | Core render pipeline: `PixbufContainer` (persistent) + fresh `litehtml::Document` per render (never stored — see the crate's own module doc for the self-referential-struct reasoning), the measure→resize→draw→resolve-images sequence, `WebViewHost`/`WebView`/`WebViewConfig`/`WebViewSource::Html`/`WebViewEvent::LinkClicked`/`WebViewHandler`/`InterceptOutcome` public API | **DONE** |
| 3 | `crates/esmail/src/main.rs` wiring (`MessageViewHandler` now fetches allowed remote images itself via `ureq`, since litehtml has no network layer to delegate to), `egui-servo-webview` deleted, verification | **DONE** |

**API deviations from the plan's sketch, and why** (per HANDOFF.md §6's
working agreement: fix the plan in the same commit rather than silently
diverging):

- **`WebViewHost::from_eframe(cc, size)` was dropped in favor of
  `WebViewHost::new()` (no arguments, infallible).** The plan's own
  Cargo.toml spec for this crate lists `eframe` only as a dev-dependency
  (examples/tests), so a public method taking `&eframe::CreationContext`
  cannot exist in the library without adding `eframe` as a real dependency —
  and litehtml's `pixbuf` backend genuinely needs nothing from `cc` (no
  window handle, no GL context), so there was nothing worth threading through
  a parameter for. `main.rs`'s call site changed from
  `WebViewHost::from_eframe(cc, PhysicalSize::new(1280, 720)).expect(...)` to
  `WebViewHost::new()`.
- **`InterceptedResponse` wrapper was dropped**; `InterceptOutcome::Serve`
  carries a plain `Vec<u8>`. There is no HTTP status code to carry for an
  image load the way there was for Servo's `WebResourceResponse`, so the
  wrapper added nothing.
- **`WebViewHandler::navigation`/`NavigationPolicy` were dropped entirely**,
  not kept vestigially — litehtml has no navigation concept at all (see the
  crate's `WebViewEvent::LinkClicked` doc), so there was no policy left to
  decide, matching the plan's own "simpler is better" guidance for this case.
- **`master_css: None`, `user_styles: Some(EMAIL_MASTER_CSS)`** — not
  `Some(EMAIL_MASTER_CSS)` as `master_css`. Passing it as `master_css`
  *replaces* litehtml's built-in master stylesheet (confirmed by reading
  `litehtml_c.cpp`'s `lh_document_create_from_string`: `master_css ?
  master_css : litehtml::master_css`) rather than layering on top of it,
  which was tried first and produced a real bug — every element collapsed
  onto one or two inline-flowed lines, since `<h1>`/`<p>`/`<div>`/`<table>`
  lost their default `display: block`/`table-row`/etc. Caught by the
  mandatory `ESMAIL_PREVIEW=demo` screenshot check per HANDOFF.md §2 (see
  its own before/after screenshots in this session's transcript), not by any
  test — exactly the kind of bug HANDOFF.md warns compiles clean and changes
  no warning count.
- **Click detection uses a short-lived, hit-test-only `Document`** (layout,
  no draw) built on demand from `WebView::show`'s `egui::Image` response,
  rather than keeping any `Document` alive across frames. `on_lbutton_down`
  + `on_lbutton_up` + `take_anchor_click()` on that one-shot `Document` is
  enough for `WebViewEvent::LinkClicked` without needing a persistent,
  self-referential `Document`/`PixbufContainer` pair.

**Known limitation — `vh` CSS units are relative to this render's own
content height, not a real browser viewport.** `PixbufContainer` ties
"viewport size" and "canvas size" to the same `resize_with_scale` call, and
this crate renders a message as one static image at its full content height
(so `WebView::show`'s `egui::ScrollArea` can scroll it natively) rather than
into a fixed-size scrolling viewport. A message seeded at height 1 (this
crate's very first render) resolved `1vh` to ~0.01px, collapsing any `height:
NNvh` block to nothing — caught by the same demo-page screenshot check (its
`.tall { height: 60vh; }` block). Fixed by seeding a plausible
`DEFAULT_VIEWPORT_HEIGHT` (800 logical px) rather than 1px, so `vh` resolves
to something reasonable instead of collapsing to zero — this does not make
`vh` mean what it would in a real browser, it just avoids the degenerate
case. In practice this is a non-issue for real mail: no mainstream mail
client preserves or predictably renders viewport-relative units in HTML
email, so authors do not rely on them. See
`egui-litehtml-webview/src/lib.rs`'s `DEFAULT_VIEWPORT_HEIGHT` doc comment
for the full reasoning.

**What did not land / deliberately out of scope for this pass:**

- **Text selection (Phase 4)** — `litehtml::selection::Selection` and
  `PixbufContainer::draw_selection_rects` exist in the vendored crate and are
  untouched; nothing in this pass wires them up, per the plan's explicit
  scoping. The one-shot-render design (fresh `Document` per interaction, not
  a persisted one) will need revisiting for this, since a drag-select
  gesture needs the same `Document` alive across a sequence of frames, not
  just for one instantaneous click.
- **Hover cursor / `:hover` styling** — `PixbufContainer::cursor()` and
  `Document::on_mouse_over` exist but are not called; only click (not
  hover/move) triggers a `Document` build. Not required by any current
  `esmail` call site.
- **CI/Dockerfiles (Phase 5)**, **`rusqlite` version (Phase 5)**, and
  **rewriting this file's/HANDOFF.md's Servo-era history (Phase 6)** —
  explicitly out of scope per the approved plan; not touched.
- **Per-image memory growth across message loads** — `PixbufContainer`'s own
  decoded-image cache (`images: HashMap<String, Pixmap>`) is never purged by
  `WebView::load`/`reload` (only `pending_images`/`requested_images`
  tracking is, via `clear_pending_images`), so distinct image URLs across
  many opened messages accumulate for the life of the `WebView`. Not
  addressed here; `PixbufContainer` exposes no eviction API to call.

**Verification actually run:** `cargo check --workspace` (clean, no
warnings, `--examples --tests` too), `cargo test --workspace`
(`ESMAIL_TEST_CA_TRUSTED=1`) — 115 `esmail` lib tests + 10 `main.rs` tests +
15 `mail-mock-server` integration tests (2 `#[ignore]`d stress tests) + 2
`mail-mock-server` lib tests + 2 smoke tests, all passing, none of them
exercising litehtml itself (none existed before this pass either — the
widget crate has no unit tests of its own yet, same as the gap this
migration inherited rather than introduced). `ESMAIL_PREVIEW=demo
ESMAIL_SCREENSHOT=... ESMAIL_SCREENSHOT_FRAMES=90` against the debug binary,
twice — the first screenshot caught the `master_css` bug above, the second
(after the fix) shows correct layout: heading, accented UTF-8 text, the
link, the `From`/`Subject` table, and the `.tall` gradient block extending
well past the visible window (confirming the `ScrollArea` has real content
to scroll, replacing the old overlay-scrollbar polling entirely). `cargo
build --release --bin esmail` succeeded; the resulting `esmail.exe` is
**12,817,920 bytes (≈12.2 MiB)** — down from Servo's 100-300MB+DLLs, though
larger than the 3.96MB fully-static build measured in the prior
hands-on-validation session (that number came from a minimal standalone
binary linking only `litehtml`/`tiny-skia`/`cosmic-text`, not the full
`esmail` binary with `rusqlite` bundled, `eframe`/`egui_glow`, async-imap/
tokio, keyring backends, etc. all still linked in). Not verified: the
`libEGL.dll`/`libGLESv2.dll` copy step in HANDOFF.md §3.9 is confirmed
*unnecessary* now (the screenshot run above succeeded without them present
next to the binary) — litehtml's `pixbuf` backend is pure CPU, no
GL/EGL dependency at all.

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

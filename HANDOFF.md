# Handoff

Read this before touching anything. [PLAN.md](PLAN.md) is the full design; this
is what you need to actually work, plus the mistakes already made so you do not
repeat them.

**Your next task is B2** ([PLAN.md](PLAN.md) §B2). Everything in phases 0-2 is
done and committed, and B1 (phase 3) is done too.

---

## 1. The environment

Work in the worktree, never `cd` to the parent repo:

```
C:\Users\hadri\Documents\repos\ebrowser\src\.claude\worktrees\imap-mail-client-egui-736b94
```

Branch `claude/imap-mail-client-egui-736b94`. The layout:

```
Cargo.toml                            workspace root
crates/egui-servo-webview/src/lib.rs  the widget  (833 lines, incl. tests)
crates/esmail/src/main.rs             the app
crates/esmail/src/imap.rs             IMAP actor
crates/esmail/src/db.rs               SQLite actor
crates/esmail/src/screenshot.rs       screenshot dumps
```

Commands, with real timings on this machine:

```bash
cargo check --workspace          # ~2s warm, ~2min cold
cargo test -p egui-servo-webview # ~15s warm; 13 tests, all must pass
cargo build --bin esmail         # ~25s warm, ~3min cold
```

A cold build compiles Servo and takes minutes. Run long builds in the
background rather than blocking on them.

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

### 3.1 Servo's hooks fail OPEN

`NavigationRequest::drop` sends **allow**. An unhandled `WebResourceLoad` sends
**DoNotIntercept**. Dropping either permits the thing you meant to block, and
nothing warns you.

This already shipped as a bug: `drop(request)` was allowing every link
navigation *and* emitting `LinkClicked`, so links opened twice. Fixed in
`06db971` by calling `request.deny()` explicitly.

A3 is precisely about doing this properly. Every path through your handler must
end in an explicit `allow()` / `deny()` / `intercept()` / `cancel()`.

### 3.2 Do NOT resize the offscreen rendering context

`WebView::resize` already calls `resize_rendering_context` internally
(`servo-0.1.0/webview.rs:393`). Calling `offscreen_ctx.resize()` yourself first
makes Servo's own resize a no-op, because `OffscreenRenderingContext::resize`
early-outs on an unchanged size — and the page stays laid out at the old width.

This was tried, looked plausible, compiled, changed no warning counts, and was
caught only by the screenshot. There is a comment at the call site in
`show()`. Leave it alone.

### 3.3 `stop()` does not exist

`servo::WebView` in 0.1.0 has `load`, `reload`, `can_go_back`, `go_back(amount)`,
`can_go_forward`, `go_forward(amount)` — but **no** `stop()`. Do not plan around
cancelling an in-flight load. The only control points are up front.

### 3.4 Field declaration order is drop order

In `EsMailApp`, `web_view` is declared **before** `web_view_host` on purpose: the
view must be torn down before the engine backing it. Do not reorder.

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

### 3.7 `rusqlite` must stay at 0.37

`servo-storage` depends on `rusqlite ^0.37`, which pins `libsqlite3-sys ^0.35`.
`libsqlite3-sys` sets `links = "sqlite3"`, so Cargo permits exactly one copy in
the graph. Bumping rusqlite makes the workspace unresolvable. Documented at the
declaration in the root `Cargo.toml`.

### 3.8 Servo's log noise is not yours

Six `webrender::device::gl` "Cropping texture upload" warnings during the first
two paints are its GPU cache warming up. Seven `profile_traits::mem`
"Disconnected" warnings at exit are Servo tearing itself down — they appear even
when no view is ever drawn. Both were investigated and are not embedder bugs.
`init_logging()` in `main.rs` filters them; `RUST_LOG` overrides it. Do not go
hunting for them again.

### 3.9 The exe needs `libEGL.dll` / `libGLESv2.dll` next to it, not just built

`ESMAIL_PREVIEW` screenshotting panics with `egl function was not loaded` at
`surfman`'s `egl_bindings.rs` unless `libEGL.dll` and `libGLESv2.dll` are next
to `esmail.exe` (or on `PATH`). Cargo does not copy them there. They exist
untracked at the *repo* root (`C:\Users\hadri\Documents\repos\ebrowser`, one
level above `src`) — copy them into `target/debug` (or `target/release`)
before running the binary in a fresh worktree:

```bash
cp "$(git rev-parse --show-toplevel)/../libEGL.dll" \
   "$(git rev-parse --show-toplevel)/../libGLESv2.dll" ./target/debug/
```

This is the same untracked-DLL situation noted in [PLAN.md](PLAN.md)'s Risks
section; that section still owns the packaging decision (gitignore vs. commit
vs. fetch-at-build-time). This entry just saves you from re-diagnosing the
panic.

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
| *(this branch)* | **B1** — `config.rs`/`secrets.rs`: TOML `Config` in the platform config dir, passwords in the OS keyring, legacy `esmail_config.txt` migration, inline saved-accounts list on the login screen |

State: `cargo check --workspace` clean, `cargo test --workspace` 25 passing, app
builds, runs, screenshots and exits cleanly.

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

The user's `mail` checkout still has the same WIP uncommitted that is now
committed here as `0bbfd2d`, plus the manifest fix applied directly to it. Once
this branch is accepted, that working tree should be reset rather than
hand-merged — otherwise the two diverge in `lib.rs`, `imap.rs` and `main.rs`.
Confirm with the user before touching their checkout.

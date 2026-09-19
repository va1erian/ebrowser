# Local patches to `litehtml`

This directory is a copy of the `litehtml` crate (only that crate, not
`litehtml-sys`) from <https://github.com/va1erian/litehtml-rs>, branch
`bump-litehtml-table-measure-cache` (commit `9f8cad6`: `master` at `61125c8` plus a
bump of the C++ litehtml to the table-cell-measurement memoization, which is
what makes deeply nested table layouts finish at all). It is wired in through `[patch]` in the workspace `Cargo.toml`.

Changes against that commit:

* `Cargo.toml`: `litehtml-sys` is a git dependency instead of `path = "../litehtml-sys"`
  (the sys crate is not vendored here).
* `src/pixbuf.rs`, `PixbufContainer::draw_image`: rewritten to honour the
  layer geometry litehtml hands it. Before, every image was blitted at its
  *natural* pixel size, anchored at the border box, and clipped to the clip
  box -- so an `<img width=200>` of a 600px picture came out as a 200px crop of
  its top-left corner, small images were never upscaled, and `max-width`,
  `background-size` and `background-position` were ignored. Now the image is
  scaled to `layer.origin_box()` (the final size + position litehtml computed),
  tiled according to `layer.repeat()`, and clipped to `layer.clip_box()`.
  A dedicated clip mask is only built when a tile actually spills outside the
  clip box.

`upstream-draw-image.patch` is the `pixbuf.rs` change as a unified diff, ready
to apply to the upstream repo. Once it is merged there, delete this directory
and the `[patch]` section in the workspace `Cargo.toml`.

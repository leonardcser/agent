# smelt-term

Pure terminal renderer: double-buffered diff/flush, `LayoutTree`,
`Grid`, `paint_chrome`, and half-block-friendly cell primitives.

Key entry points:

- `Compositor::render_with`: drive a frame.
- `paint_layout_tree`: walk a `LayoutTree` and dispatch leaves.
- `flush_diff`: emit SGR escapes for a `Grid` diff.
- `Grid` / `GridSlice`: `set` / `put_str` (full overwrite; string writes
  return the clipped end column), `put_char` / `put_str_fg` / `put_line`
  (preserve bg where applicable).
- `TerminalSession`: raw-mode/alternate-screen lifecycle guard with suspend
  support for shell-outs.

Editor concepts like buffers, Vim, and overlays live in sibling crates.
`smelt-term` owns rendering, layout geometry, and runtime-neutral split interaction.
It supports Rust 1.71 and has no Lua, async-runtime, or agent dependencies.

## Resizable layouts

Keep a `Split` handle and compose its two children with `LayoutTree::split`.
Splits nest in either axis and work inside boxes or `LayoutTree::frame`, which
adds independent border/title/padding while preserving the child's natural size.

```rust
use smelt_term::{Axis, LayoutTree, PaintId, Split, SplitOptions, SplitResizeMode, SplitSize, Surface};

let sidebar = Split::new(Axis::Horizontal, SplitOptions {
    size: SplitSize::Cells(30),
    minimum: [20, 20],
    resize_mode: SplitResizeMode::Cells,
    ..SplitOptions::default()
});
let mut surface = Surface::new(120, 30);
surface.set_layout(LayoutTree::split(
    sidebar.clone(),
    LayoutTree::leaf(PaintId(1)),
    LayoutTree::leaf(PaintId(2)),
));
```

- **Preference vs geometry:** `preferred_size` / `set_preferred_size` read and
  restore plain `SplitSize` values. `reset` restores the constructor's preference.
  Terminal clamping never overwrites it, including no-op gestures on tiny screens.
- **Resize policy:** `Cells` keeps a sidebar's chosen cell count; `Proportional`
  (the default) keeps the chosen fraction after user resizing.
  `ResolvedSplit::equalize` balances the panes using the same policy. Minima include child chrome; when they cannot
  fit, the resolver relaxes them proportionally. The divider consumes one cell,
  except when fewer than three cells are available.
- **One geometry snapshot:** `Surface::resolve_layout` or `LayoutTree::resolve`
  returns leaf rectangles, dividers, split handles, and painter-order operations.
  `split_for_leaf` finds the nearest matching ancestor without enum matching.
  `ResolvedSplit::resize` grows either pane using the pointer resize bounds.
  Use `Surface::render_resolved` to paint that exact snapshot; resolve again after
  sizing, content measurements, or terminal dimensions change.
- **Input:** `SplitInteraction::begin` accepts a hit from `divider_at`; `update`
  uses current geometry and works outside the divider. `release` atomically
  applies the release position and ends capture. `cancel` retains the last size;
  use it on keyboard input or focus loss. Passing missing or replacement geometry
  cancels safely. Responses have explicit `Started`, `Dragging`, `Finished`, and
  `Cancelled` phases. `changed` reports any change during the gesture. Redraw on a
  response; persist when `response.ended() && response.changed`.
- **Appearance:** `Surface::set_layout_style` accepts normal/active `DividerStyles`
  and the interaction's `active_id`. `SplitOptions::styles` overrides those defaults
  for one split. Styles, including backgrounds, are preserved; the library does
  not prescribe theme group names. Default dividers use the plain terminal style.
- **Identity:** clones share immutable identity/configuration and mutable sizing.
  Inspect `id()`, `axis()`, and `options()`; use `SplitOptions` to construct a
  distinct configuration. Keep handles across layout rebuilds, but use a distinct
  handle for each occurrence in a layout. `LayoutTree::splits` enumerates handles
  without computing geometry, so hosts can validate identity across surfaces.
  Persist cell counts or validated `SplitSize::ratio(numerator, denominator)`
  values, never runtime IDs.
- **Scaling:** resolved snapshots share one leaf index and use parent links and
  pane-order ranges for routing. Storage and traversal are linear in layout size,
  including deeply nested split chains.

Run the standalone mouse/keyboard example:

```bash
cargo run -p smelt-term --example split
```

It supports divider dragging, `H`/`L`, equalize (`=`), reset (`r`), and terminal
resizing. The host owns event dispatch and persistence; the library owns bounds
and gesture state. Public API integration tests live in `tests/split.rs`.

## Text and ANSI

Use `smelt_term::ansi::parse_ansi_lines` with `GridSlice::put_line` for captured
terminal output. It preserves SGR state across rows and graphemes across style
boundaries. `put_str`, `put_padded`, `display_width`, and `truncate_width` use the
same grapheme-aware cell-width policy. No separate ANSI/style dependency or
character-by-character clipping loop is needed.

Part of the [smelt](https://github.com/leonardcser/smelt) project but
usable standalone.

## License

MIT

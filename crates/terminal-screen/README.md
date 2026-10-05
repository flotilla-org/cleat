# terminal-screen

A pure cell-snapshot analysis crate. It does not parse VT bytes, interpret an
application's composer, run a recognizer, or track identities across frames.

```rust
use terminal_screen::{analyze, Cell, Cursor, RowMetadata, ScreenGrid};
let grid = ScreenGrid::new(
    1, 1, vec![Cell { grapheme: "x".into(), ..Cell::default() }],
    vec![RowMetadata::default()], Cursor::default(), 1,
).unwrap();
let tree = analyze(&grid);
assert_eq!(tree.select("row:last > span:not([faint])").unwrap()[0].text, "x");
```

`ScreenGrid` owns resolved RGB colours, grapheme strings, styles, cell width,
semantic content, row wrap/prompt metadata, cursor state and a generation.
Construction checks dimensions and visible cursor bounds. A producer supplies
already-segmented graphemes and wide-cell continuations; analysis never guesses
Unicode width. Rectangles are zero-based, half-open **cell** bounds, independent
of UTF-8 byte or scalar count. Empty narrow cells contribute a space; wide tails
and wrap heads contribute no text. Invisible cells retain observed display
content, which callers can filter with `[invisible]`.

## Tree and regions

The tree has `screen`, `box`, `band`, `row`, `span` and a `cursor` pseudo-element.
Each physical row appears once, including empty and soft-wrapped rows. Its
`index` and `index-from-bottom` are zero-based; `soft-wrap`, `wrap-continuation`,
`prompt` and `semantic-prompt` preserve producer metadata. Spans split on style
or semantic changes; width is geometry and does not split an otherwise equal
style run. True boolean attributes have the string value `true`; false flags
are absent. Spans expose `fg`, `bg` (`#rrggbb`), style flags, underline metadata,
optional hyperlink, and `semantic` (`input`, `prompt`, `output`) plus the
corresponding boolean attribute. Every node exposes implicit `[text=…]`.

Boxes require complete single/double/rounded/heavy Unicode or ASCII borders.
An optional title may interrupt the top edge. Unicode scores 1.0; ASCII scores
0.85. Full-width bands observe blank rows (0.5), repeated separators (0.95), or
uniform styles contrasting with a neighbouring row (0.9). These scores describe
structural evidence, never a semantic role. Detected regions have a typed
`confidence` field and a string attribute of the same name.

Nested boxes attach to the smallest containing box. Rows attach to the smallest
box containing their **entire** bounds, with a band between box and row when
present. Partial-width boxes expose their bounded text and coordinates without
duplicating or clipping physical rows. This deliberately leaves column-level
panel membership to a future tree refinement. Child order is spatial preorder.
A visible, in-bounds cursor is a child of its physical row; a hidden cursor is
absent. Its style, blinking, password-input and wide-tail flags are retained.

## Selectors

The parser is handwritten; element names and attribute names have no registry.

- Elements or roles: `row`, `span`, `composer`, `*`.
- Attributes: `[faint]`, `[semantic=input]`, `[text='draft probe']`, and `^=`,
  `*=`, `$=`. Exact matching is case-sensitive; substring operators use Unicode
  lowercase, following xa11y. Quoted values support escaped quotes/backslashes;
  unquoted values accept name tokens (ASCII letters, digits, `_`, `-`).
- Combinators: `screen row` (descendant), `row > span` (child).
- `:nth(n)` (one-based), `:last`: positional filters over the segment's matching
  nodes in document order, **not** CSS sibling indices. Non-positional predicates
  run first, then positional filters in their written order. Out-of-range nth
  returns an empty result. `row:last` is the bottom physical row.
  `row:nth(1):has-text('x')` first keeps rows containing `x`, then selects the
  first; it equals `row:has-text('x'):nth(1)`. `row:nth(2):last` selects the
  second match, while `row:last:nth(2)` selects nothing.
- `:not(selector)` rejects nodes matching that selector in the whole tree,
  including when it appears inside a scoped `:has`. For example,
  `screen:has(row:not(screen row))` cannot match: every row matches the global
  `screen row` selector.
- `:has(selector)` searches strict descendants of the candidate; `:has(> span)`
  restricts the first segment to direct children. The candidate is not in scope.
- `:has-text('substring')` is case-sensitive over the node's observed text.
- `:matches(/regex/)` compiles a Rust regex at parse time. Escape a slash as
  `\/`; inline flags such as `(?i)` work, including Unicode-aware case matching.
  Unicode word/digit/whitespace classes are enabled; Unicode general-category
  and script tables (`\p{…}`) are not enabled. Invalid regexes are parse errors.
- Comma-separated selector groups return a deduplicated union in document order.

`Selector::parse` yields a reusable selector; `evaluate` returns `NodeId`s.
`ScreenTree::select` is the convenience API returning nodes with bounds/text.
Errors include a byte offset. Parsing limits input to 16384 bytes, predicate
nesting to 32 selector levels (including the top-level selector), and compiled regex size to 1 MiB.

Recognizers can annotate a node's `roles` and `attributes` via `annotations_mut`, or
attach a new `Node` with `add_node`. New node bounds must fit the parent, and
attached geometry and parent/child links cannot be changed through annotations. Node IDs are valid only within that tree.

## Evaluation cost

Each `evaluate` builds one preorder index. Subtree intervals restrict `:has` to
strict descendants, and interval/child masks avoid checking every previous
match's ancestor chain for combinators. Nested selector results are cached by
selector and scope for that evaluation only; frame-wide `:not` results are
computed once. Substring comparison values are lowercased at parse time.

On a synthetic 200×50 screen with a style split at every cell, ten-query dev
build measurements in the same vessel gave:

| Query | Before subtree indexing | After |
| --- | ---: | ---: |
| `row:has(cursor) span:not([faint])` | 45.1 ms/query | 6.9 ms/query |
| `row:has(span:not([faint])) > span:last` | 414.1 ms/query | 15.1 ms/query |

These are diagnostic timings, not CI thresholds or a per-frame latency promise.
Broad nested `:has` queries can still scan overlapping subtrees; evaluation cost
also depends on tree size and selector structure, not just parser input limits.
The memo stores both ordered matches and a membership set for every visited
selector/scope pair; peak memory grows with the number of scopes plus the total
cached matches. Broad nested queries can therefore use substantial memory on
large trees. The memo is dropped when evaluation returns.
Reproduce with `cargo test -p terminal-screen --test structure
selector_timing_span_heavy -- --ignored --nocapture`.

## Validation

```sh
cargo test -p terminal-screen --locked
# With cleat's pinned Ghostty install prepared:
cargo test -p cleat --locked --features ghostty-vt --test terminal_screen_cast
```

The crate has no cleat or Ghostty dependency. The `regex` dependency
provides bounded, non-backtracking regex matching for `:matches`; default regex
features are disabled. The real-VT cast replay lives in cleat's integration tests,
gated by its
`ghostty-vt` feature. Cleat depends on terminal-screen only for tests until #24
wires it into the runtime.

The committed Codex 0.160 fixture has a **non-faint `›` marker** at the start of
its cursor row. The literal `row:has(cursor) span:not([faint])` query therefore
returns `›` when empty and `› draft probe` when drafted (after trimming spaces).
The faint placeholder is excluded correctly. Obtaining just the editable value
requires #313's recognizer/content-extraction rule; structural analysis preserves
all matching cells rather than silently removing application-specific prefixes.

The governor-authorized acceptance query is
`row:has(cursor) span:not([faint]):not([bold])`, yielding empty then `draft probe`.
The test separately asserts the marker-inclusive results above. This is observed
style evidence for this fixture, not a general composer rule.

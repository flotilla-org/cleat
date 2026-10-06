# terminal-screen

Pure, owned terminal snapshots, switchable region producers and selectors. There
is no VT parser, daemon state, application recognizer or identity across frames.
**Selector syntax is experimental until #24 lands.**

```rust
use terminal_screen::{analyze, Cell, Cursor, RowMetadata, ScreenGrid};
let grid = ScreenGrid::new(1, 1,
    vec![Cell { grapheme: "x".into(), ..Cell::default() }],
    vec![RowMetadata::default()], Cursor::default(), 1).unwrap();
let tree = analyze(&grid);
assert_eq!(tree.select("row:last-match > span:not([faint])").unwrap()[0].text, "x");
```

`ScreenGrid` owns resolved RGB colours, grapheme strings, style flags, cell
widths, input/prompt/output tags, row wrap/prompt metadata, cursor and generation.
Construction checks dimensions and cursor bounds. Producers supply graphemes
and continuations; analysis does not guess Unicode width. Coordinates are
zero-based, half-open **cell** rectangles. Empty narrow cells add a space; wide
tails and wrap heads add no text. Invisible cells retain their observed content.
Physical rows remain available through `grid.row` and `grid.text`.

## Region producers and tree

`RegionProducer::produce(grid, optional_history)` yields a `RegionTree`.
`GraphSegmenter` is the default used by `analyze`; alternate producers construct
validated partitions with `RegionTree::new`, then call `to_screen_tree`. Every
region has provenance (`declared`, `structural`, `graph`, `temporal`, `learned`),
confidence and boundary evidence. Temporal production itself is not implemented.

The tree is `screen > region… > row slice > span`; row slices use the element
name `row`. Each grid cell belongs to exactly one row-slice leaf in the region
partition, and exactly one span in the selector tree. Region bounds are bounding
boxes; a nonrectangular region's text reads its descendant slices, not all cells
in its bounding box. Separated slices on one physical row are joined with a
space, and different rows with a newline. Cell counts include descendant leaves.
Regions may nest around frames as well as inside them. Child order is spatial
preorder; IDs are valid only within one frame.

Graph segmentation classifies walls, gutters and content, assigns explainable
costs to neighbouring cells and merges at fixed block/group/zone/enclosure
thresholds. Equal cell sets collapse into one region retaining all kinds as
roles (`frame`, `enclosure`, `zone`, `group`, `block`, `gap`). No corner-matched
box detector or per-row band detector remains. `GraphParams` defaults are
heuristic; `GraphSegmenter::new` checks custom weights and threshold ordering.
Confidence measures boundary/internal cost margin, not semantic certainty.
Known limits include unstructured screen-reader layouts, paragraph chaining,
partial dividers and touching frames. Weights are not calibrated to ground truth.
See the [spike evidence](../../docs/spikes/graph-segmentation.md).

Row slices retain `index`, `index-from-bottom`, `slice-col`, `soft-wrap`,
`wrap-continuation`, `prompt` and `semantic-prompt`. Spans split on style/semantic
changes within a slice, with `fg`, `bg`, underline metadata, optional hyperlink,
style flags and semantic attributes. True flags have value `true`; false flags
are absent. Every node exposes implicit `text`. Visible cursor pseudo-elements
are row children and carry style/blink/password/wide-tail metadata. They are
observational overlays and do not own cells; hidden hardware cursors are absent.

## Selectors

The parser is handwritten. Element, role and attribute names are data-driven.

- Names: `region`, `row`, `span`, `block`, recognizer roles such as `composer`, `*`.
- Attributes: `[faint]`, `[semantic=input]`, `[state!=working]`, `^=`, `*=`, `$=`.
  All comparisons are case-sensitive by default. `[text*='DRAFT' i]` explicitly
  uses Unicode lowercase matching; equality and inequality follow the same rule.
  Missing attributes fail comparisons, including inequality. Quoted values
  escape quotes/backslashes; unquoted values use ASCII name tokens (`_`, `-`
  included), so decimal punctuation needs quotes.
- Relations: descendant space, child `>`, adjacent sibling `+`, later sibling `~`.
  Sibling order is tree child order and never crosses a parent boundary.
- `:nth-match(n)` / `:last-match`: one-based positions over the segment's entire
  match set, in document order. Other predicates run first, then match positions
  run in written order. Out-of-range positions return no nodes. Old `:nth` and
  `:last` are rejected.
- `:first-child`, `:last-child`, `:nth-child(n)` and `:nth-child(n of S)` count
  siblings under each parent. `S` selects globally, then is intersected with
  siblings. Only positive integer indices are supported, not CSS `an+b`.
  Positions inside `S` are rejected to avoid recursive positional ambiguity.
  All children count by default, including cursor pseudo-elements; roots do not
  have child positions.
- `:not(S)` rejects globally matching subjects, including inside `:has`. Positions
  anywhere inside `:not` are rejected. Comparison attributes on the subject must
  be present for negation to succeed: both `[state!=working]` and
  `:not([state=working])` fail when state is absent. Boolean absence is different:
  `:not([faint])` still matches a span without the faint flag. For alternatives,
  any unknown subject comparison prevents negation from succeeding.
- `:has(S)` searches strict descendants; `:has(> span)` starts at children,
  `:has(+ region)` / `:has(~ region)` start at following siblings. Later segments
  follow their normal relations. The candidate itself is never the first match.
- `:has-text('draft')` searches case-sensitively; `:has-text('DRAFT' i)` opts in.
- `:matches(/regex/)` compiles a bounded Rust regex once; `/regex/i` explicitly
  opts into case-insensitive matching. Inline regex flags work too. Escape `/`
  as `\/`. Unicode case and Perl classes are enabled; general-category/script
  tables are not. Invalid or oversized expressions are parse errors.
- Comma groups produce a deduplicated union in preorder.

`Selector::evaluate` returns node IDs. `evaluate_with_captures` returns node IDs
plus named regex groups from **positive `:matches` on the selected node**.
Absent optional groups are omitted; nested `:has`/`:not` captures are predicates
only. Multiple matching groups/predicates combine captures in written order,
with later duplicate names replacing earlier values. For example:

```rust
# use terminal_screen::Selector;
let selector = Selector::parse(r"span:matches(/(?P<value>draft\s+probe)/)").unwrap();
// selector.evaluate_with_captures(&tree)[0].captures["value"]
```

`ScreenTree::select` returns nodes with text/bounds. Errors carry byte offsets.
Limits: 16384 source bytes, 32 selector levels and 1 MiB compiled regex size.
Recognizers annotate roles/attributes/confidence through `annotations_mut` or add
bounded nodes through `add_node`; attached links and geometry remain immutable.
Added recognizer nodes do not redefine the producer's cell partition. Full
recognizer extraction belongs to #313.

## Cost and validation

Segmentation sorts neighbour edges once and sweeps union-find at fixed levels.
The explicit release acceptance test measures the adversarial 200×50 `+` grid
against a 5 ms/frame budget, averaged after warmup. Run it on an idle machine;
ordinary debug tests do not enforce timing. This is a segmentation budget, not
a bound on full tree construction or selector evaluation for arbitrary inputs.

Selectors build one preorder index per evaluation and cache nested results.
Subtree ranges bound descendant `:has`; sibling-relative queries scan the parent
range. Broad nested queries may scan overlapping ranges and use substantial
memory. Each visited selector/scope memo stores ordered matches, membership and
captures; peak memory grows with visited scopes plus total cached matches and
capture data. The cache is dropped after evaluation.

```sh
cargo test -p terminal-screen --locked
cargo test --release -p terminal-screen --locked adversarial_plus_budget -- --ignored --nocapture
# With the pinned Ghostty prefix:
cargo test -p cleat --locked --test graph_segmentation_casts --test terminal_screen_cast
```

Pure tests generate partition/layout, style, position, sibling and absent-value
cases. Real-VT tests replay both #312 normal-mode agent casts: composer and
footer must be separate regions. Codex also retains empty-vs-draft via faint;
the bold marker is preserved separately. Claude paints its cursor and hides the
hardware one, so no empty-vs-draft assertion is required for Claude. No app prefix
is stripped. Cleat's dev-only dependency runs these gated by `ghostty-vt`, without
introducing a runtime or Ghostty dependency into this crate.

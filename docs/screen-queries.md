# Screen queries (version 1)

`cleat inspect ID --screen [--json]` returns the graph region tree as JSON.
`--select 'SEL'` returns the matching nodes in document order, with their
coordinates, text and named regex captures. `--producer graph` is optional;
other producers are rejected until implemented. `--text-only` prints physical
screen rows as plain text, one line per row; it conflicts with `--json` and
`--select`. The tree does not include a separate physical-row view.

The [JSON schema](screen-query-v1.schema.json) applies identically to CLI and
HTTP tree/selection responses. `schema_version: 1`, `selector_syntax: "stable"`
and `producer: "graph"` identify this contract. `generation` identifies the
observed render generation. Nodes form `screen > region… > row slice > span`,
with visible cursor overlays beneath row slices. IDs are local to this response,
not persistent across generations. A selection retains the original IDs and
links; its parents/children may be outside the returned match set.

Each node has `id`, nullable `parent`, ordered `children`, `element`, `roles`,
`attributes`, `bounds`, `text`, nullable `confidence`, and `captures`. Bounds
are zero-based, half-open cell coordinates `{col,row,width,height}`. Regions
carry `attributes.provenance` (`declared`, `structural`, `graph`, `temporal`,
`learned`), numeric confidence and `attributes.evidence` explaining boundaries.
Other attributes remain extensible for recognizers and producers. Confidence
measures structural boundary evidence, not semantic certainty.

`captures` is one combined string map per matched node. For example,
`span:matches(/(?P<label>draft) (?P<value>probe)/)` returns
`"captures":{"label":"draft","value":"probe"}`. Positive predicates on the
selected node contribute captures; `:has` and `:not` are predicates only.
Missing optional groups are omitted; later predicates override duplicate names.
Tree queries and matches without named groups have an empty map.

Selector syntax is stable for this version. See the
[selector reference](../crates/terminal-screen/README.md#selectors). Positions
`:nth-match(n)` and `:nth-child(n)` accept positive integers only, never CSS
arithmetic. Roles such as `composer` and `status` will be selectable when
recognizers supply them; current queries can select graph regions, for example
`region:has(cursor):last-match` or `region:has-text('context left'):last-match`.

## HTTP

- `GET /sessions/{id}/screen` preserves `{ "text": "…" }` with no parameters.
  It is exactly the text printed by `--text-only` (plus the CLI's final newline).
- `GET /sessions/{id}/screen?tree=1` returns the tree JSON.
- `GET /sessions/{id}/screen?select=SEL` returns matches. Percent-encode SEL.
- `producer=graph` is optional for either query.
- Malformed selectors return HTTP 400 with
  `{"error":{"code":"invalid_selector","message":"… at byte N"}}`.
  Invalid query encoding or producer uses `invalid_query` or `invalid_producer`.
- Engines without VT screen support return HTTP 409 with
  `{"error":{"code":"unsupported","message":"…"}}`, including plain text
  requests. They never return an empty tree as a substitute for support.

`POST /sessions/{id}/wait` accepts an optional `select` string alongside the
existing `conditions` array and `timeout_ms`. It also accepts a condition
`{"kind":"select","select":"SEL"}` inside the array. All conditions OR:

```json
{"select":"region:has-text('ready')","conditions":[{"kind":"output_idle","quiet_ms":500}],"timeout_ms":10000}
```

The result is `{ "status":"ready|timeout|session_gone", "elapsed_ms":N }`.
A ready result includes the condition that fired, for example
`"condition":{"kind":"select","select":"region:has-text('ready')"}`.
Timeout and session-gone responses omit `condition`.

`cleat wait ID --select SEL [--timeout SECONDS] [--json]` joins existing OR
conditions. It is level-triggered: an already matching screen succeeds
immediately, including with timeout zero. Exit status is 0 for ready, 1 for
timeout, and 2 for an error/session gone. Unsupported VT or malformed selector
fails immediately with status 2. JSON ready output includes `condition`.

Waits are advisory. Use the atomic guard (#310) for writes that depend on screen
state. Trees are built only for queries or active selector waits; selectors are
compiled at registration and all pending selector waits share one tree per tick.
The graph producer's release evaluation budget is under 5ms for the adversarial
200×50 screen from #316; no tree construction runs on ordinary output alone.
Packet-attached and HTTP-attached clients query the same session actor.

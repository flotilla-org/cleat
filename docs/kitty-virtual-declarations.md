# Kitty virtual declarations

Cleat exports original live `U=1` declarations separately from resolved
`TerminalImagePlacement` strips. A declaration exists even before any
placeholder cells have been drawn. Columns and rows are the declared values,
including zero (automatic geometry), rather than resolved strip dimensions.
The set includes every virtual declaration with a live image on the observed
screen. Historical views use their screen's declarations.

Each `TerminalVirtualPlacement` carries image ID and image generation, original
placement ID, `placement_id_explicit`, an opaque handle, creation-order sequence,
columns, rows, z, source rectangle and offsets. Match image data by image ID and
generation. A generation change must not reuse an old pixel asset.

## Placement identity and ordering

An omitted `p` exports `placement_id=0, placement_id_explicit=false`. A command
that sent `p=0` exports zero with `placement_id_explicit=true`. Neither case
exports Ghostty's internal placement ID. A nonzero explicit ID remains the
original numeric `p`.

Handles and creation-order sequences are assigned by the cleat VT instance;
handles are opaque and never Kitty `p` values. They remain stable during the
lifetime of an unchanged declaration, including observations, unrelated text,
and retained declarations whose image generation changes. Deletion, reset, or
replacement of declaration parameters ends that lifetime. Sequences increase
across both screens and are not reset by RIS. A new VT instance (including
session adoption on another host) establishes a new handle namespace.

To recreate the placeholder contract, send pixels, then re-emit the complete
live declaration set sorted by `creation_order`, then placeholder text. Preserve
omitted `p`; do not substitute the opaque handle or Ghostty internal ID for `p`.
When a declaration disappears, remove its previous target placement before
recreating the current set as necessary. The receiving terminal applies its own
Kitty resolution rules.

Exact fragment matching requires a **nonzero explicit** placement ID, together
with matching image ID and generation. A fragment with `placement_id=0` does
not identify a declaration, even if only explicit declarations were created.
This ambiguity also applies to explicit `p=0`. Fragments keep their existing
fields and behavior; Wheelhouse and `kitty_output.rs` continue rendering them
directly.

## Provider and packet compatibility

Provider ABI **11** stays intact: no existing C structure grows and no ABI-11
caller needs larger output storage. The additive, independently versioned
`cleat_virtual_placement` structure uses `CLEAT_VIRTUAL_PLACEMENT_VERSION=1`.
After acquiring a snapshot or render update, call the matching accessor:

```c
size_t count = 0;
const cleat_virtual_placement *declarations =
    cleat_session_render_update_virtual_placements(session, &count);
```

`cleat_session_snapshot_virtual_placements` serves an outstanding full-grid
snapshot in the same way. Arrays are borrowed until the corresponding release
or session destruction. An empty set returns NULL and count zero. The daemon
provider serves full initial render updates rather than full-grid snapshots.

Packet protocol **13** advertises `min_supported_version=12`. Peers announce
support in the JSON `/connect` subscribe body with `"virtual_placements": true`.
Only these peers receive `MSG_SESSION_RENDER_VIRTUAL` (29), containing a
`VirtualRenderPacket` with the existing `RenderPacket` plus the complete
virtual-declaration set. Image bytes precede the envelope as before; declarations
and fragments become visible together for one render generation. An empty set
replaces all preceding declarations after deletion. The ordinary render's
postcard layout is unchanged: its in-memory declaration field is skipped during
serialization.

Absent or false capability gives exactly the legacy protocol-12 hello, message type and payload;
images referenced only by undrawn declarations are not transferred to legacy
clients. Generic CLI/socket clients keep this path. The daemon-backed provider
opts in, while still accepting protocol-12 daemons and legacy render messages.
Redirect and transfer compatibility use the common protocol range.

## Pinned Ghostty API gap

The pinned fork `rjwittams/ghostty@1d7fb7a15` exposes declaration geometry but
its `PlacementId` getter drops the internal/external namespace tag. Two distinct
live placements can have the same numeric getter ID. Resolved fragments retain
the placeholder's ID, not its selected declaration's key. The C reproducer and
captured output are in
[`tests/fixtures/kitty-virtual-declarations`](../crates/cleat/tests/fixtures/kitty-virtual-declarations).

Cleat observes incoming Kitty control headers and their command boundaries to
retain original `p` presence and creation order, then reconciles those records
against Ghostty's live declaration getters. It does not parse image payloads or
invent declarations for rejected commands. Chunked transmissions retain the
first chunk's control metadata. Storage generation avoids walking declarations
on every plain-text write. Source geometry and image generations remain owned
by Ghostty.

A future consumer requiring exact matching of zero-ID fragments would need an
additive fork API exposing the namespace tag and resolved target key. This PR
makes no fork change. Its driver is
[rjwittams/katzensteg#121](https://github.com/rjwittams/katzensteg/issues/121)
(convoy `session-window-images`); Flotilla's cleat provider consumers receive
the additive interface, and Wheelhouse does not consume declarations.

If a live declaration cannot be attributed to an observed command (for example,
a control header exceeds the observer’s 4096-byte limit), cleat logs the metadata
error and withholds that screen’s declaration set until all its declarations are deleted
or the terminal is reset. Later commands cannot relabel an unattributed entry. Terminal
output and resolved fragments continue; cleat never guesses whether `p` was sent.
History captures reconcile on a copy of the registry and cannot change live handles.

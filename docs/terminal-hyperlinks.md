# OSC 8 destinations in render frames

`TerminalRenderStyle.hyperlink_uri` contains owned OSC 8 destination bytes for
that cell in its render frame. Empty bytes mean no destination. The C equivalent,
`cleat_render_style.hyperlink_uri`, borrows those bytes from the returned render
update. Copy them alongside cells before releasing the update or pulling again.
`has_hyperlink` and `hyperlink_id` remain descriptive metadata, not URL lookup keys.

Both live in-process renders and daemon renders carry this field. Tracked history
captures populate the same field as well as their existing per-cell `links` list.
Row replacement and scroll copy apply to URI data together with cell content.
A retained update never resolves a URI from a newer engine state.
Live render updates share a cumulative 1 MiB URI budget across all included rows,
matching the tracked-history resource limit. An over-budget update is rejected
rather than publishing a partially stripped frame; later valid output can recover. Snapshot-only
feeds do not expose URI destinations; use render updates for hyperlinks.

This changes the C layout (provider ABI **10**) and the postcard render payload
(packet protocol **11**). Rebuild native consumers and upgrade both daemon and
attach client together. Older packet protocols are rejected during negotiation;
serde defaults support older JSON, not older postcard layouts.

The packet-to-terminal attach renderer emits OSC 8 around linked graphemes and
closes each link before moving or rendering other content. Invalid UTF-8 and
control-bearing URI strings are not relayed, preventing OSC termination/injection.
The renderer never opens destinations. GUI consumers must show the destination
and apply their own explicit activation and URI-scheme policy.

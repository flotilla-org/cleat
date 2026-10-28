# An attachment carries many views; live views share one update stream

Status: accepted, 2026-10-28. Records [#399](https://github.com/flotilla-org/cleat/issues/399); refines how [ADR 0004](0004-daemon-hosts-a-set-of-sessions.md) describes the multiplexed connection.

ADR 0004 has a multi-pane client carry "its session streams" over one connection, and until protocol 16 each place a client showed a session was its own channel. A client showing one session twice (Wheelhouse showing a vessel's terminal in two workspaces, [wheelhouse#417](https://github.com/flotilla-org/wheelhouse/issues/417)) received the same renders twice and voted on the grid twice as two drivers.

## Decision

The connection carries **attachments** (a channel: one client's subscription to one session) and each attachment carries one or more **views**. The daemon already kept history positions per view; the wire now says so too.

1. **Per attachment:** the live render stream and its acknowledgement credit, image transfer, and clipboard effects. Clipboard reaches an attachment once while any of its views is a controller.
2. **Per view:** role, window size, focus, cell pixel size and history position. The grid is the intersection over controller views across all attachments; focus is the union over controller views; the earliest controller view (connection, then channel, then view) supplies the application's cell pixel size and other views' mouse positions are converted from their own cell size. Take control demotes every other controller view, including views of the same attachment.
3. **Live views share the stream.** The daemon sends one live render per change to the attachment, whatever its number of live views; the client fans it out, catching a lagging or returning view up from its copy of the live grid. A view in history gets its own frames under its own credit. Input from a controller view returns that view, and only that view, to live.
4. **Opening and closing.** A channel opens with its default view (view 0). Further views open and close with messages on the channel. Per-view messages travel wrapped in a view envelope; unwrapped ones address the default view. Closing the last view leaves the attachment open, its live stream paused, and touches nothing else on the connection.
5. **Opt-in compatibility.** A client asks for views when it connects; only then does the daemon advertise protocol 16. Every other peer keeps one implicit view per channel and byte-identical messages. A views client talking to an older daemon keeps its default view and reports the others closed.

## Considered options

- **One channel per view, deduplicated in the daemon.** Rejected: the daemon would still encode and send each render once per channel, which is the cost the change removes.
- **Attachment-level role and size.** Rejected: one host shows a session in places with different sizes, zoom levels and focus, and only some of them may drive.
- **Per-view live streams with shared encoding.** Rejected: a hidden or slow view would hold back its own credit while still costing a frame per change; one stream per attachment lets the client decide which views to present.

## Consequences

- The C API gains a view handle opened on a session handle; the session handle keeps the attachment and its default view, so existing clients are unchanged.
- An attachment whose views are all in history, or that has none, receives no live frames until a view follows the live stream again. Pacing for hidden views and per-connection prioritisation remain open.
- Presence lists one participant per view; two views of one attachment share a connection and channel id.

# A recording is a multi-track state timeline; image pixels come from the engine

[ADR 0001](0001-session-hosting-and-recreation.md) made recreation-from-recording
the persistence floor. [ADR 0002](0002-recording-on-by-default.md) made recording
on by default and decoupled the on-disk format from a permanent asciinema-export
invariant. This ADR sets the direction for what that format becomes once sessions
carry graphics.

Today a recording is a **byte log**: the daemon tees the PTY output stream into an
asciicast file. That is the right shape for text, and wrong for two things at once.

**It is incorrect for external-medium images.** Kitty graphics can transmit pixels
by shared memory (`t=s`), file (`t=f`), or temp file (`t=t`) instead of inline
(`t=d`). For those media the pixel bytes are *never* in the PTY stream — the engine
reads the shm/file itself and deletes temp files. So a recording of an
external-medium session contains a dangling reference and no pixels: it cannot be
replayed or recreated. This is not a corner case — it is the normal mode of
graphics-heavy producers like [katzensteg](https://katzensteg.dev), which streams
native app frames (games, emulators) over shm.

**It is unbounded for image-dominated sessions.** A session that repaints the full
window every frame — a TUI, or katzensteg at video frame rates — is, in byte-log
form, a torrent of near-full-frame transmissions where all but the latest are dead
weight. Storing every frame verbatim is both enormous and almost entirely
redundant.

We therefore stop treating a recording as one byte stream and treat it as a
**multi-track state timeline**: a *terminal track* (text, control, placements,
geometry) and an *image track* (actual frames) sharing one clock. The terminal
track stays asciicast. The image track gets its own fidelity policy — sampled,
decaying with age, and for image-dominated sessions an actual video encoding. The
**image bytes are sourced from the engine boundary**, not the PTY stream — which is
what makes capture medium-agnostic. Two tap points are open and not mutually
exclusive:

- **Pre-decode** — intercept the ingested source payload before the engine decodes it
  (e.g. the encoded PNG, which cleat already sees in the `decode_png` callback the
  engine calls back into, or the raw bytes read from a file/shm medium). This is the
  original transmitted form: typically far smaller than decoded RGBA and the most
  faithful record of what was sent.
- **Post-decode** — pull the decoded frame from the engine's graphics storage
  (`kitty_image_state()` + `with_kitty_image_data()`). Always available and uniform
  across formats and media, at the cost of size (raw RGBA) and a decode step removed
  from the original.

Pre-decode is the preferred capture where a source payload is available; post-decode
is the always-available fallback. Either way the just-landed provider image surface
is the enabler.

## Considered options

- **Byte-stream APC-G sniffer only** (the "Path 1" of #50). Parse `APC G` out of the
  PTY stream and re-emit it on replay. Correct for inline `t=d`, but it *structurally
  cannot* capture `t=f`/`t=t`/`t=s` pixels — they are not in the stream. Rejected as
  the whole answer; engine-sourced capture is required for external media. The
  sniffer remains useful for placement/lifecycle bookkeeping.
- **Inline every image into the asciicast** (`t=s`→`t=d` rewrite, pixels in the
  cast). Self-contained and keeps a single file, and is the right *export* form. As
  the *storage* form it is unbounded for video-rate sessions. Kept as the export
  flattening, rejected as the storage strategy for the image track.
- **Always use a media container (MP4/MKV) for every session.** Overkill for the
  common text session and a heavyweight muxer dependency on the hot path. Rejected as
  the default; adopted for the video tier only (see consequences).
- **Do nothing / accept lossy graphics** (status quo). Rejected: silent
  unreplayability of graphics sessions is a correctness bug, not a fidelity
  preference.

## Consequences

- The work lands as a ladder, each rung independently shippable and useful:
  1. *(today)* byte-log asciicast; inline images only; external images lost.
  2. **Engine-sourced image capture** ([#73](https://github.com/flotilla-org/cleat/issues/73))
     — capture image bytes at the engine boundary (pre-decode source payload where
     available, post-decode frame otherwise) at transmit time so shm/file/temp-file
     sessions become replayable. Capture happens while the engine is processing the
     transmit — before it ACKs receipt to the terminal, and therefore before the
     sender deletes a temp file or reuses a shm segment. Winning that race is the
     whole reason capture must sit at the engine boundary rather than re-reading the
     medium later. This is a **correctness fix** and the prerequisite for everything
     below.
  3. **Semantic compaction** ([#74](https://github.com/flotilla-org/cleat/issues/74))
     — drop superseded content at recognized safe points: output before a full-screen
     snapshot boundary (it is reconstructable from the snapshot) and image
     transmissions whose images are deleted or fully overdrawn.
  4. **Target frame rate with age decay** *(not yet tracked; an issue is filed when
     the rung is approached)* — for image-heavy sessions, keep frames at a target
     cadence that decays as history ages; recent history stays high fidelity, old
     history goes sparse, and the *latest* frame is always exact.
  5. **Adaptive video track** *(not yet tracked; an issue is filed when the rung is
     approached, and is where the "this is video" threshold and the
     sidecar-vs-promotion container choice below are decided)* — recognize an
     image-dominated session and route its frame strip to a real video encoder:
     cheap/fast codec while live, background re-encode of cold segments to a
     higher-ratio codec, keyframe-sampled decode on replay/seek.
- **The current state stays exact; dropped history is lossy by construction.** This
  is the same trade ADR 0002 already blessed (scrollback above the current activation
  is best-effort). Compaction and decay must never degrade reconstruction of the
  *current* screen or what an in-flight seek/recreation needs — only already-superseded
  history.
- **The asciinema-export invariant holds, with an honest caveat.** Export remains
  always possible by flattening the image track back to inline `t=d` frames against
  the terminal track. For decayed/compacted regions the export is necessarily lossy
  (it reflects the sampled frames that were kept), and for a video-tier track export
  means decoding frames back out — possible but not cheap. Exportability, not
  byte-for-byte player fidelity of the raw store, is the invariant. This refines, not
  weakens, ADR 0002: its "asciinema export is always possible" is preserved in the
  *capability* sense, but the cost profile changes — text export stays trivial, while
  video-tier export shifts from a cheap transform to a potentially expensive,
  user-triggered decode/re-encode. That cost shift is an accepted consequence.
- **Container format.** asciicast stays the base container and the export lingua
  franca; it is append-only, trivial to write, and right for the text-dominated
  common case. The video tier uses a real media container, and that container is
  **Matroska (MKV), not MP4**: MKV is designed for an arbitrary number of
  synchronized timed tracks including non-AV data tracks, it is stream/append
  friendly, and the video track is exactly its purpose; MP4's box structure is
  awkward for non-AV data and less append-friendly. Two shapes are viable, and the
  choice between them is **deliberately deferred to step 5** (it is not "either is
  fine" — it is "not chosen yet"); **sidecar is the recommended first form**. The
  options: a **sidecar** encoded video referenced by image-id + timestamp from cast
  events (keeps the simple cast, adds a file the AV tooling already understands), or
  **promotion** of the whole recording to a single MKV with a data track for terminal
  events once a session crosses the "this is video" threshold.
- **Storage pressure is a known operational concern.** Before compaction and decay
  exist (rungs 3–4), and in the window between them firing, a video-rate session can
  outrun available disk. Unlike the terminal recorder, which is fail-closed at
  session *start* (ADR 0002), the **image track is best-effort and fails open**:
  under storage pressure it degrades — drop frames, lower the target rate, or disable
  the image track — rather than killing a live session over graphics it could run
  without. This composes with the cooperative size-budget reaping in the control-plane
  design; the exact policy is an implementation detail, noted here so it is not
  mistaken for an oversight.
- **History rewriting is shared machinery.** Compaction, decay, and re-encoding all
  rewrite a recording in place (replace a prefix with a snapshot, drop dead frames,
  transcode a cold segment). [#71](https://github.com/flotilla-org/cleat/issues/71)
  (re-basing appended-cast timestamps) is the first instance of the same operation;
  the rewrite path should be built once and reused.
- **Front-truncation (ADR 0002) is the coarse case of this.** "Truncate everything
  before the current activation" is rung 3 at activation granularity; snapshot-boundary
  compaction is the within-activation refinement.
- **Relation to the custom codec (ADR 0002).** Semantic compaction (structural: drop
  dead bytes) is orthogonal to and composes with a statistical codec (compress what
  remains). For graphics-heavy sessions compaction and the video tier are the larger
  levers and come first.

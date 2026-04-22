# `transcript` end-bounds design

**Date:** 2026-04-22
**Issue:** [#52](https://github.com/flotilla-org/cleat/issues/52)

## Problem

`cleat transcript` today accepts a start bound only: `--since <offset>` or `--since-marker <name>`. Slicing to a specific later point requires hand-trimming the output. Agents repeatedly hit three end-bound needs:

1. Slice between two named markers
2. Slice from a marker until the recording goes idle (the "one step settled" pattern)
3. Slice from a marker until the next marker, whatever its name

This spec adds those end-bounds as first-class flags, with a consistent soft-ceiling behaviour when an end-bound is not reached.

## Scope

**In scope**
- Four new end-bound flags on `transcript`
- humantime duration parsing for `--until-idle`
- Backwards-compatible humantime upgrade on `wait --idle-time`
- Daemon-side slicing with new `StartBound` / `EndBound` enums passed across the service layer

**Out of scope**
- `expect` — waits for text appearance; end-bounds don't fit its shape
- `capture` — captures current screen, no slicing concept
- JSON output mode on `transcript` — separate feature
- `expect --timeout` humantime conversion — scope creep from `wait --idle-time`

## Flag surface

```
cleat transcript <id>
  [--since <offset> | --since-marker <name>]
  [--until <offset> | --until-marker <name> | --until-next-marker | --until-idle <duration>]
  [--raw]
```

- **Start bounds** are unchanged and already mutually exclusive via `conflicts_with`.
- **End bounds** are mutually exclusive among themselves. Enforced via `conflicts_with` on each flag.
- **Start and end bounds compose freely** — any start can pair with any end.
- **Omitted end bound** means "to end of recording," matching today's behaviour.
- **`--until-idle`** accepts humantime-suffixed durations: `500ms`, `2s`, `1m30s`.

### `wait --idle-time` harmonisation

`wait --idle-time` today accepts plain seconds (`--idle-time 2`). This spec extends it to accept either form:

- Plain numeric (`2`, `0.5`) → seconds, as today
- humantime-suffixed (`500ms`, `2s`) → parsed via `humantime`

Backwards-compatible. Implemented as a custom clap value parser that tries humantime first, then falls back to `f64`. The same parser is used for `--until-idle`.

## Semantics

### Miss behaviour

| End bound | Miss case | Action |
|---|---|---|
| `--until <offset>` | n/a — raw offset always resolves | — |
| `--until-marker <name>` | name not in marker table | Hard error: `marker 'X' not found` |
| `--until-marker <name>` | marker offset < start offset | Hard error: `marker 'X' precedes start` |
| `--until-next-marker` | no marker after start | Soft ceiling: slice to EOF; stderr note `# bounded by EOF (no marker after start)` |
| `--until-idle <duration>` | no gap of ≥N found | Soft ceiling: slice to EOF; stderr note `# bounded by EOF (no <duration> idle found)` |

When the intended bound is hit, no stderr note is emitted. This keeps agent workflows clean (redirect `2>/dev/null` if the note is noise) and gives interactive users a hint when an expected bound wasn't reached.

### Idle detection

"Idle" means: the first gap between two consecutive *output* events in the cast file whose duration is `>= N`.

- Events in asciicast v3 carry timestamps. Gap = `t[i+1] - t[i]`.
- Only output events participate (cast file type `'o'`). Markers, snapshots, and other event types are ignored for idle detection.
- The slice ends at the byte offset corresponding to the last output event *before* the gap (i.e., the slice includes the last event before idle starts).

### Next-marker detection

"Next marker" means: the marker whose byte offset is the smallest value strictly greater than the resolved start offset.

- Marker table today is a `HashMap<String, u64>` of name → offset. Resolution is a linear scan for `min offset where offset > start`.
- If markers end up placed out of chronological order (e.g., via some future tool that backdates markers), the "next" is still determined by offset, not by creation time. This matches the conceptual model of markers as *positions*, not events.

## Architecture

### Service-layer API

Daemon-side slicing keeps the logic where the cast file and marker table already live. Two new methods, matching the existing one-per-output-format pattern:

```rust
fn capture_slice_raw(
    &self,
    id: &str,
    start: StartBound,
    end: EndBound,
) -> Result<(Vec<u8>, SliceOutcome), ServiceError>;

fn capture_slice_text(
    &self,
    id: &str,
    start: StartBound,
    end: EndBound,
) -> Result<(String, SliceOutcome), ServiceError>;
```

```rust
enum StartBound {
    Offset(u64),
    Marker(String),
    FromBeginning,
}

enum EndBound {
    Offset(u64),
    Marker(String),
    NextMarker,
    IdleGap(Duration),
    EndOfRecording,
}

struct SliceOutcome {
    /// Byte range actually delivered.
    start_offset: u64,
    end_offset: u64,
    /// Whether the end was the one requested or a soft-ceiling fallback.
    hit_intended_end: bool,
    /// If hit_intended_end is false, a short reason for the stderr note.
    fallback_reason: Option<String>,
}
```

The `SliceOutcome` return lets the CLI layer decide whether to emit the stderr note without the service layer having to know about stderr.

### Existing `capture_since_*` methods

The existing `capture_since_raw`/`capture_since_text` methods are retired. Their call sites migrate to `capture_slice_{raw,text}` with `EndBound::EndOfRecording`. Retirement rather than keeping both forms avoids carrying two entry points that do the same thing.

Migration touches:
- `Command::Transcript` dispatch in `cli.rs:380-400`
- Any lifecycle tests that call the old methods (inventory during planning)

### Protocol layer

`StartBound` and `EndBound` need to cross the daemon socket. Serialisation:
- Both enums are tagged unions encoded as `(tag_byte, payload)`
- `Duration` serialised as `u64` milliseconds
- `String` for marker names, length-prefixed (existing protocol pattern)

Specific byte encoding deferred to planning — follows whatever convention the existing `Frame` encoding uses in `protocol.rs`.

### Clap flag structure

Four new flag fields on `Transcript { ... }`, each with `conflicts_with` on the other three to enforce mutual exclusion. Matches the pattern `--since` / `--since-marker` already uses.

```rust
Transcript {
    id: String,
    #[arg(long, conflicts_with = "since_marker")]
    since: Option<u64>,
    #[arg(long, conflicts_with = "since")]
    since_marker: Option<String>,

    #[arg(long, conflicts_with_all = ["until_marker", "until_next_marker", "until_idle"])]
    until: Option<u64>,
    #[arg(long, conflicts_with_all = ["until", "until_next_marker", "until_idle"])]
    until_marker: Option<String>,
    #[arg(long, conflicts_with_all = ["until", "until_marker", "until_idle"])]
    until_next_marker: bool,
    #[arg(long, conflicts_with_all = ["until", "until_marker", "until_next_marker"], value_parser = parse_humantime_or_seconds)]
    until_idle: Option<Duration>,

    #[arg(long)]
    raw: bool,
}
```

### Duration parsing

One shared value parser function:

```rust
fn parse_humantime_or_seconds(s: &str) -> Result<Duration, String> {
    // Try humantime first (matches "500ms", "2s", "1m30s", etc.)
    if let Ok(d) = humantime::parse_duration(s) { return Ok(d); }
    // Fall back to float seconds ("2", "0.5")
    s.parse::<f64>()
        .map(Duration::from_secs_f64)
        .map_err(|_| format!("invalid duration: {s}"))
}
```

Used by `--until-idle` and by `wait --idle-time`. Shared location: `crates/cleat/src/duration_parser.rs` or inline in `cli.rs` — planning decides.

### Dependency

Add `humantime = "2"` to `crates/cleat/Cargo.toml`. Small, widely-used crate, no transitive bloat.

## Testing

### Unit

- `parse_humantime_or_seconds`: covers `500ms`, `2s`, `1m30s`, `2`, `0.5`, and invalid inputs
- `EndBound` resolution against a mock event stream: test each variant including soft-ceiling fallback
- Idle-gap detection: no events, single event, gap exactly == N, gap > N, no gap found (EOF fallback), only non-output events between two output events

### Integration

- Service layer: `capture_slice_{raw,text}` against a fixture cast file with known markers and idle gaps
- Fallback signalling: `SliceOutcome.hit_intended_end` and `fallback_reason` populated correctly

### CLI

- Flag parsing: mutual-exclusion errors for conflicting bounds
- Humantime parsing: both forms accepted on `--until-idle` and `--idle-time`
- Dispatch: verify the right `StartBound` / `EndBound` is constructed from each flag combination

### End-to-end (lifecycle test)

- Launch a session with recording; mark A; emit output; mark B; emit output; mark C
- `transcript --since-marker A --until-marker B` returns exactly the bytes between A and B
- `transcript --since-marker B --until-next-marker` returns exactly the bytes between B and C
- `transcript --since-marker A --until-idle 200ms` (with a controlled idle pause) terminates at the expected point
- Soft-ceiling case: `transcript --since-marker C --until-idle 10s` on a short session falls back to EOF and emits the stderr note

## Rollout

Single PR, single branch `transcript-between-markers`. No feature flag needed — new surface only, existing surface is either unchanged (`--since`, `--since-marker`, `--raw`) or harmonised backwards-compatibly (`wait --idle-time`).

## Open questions / follow-ups

- **JSON output on `transcript`** — if agents want structured access to the `SliceOutcome` metadata (hit/miss status, actual range), that's a separate feature. Not blocking here.
- **`expect --timeout` humantime** — same pattern as `wait --idle-time`; obvious future symmetry but out of scope.
- **`--until-duration <duration>`** (time-based end, independent of idle) — not in the note's original asks; file a follow-up if requested.

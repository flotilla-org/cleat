# `cleat replay` design

**Date:** 2026-04-22
**Issue:** [#53](https://github.com/flotilla-org/cleat/issues/53)

## Problem

Agents debugging with cleat repeatedly need to replay recorded output at human-viewable pace. Today the only option is `cat session.cast | jq '.[2]' | some-decoder` — the raw pty output flashes past too fast to inspect. The stage-11 agent note flagged this as the second-highest-priority improvement after the behavioral-model docs:

> We ended up manually replaying captured byte files with `cat`, which is too fast to visually inspect. A built-in timed replay for recordings or transcript slices would be very useful.
> [...]
> This seems especially valuable for agent debugging, where the point is often to compare two very similar streams and understand where behavior diverges.

`replay` is the built-in answer: play a cast file (or a slice of one) at controlled speed to stdout, with the same bound flags the just-merged `transcript` command has.

## Scope

**In scope**

- New `replay` subcommand.
- Positional path argument (common case) or `--session <id>` flag (runtime lookup).
- The same 6 bound flags `transcript` accepts (`--since`, `--since-marker`, `--until`, `--until-marker`, `--until-next-marker`, `--until-idle`), with one restriction: marker-based flags require `--session` (markers are resolved through the daemon socket). Raw offset and `--until-idle` work with either the positional path or `--session`.
- `--speed <f64>` — gap multiplier. Validated positive finite.
- `--max-idle <duration>` — clamp gaps after speed scaling. Humantime format shared with `--until-idle` / `wait --idle-time`.
- Streaming event reader in `cast_reader` so replay doesn't buffer the whole file.

**Out of scope**

- `--step` and any stepwise mode — deferred until #23 / #24 give us a semantic definition of "step" (a step in the child program's execution, not a cast-file line or byte chunk). Filing a follow-up on whichever of those issues is the better home.
- `--output <file>` flag — `replay > file.bin` via shell redirection is already possible.
- Progress indicator / elapsed-time display.
- `--diff` or side-by-side multi-stream comparison.
- Resize-event replay (re-emit `CSI ? 8 ;row; col t` on recorded resize events). The recording and replay terminals may be different sizes regardless; flagged as a known follow-up.

## CLI surface

```
cleat replay <path>                                       # positional path
cleat replay --session <id>                               # runtime lookup
cleat replay <path> --speed 0.5                           # half-speed
cleat replay <path> --max-idle 200ms                      # clamp idle
cleat replay <path> --since-marker a --until-marker b     # slice
```

- **Path vs. `--session`** — mutually exclusive, one required. Positional accepted for the common "replay this file" case. `--session <id>` is for replaying a running-or-recently-run session's cast file from the runtime root (`$XDG_RUNTIME_DIR/cleat/<id>/session.cast`).
- **Slice bounds** — all 6 flags reused from `transcript`. Same mutual-exclusion rules among end bounds; same start-bound-required rule. Shared code path via `SessionService::resolve_slice_range` (new extracted helper — see Architecture).
- **`--speed <f64>`** — positive, finite multiplier. Default `1.0`. `0.5` = half-speed (gaps stretched 2×), `2.0` = double (gaps halved). Rejected if ≤ 0 or non-finite with the error `invalid speed: <value>`.
- **`--max-idle <duration>`** — clamp any inter-event gap to this maximum after speed scaling. Reuses `duration_parser::parse_humantime_or_seconds`. `0ms` is legal and means "skip all pauses, dump at maximum speed."

### `--session` marker resolution

For `--session <id>` with `--since-marker` / `--until-marker` / `--until-next-marker`, marker resolution uses the existing daemon socket path (`SessionService::resolve_marker` / `resolve_next_marker_after`). **The session's daemon must be alive when replay runs** — markers live in the daemon's in-memory map, not the cast file (markers ARE written to the cast file as `EventCode::Marker` events, so a cast-file marker scanner could support offline `--session` replay in the future, but that's out of scope here).

Marker flags with a positional path (no `--session`) are rejected by clap via `requires = "session"` on each marker flag. Raw `--since` / `--until` / `--until-idle` work with either form.

## Event handling policy

Walk events from the resolved start offset to the resolved end offset. For each event:

| Code | Action |
|---|---|
| `Output` (`o`) | Sleep for the (scaled, clamped) gap, then write `event.data` bytes to stdout and flush |
| `Input` (`i`) | Skip — replaying input would re-send the original user's keystrokes, confusing |
| `Resize` (`r`) | Skip — replay terminal may be a different size than the recording; see follow-up |
| `Marker` (`m`) | Skip — metadata |
| `Exit` (`x`) | Skip — metadata |
| `Custom(_)` | Skip — unknown |

## Timing formula

```rust
let mut prev_time = Duration::ZERO;
for event in iter_output_between(path, start, end)? {
    let event = event?;
    // cast_reader already filters to Output events, so event.code is Output here.
    let gap = event.time.saturating_sub(prev_time);
    let scaled = Duration::from_secs_f64(gap.as_secs_f64() / speed);
    let clamped = max_idle.map_or(scaled, |m| scaled.min(m));
    std::thread::sleep(clamped);
    stdout.write_all(event.data.as_bytes())?;
    stdout.flush()?;
    prev_time = event.time;
}
```

### Timing with arbitrary start offsets

The existing `cast_reader` seek semantics (documented on `read_output_since`) reset delta accumulation to zero at the seek point. Consequence for replay: when a `--since <offset>` or `--since-marker <name>` seek lands between two events in the cast file, the first decoded event's `event.time` is the delta from the *previous* event that was written to the file — which may or may not be inside our slice range.

Practical effects:

- **Start at marker / offset-right-after-marker** (common case): the first output event's `event.time` is the natural inter-event delay from whatever output the child emitted most recently before the mark. Replay pauses by that amount before the first output. Usually what users want — it preserves the "rhythm" of the session starting from the mark.
- **Start at arbitrary offset that lands in a long idle period**: first event's `event.time` can be many seconds, producing an initial pause before any output. Users wanting to skip this supply `--max-idle`, which clamps exactly this case.
- **`prev_time` initialization**: the replay loop starts with `prev_time = Duration::ZERO` to match the reader's post-seek reset. First event's gap is therefore `event.time - 0 = event.time`.

This matches what `transcript` does (concatenates output without regard to timing) and is the simplest semantics to reason about. If a more sophisticated mode is ever needed (e.g. "start immediately, no leading pause"), `--max-idle 0ms` is the knob.

## Architecture

### File layout

- **New module** `crates/cleat/src/replay.rs` — replay loop, `ReplayOptions` struct, unit-tested timing function. Small (< 150 lines).
- **Modify** `crates/cleat/src/cast_reader.rs` — add `iter_output_between` streaming iterator alongside the existing `read_output_between`. The Vec-returning function stays for callers that legitimately want it all (`capture_slice_inner`).
- **Modify** `crates/cleat/src/cli.rs` — add `Command::Replay { ... }` variant and dispatch.
- **Modify** `crates/cleat/src/server.rs` — extract bound resolution from `capture_slice_inner` into a separate `resolve_slice_range(id, StartBound, EndBound) -> Result<(u64, u64, Option<FallbackReason>), String>`. Both `capture_slice_inner` and the new replay dispatch use it. Keeps replay independent of `capture_slice_inner` (which allocates a Vec).
- **Modify** `crates/cleat/src/lib.rs` — export `pub mod replay;`.

### Streaming iterator

```rust
// in cast_reader.rs
pub fn iter_output_between<'a>(
    path: &'a Path,
    start: u64,
    end: u64,
) -> Result<impl Iterator<Item = Result<Event, String>> + 'a, String> {
    let file = std::fs::File::open(path).map_err(|e| format!("open {path:?}: {e}"))?;
    let mut reader = BufReader::new(file);
    if start > 0 {
        reader.seek(SeekFrom::Start(start)).map_err(|e| format!("seek: {e}"))?;
    }
    Ok(OutputEventIter {
        reader,
        byte_pos: start,
        end,
        prev_time: Duration::ZERO,
        first_line: start == 0,
    })
}

struct OutputEventIter<R> {
    reader: BufReader<R>,
    byte_pos: u64,
    end: u64,
    prev_time: Duration,
    first_line: bool,
}

impl<R: BufRead> Iterator for OutputEventIter<R> {
    type Item = Result<Event, String>;
    fn next(&mut self) -> Option<Self::Item> {
        // Loop until we find an Output event, EOF, or the byte range ends.
        // Returns the next Output-code Event, skipping non-Output events
        // and updating prev_time via decode_event.
    }
}
```

Implementation detail: the iterator filters to `EventCode::Output` internally so the replay loop doesn't need to.

### Bound resolution extraction

Current `capture_slice_inner` in `server.rs` does all of: resolve start, resolve end (with fallback-on-miss bookkeeping), read output, format result. The bound-resolution half is reusable. Extract:

```rust
pub(crate) fn resolve_slice_range(
    &self,
    id: &str,
    start: StartBound,
    end: EndBound,
    cast_path: &Path,
) -> Result<(u64, u64, Option<FallbackReason>), String> { ... }
```

`capture_slice_inner` calls this and then reads via `read_output_between`. Replay calls this and then reads via `iter_output_between`. No duplicated logic.

### Error handling

- **Path doesn't exist** → `replay: no such file: <path>`
- **File I/O errors** (seek fail, read fail) → surface with file-path context.
- **Malformed lines mid-stream** → silently skipped, matching the existing `cast_reader` behavior used by `transcript` and `capture`. The `iter_output_between` iterator yields only successfully-decoded events; decode errors are swallowed inside the iterator to preserve this invariant. Changing that reader-wide would be a separate design concern with knock-on effects for the merged `capture_slice_*` path, so it stays out of this spec.
- **Broken pipe** (stdout consumer hangs up) → handle cleanly, exit 0. Rust's default behavior is to return `ErrorKind::BrokenPipe`; the replay loop treats it as end-of-replay.
- **SIGINT** (Ctrl-C during sleep) → standard exit code 130. `std::thread::sleep` is interruptible by signal; no special handler.
- **Speed ≤ 0 or non-finite** → clap value-parser rejects at parse time with `invalid speed: <value>`.
- **Marker flags with positional path** (`--since-marker` / `--until-marker` / `--until-next-marker` with no `--session`) → enforced at the clap layer via `requires = "session"`; clap emits the standard `the argument '--since-marker <...>' requires '--session <...>'` message. No runtime check needed.

## Testing

### Unit tests

In `replay.rs`:
- Timing calculation: gap × speed / speed + clamp edge cases. Table-driven (gap, speed, max_idle) → expected sleep.
- Speed validator: positive-finite cases pass, ≤ 0 / NaN / inf fail.

In `cast_reader.rs`:
- Iterator yields same events as the Vec-returning version on a fixture file (equivalence test).
- Iterator respects byte-range bounds (same as existing `read_output_between` tests).
- Iterator skips non-output events.

### Integration tests

In `tests/replay.rs` (new file):
- Fixture cast file with known events. Run replay with `--speed 1000` (fast), capture stdout, verify byte-for-byte match with the output-event concatenation.
- Same but with `--max-idle 0ms` — expect zero sleeps, output identical.
- Slice test: replay with `--since <offset>` / `--until <offset>` on the fixture, verify only the sliced output appears.

### Lifecycle test (one)

In `tests/lifecycle.rs`, with the session daemon kept alive for the duration:

- Create a session with recording, named_mark A, send a few lines with short sleeps, named_mark B, send more.
- **While the daemon is still running**, invoke `replay --session <id> --since-marker A --until-marker B --speed 1000`.
- Assert stdout contains the expected middle bytes and not the trailing bytes.
- After the assertion, kill the session.

This specifically exercises the socket-backed marker-resolution path. Offline `--session` replay (post-daemon-exit) is out of scope — a cast-file marker scanner would be needed, and that's its own follow-up.

Timing-sensitive; use a fast speed multiplier to keep the test under a second.

## Rollout

Single PR, single branch `transcript-replay`. No feature flag — new subcommand.

Follow-ups:
- Stepwise (`--step`) when #23/#24 land. A comment on the right issue will point back to this spec's omission.
- Resize-event replay (follow-up issue if anyone asks).
- Cast-file marker scanning for `--session <id>` with dead daemons (follow-up issue).
- `--diff` / side-by-side mode (not this PR).

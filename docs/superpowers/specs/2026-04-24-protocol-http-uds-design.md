# HTTP-over-UDS protocol design

**Date:** 2026-04-24
**Status:** Design spec; not yet linked to an implementation issue.
**Related issues:** #7 (FD transfer), #8 (session handover), #9 (event stream), #29 (VT transcoding), #57 (agent control plane), #58 (VT-authoritative-always)

## Problem

Today's daemon-client protocol is a custom binary frame format defined in `crates/cleat/src/protocol.rs`: 1-byte tag + 4-byte LE length + bespoke per-variant payload. The daemon is sync (nix-based poll loop), per-session, one UDS file per session.

The shape works but has costs:

- No `curl` interaction with the control plane — every operation requires the cleat client binary.
- No standard tunneling story for tools — `socat`, `ssh -L`, or wireguard relay can move the UDS bytes, but consumers on the far side still need bespoke decoding.
- No client SDKs in other languages without writing a fresh decoder.
- Bespoke binary framing is unfamiliar to readers; structured REST is the lingua franca for both agents and devs.
- Async daemon was already wanted for other reasons; HTTP-on-UDS naturally requires it at the network edge.

The shift: REST over HTTP/1.1 on UDS for the control plane, HTTP `Upgrade` for the bidirectional attach plane (reusing the existing `Frame` codec post-101), and a sync core API that's cleanly embeddable as a library for in-process consumers.

## Scope

**In scope**

- REST control plane over HTTP/1.1 on UDS — `sessions`, `markers`, `recording`, `transcript`, `keys`, `signal`, `resize`, `wait`, `expect`, `screen`, `inspect` endpoints.
- Per-daemon collection model — a daemon owns 1+ sessions; cross-daemon aggregation stays in the client.
- HTTP `Upgrade` for attach (`Upgrade: cleat-attach/1`) — post-101, the socket speaks the existing binary `Frame` codec verbatim.
- Hard cutover from the existing binary frame protocol — no transitional shim code in the final state.
- Sync core API (`cleat::Session::*`) embeddable as a library with no transport deps. Daemon builds with `daemon`/`client` Cargo features enabling tokio/axum/reqwest.
- Async daemon implementation (tokio + axum) at the network edge.
- Forward-compat seam for FD passing (#7) via `Upgrade: cleat-fd/1` — design only, no implementation in v1.

**Out of scope**

- **Auth tokens / remote identity.** Trust boundary remains UDS perms; ssh-forwarded UDS works (flotilla case) without changes. Clear seam to add bearer-token auth later.
- **Browser/JS direct support.** A future need is served by a reverse proxy fronting the UDS; in-tree browser support is not v1.
- **OpenAPI as source-of-truth.** Markdown documentation for v1; sprinkle utoipa annotations later (code-first, low-disruption retrofit). Skipping the Oxide/progenitor ecosystem.
- **Multi-attach with N controllers / M watchers.** Mirror today's behavior — second attach gets `409 Conflict`. Future zellij-like model slots in cleanly when needed.
- **FD passing implementation.** Design the seam, defer the work.
- **Backwards compatibility.** Hard cutover. We control all installs (single user/team consumer).
- **WebSocket as the attach framing.** Considered and rejected: adds tokio-tungstenite dep, per-frame overhead, reinvents `AttachInit`/`Input`/`Output` as JSON message types when they're already perfectly good binary frames. Easy to add later via a different `Upgrade:` token if browser/dashboard use cases emerge.
- **Versioning prefix in URLs.** No `/v1/` for v1. Add `/v2/` only if a future breaking change requires it.

## Architectural decisions

### Crate layout: single crate, feature-gated

Stay with the existing `cleat` crate. Add Cargo features:

```toml
[features]
default = []
daemon = ["dep:tokio", "dep:axum", "dep:hyper"]
client = ["dep:reqwest"]
ghostty-vt = []  # existing

[[bin]]
name = "cleat"
required-features = ["daemon", "client"]
```

The library API (sync `Session::*`) is always available with no features enabled. Embedders use `default-features = false` and pay zero transport cost. CLI binary requires both features. CI matrix builds both feature configs.

YAGNI vs splitting into `cleat-core` + `cleat`: split only if the dep boundary actually leaks. Today's monolithic crate is fine.

### Service core: sync API with seams

The core (session service, VT engine, recording, transcoding) exposes a sync API. Internal communication between subsystems (PTY pump, VT engine, recording) uses channels or trait abstractions where natural — leaves "seams" where async tasks could be introduced internally without changing the public API.

Pragmatic stance: don't over-abstract upfront. Identify natural boundaries (e.g. PTY pump → VT engine, recording sink) and use queue-based communication where it falls out, direct calls where it doesn't.

### HTTP framework: axum

Mainstream Rust async web framework. Plays cleanly with utoipa later. hyper underneath if we need lower-level control on the Upgrade path.

### Long-running operations: blocking POST

`POST /sessions/{id}/wait` and `POST /sessions/{id}/expect` block the HTTP response until the condition is satisfied or the body's `timeout_ms` elapses. No async-job pattern, no SSE, no client polling.

No HTTP idle-timeout middleware: we control all installs, no surprise reverse-proxy interference. Connection lifetime is governed by the request's own `timeout_ms` parameter.

Wait/expect timing out is `200 OK` with body `{status: "timeout", elapsed_ms: N}` — the request succeeded; the wait result is "timeout."

### Multi-attach: mirror today's behavior

A second attach to a session that already has a controller returns `409 Conflict`. Today's `Frame::Busy` translates directly. The future zellij-like N-controller/M-watcher design (size intersection across viewers, etc.) is a separate spec.

## URL/resource shape

Per-daemon collection. A daemon owns 1+ sessions; cross-daemon listing stays in the client (scan known directory tree for UDS files).

### Sessions

| Method | Path | Replaces | Notes |
|---|---|---|---|
| `GET`    | `/sessions` | (CLI `list`) | List sessions this daemon owns |
| `POST`   | `/sessions` | (CLI `spawn`) | Body `{cmd, args, cwd, cols, rows, vt_engine, recording}` → `SessionInfo` |
| `GET`    | `/sessions/{id}` | `Inspect`  | Returns `InspectResult` |
| `DELETE` | `/sessions/{id}` | (CLI `kill`) | Terminate child + clean up |

### Per-session ops

| Method | Path | Replaces | Notes |
|---|---|---|---|
| `POST` | `/sessions/{id}/keys` | `SendKeys` | Body `{bytes}`; `?mark=name` for `SendKeysWithMark` |
| `POST` | `/sessions/{id}/resize` | `Resize` | Body `{cols, rows}` |
| `GET`  | `/sessions/{id}/screen` | `Capture` | Rendered text via `screen_grid` |
| `POST` | `/sessions/{id}/signal` | `Signal` | Body `{signal, target}` |
| `POST` | `/sessions/{id}/wait`   | `Wait` | Blocking; body `{conditions, timeout_ms}` |
| `POST` | `/sessions/{id}/expect` | `Expect` | Blocking; body `{text, since_offset, timeout_ms}` |

### Markers

| Method | Path | Replaces |
|---|---|---|
| `GET`  | `/sessions/{id}/markers` | (lists all from `RecordingInspect`) |
| `POST` | `/sessions/{id}/markers` | `Mark` (body: `{name?}`) |
| `GET`  | `/sessions/{id}/markers/{name}` | `ResolveMarker` |
| `GET`  | `/sessions/{id}/markers:next?after=N` | `ResolveNextMarker` (Google AIP `:verb` style) |

### Recording

| Method | Path | Replaces |
|---|---|---|
| `GET`  | `/sessions/{id}/recording` | (status from `RecordingInspect`) |
| `PUT`  | `/sessions/{id}/recording` | `RecordControl` (body: `{enabled}`) |

### Transcript

| Method | Path | Replaces |
|---|---|---|
| `GET`  | `/sessions/{id}/transcript?since=N&until=...&since_marker=...&until_marker=...&until_idle=...` | `capture_slice_*` |

### Attach (HTTP Upgrade — see below)

| Method | Path |
|---|---|
| `GET`  | `/sessions/{id}/attach` (`Upgrade: cleat-attach/1`) |

### Daemon-level

| Method | Path | Notes |
|---|---|---|
| `GET`  | `/` | Daemon identity: `{id, owns: [...session-ids], started_at, version}` |
| `GET`  | `/healthz` | Liveness probe |

### Error model

- HTTP statuses: `400` bad input, `404` no such resource, `409` busy (replaces `Frame::Busy`), `422` validation, `500` internal, `504` server-side timeout (not the wait operation timing out — that's a successful response).
- Body shape: `{error: "human message", code: "BUSY"|"NOT_FOUND"|...}`. Uniform across endpoints.

## Attach plane

### Handshake

```
GET /sessions/{id}/attach HTTP/1.1
Upgrade: cleat-attach/1
Connection: upgrade
```

Server checks "is busy" (single-attach today) at the HTTP layer:

- **Busy** → `409 Conflict` (no upgrade), JSON error body
- **Otherwise** → `101 Switching Protocols` + `Upgrade: cleat-attach/1`

### Post-101: existing frame protocol verbatim

The socket speaks the existing wire format: 1-byte tag + 4-byte LE length + payload. `AttachInit`/`Ack`/`Input`/`Output`/`Resize`/`Detach` etc. are unchanged.

What changes vs today:
- Attach is initiated via HTTP `Upgrade` instead of opening a raw UDS socket and writing `AttachInit` immediately.
- Daemon's accept loop parses HTTP and recognizes the Upgrade before handing off to the existing frame-protocol handler.
- Everything else (frame variants, encoding, lifecycle) is identical.

What this preserves:
- All existing `Frame::*` encoding/decoding code is reused.
- Same wire format on the actual attach data.
- Same client-side attach loop, just preceded by an HTTP request.

### Subprotocol versioning

Lives in the `Upgrade:` token: `cleat-attach/1`, `cleat-attach/2` later if framing changes. Header-based dispatch in the daemon makes adding a real WebSocket attach (`Upgrade: websocket`) straightforward later without restructuring.

### Output backpressure

Today's daemon must already have an answer for slow clients (drop, coalesce, buffer). Mirror that on the Upgrade-attach handler. Flagged as an implementation concern for the plan.

## FD passing seam (deferred implementation)

For #7 (FD transfer / sibling session spawning), use the same Upgrade pattern:

```
GET /sessions/{id}/fd HTTP/1.1
Upgrade: cleat-fd/1
Connection: upgrade
```

Post-101, both sides switch from `read`/`write` to `recvmsg`/`sendmsg` with `SCM_RIGHTS` cmsg payloads. Tiny custom message envelope on top. UDS sockets support `SCM_RIGHTS` regardless of what application protocol they've spoken, so no second socket file is needed.

**v1 deliverable:** ensure the daemon HTTP layer cleanly hands off the raw socket on Upgrade so a future FD handler can grab it. axum supports raw upgrade via hyper's `OnUpgrade`.

`SCM_RIGHTS` is Unix-only; cleat already is. Non-issue.

## Migration sequencing

Hard cutover, but staged.

### Step 1 — Prep PR, lands on main

**Sync core library API surface.** Refactor `crates/cleat/src/lib.rs` to expose a clean public API (`cleat::Session::spawn`, `send_keys`, `capture`, `wait`, `expect`, `inspect`, `mark`, etc.) over the existing internals. No transport changes. Useful immediately for TUI tests.

This PR has standalone value regardless of whether the protocol shift ships.

### Step 2 — Branch `protocol-shift`, disciplined commits

Each commit on the branch compiles and passes its own tests (bisect-clean pattern from PR #48).

- **2a.** Add `daemon` and `client` Cargo features (no behavior yet); CI matrix builds both feature configs.
- **2b.** Add axum + tokio + reqwest as feature-gated deps.
- **2c.** Implement REST control plane handlers under `daemon` feature; new daemon entry path co-exists with old (selected via a CLI flag during branch development; default unchanged).
- **2d.** Implement HTTP-Upgrade attach handler reusing existing `Frame` codec.
- **2e.** Port client (`cleat attach`, `cleat send-keys`, etc.) to HTTP/Upgrade transport under `client` feature.
- **2f.** Port integration tests (`tests/lifecycle.rs` and others) to the new daemon.
- **2g.** Switch defaults: new daemon is default, old is opt-in via deprecated flag.
- **2h.** Delete old daemon code path, old client transport path, unused `Frame::*` variants (anything not used post-Upgrade).
- **2i.** Update README behavioral model section, document REST endpoints in markdown (`docs/api.md` or similar).

### Step 3 — Final merge

Squash or merge-commit, reviewer's choice. Bisect-clean history allows walking commits during review.

### Step 4 — Cleanup PRs on main

For things that fall out — e.g. `transcript --raw` becomes meaningful once #29 lands; old protocol docs deletion; etc.

### Estimated size

- Prep PR: small (lib refactor, no behavior change).
- Cutover PR: plausibly 2–4 kloc diff over ~7 kloc touched, with significant deletion in step 2h.
- Cleanup: ~2–3 small PRs.

## Open issues / risks

- **Output backpressure on attach** — design TBD; mirror today's behavior, document during implementation.
- **Test porting load** — `lifecycle.rs` and other integration tests are substantial. The cutover PR's size depends heavily on how mechanical the port is.
- **utoipa retrofit** — when we want OpenAPI, sprinkling annotations on existing axum handlers is the canonical low-disruption path. No structural concern; just future work.
- **Discovery of UDS paths** — cross-daemon listing stays client-side (scan known directory tree). Today's behavior; document but no new design.
- **Session ID URL safety** — today's IDs are UUID-based directory names; URL-safe, no escaping needed. Document as part of the API.

## What this design does NOT decide

- Specific axum extractor patterns or handler shapes.
- JSON serde schemas for individual endpoint request/response bodies (will be designed during plan).
- Detailed test fixtures for the new transport.
- The exact Cargo dep version pins.
- Whether to use `tower-http` middleware (probably yes for tracing/error mapping; design during plan).

These belong in the implementation plan, not the spec.

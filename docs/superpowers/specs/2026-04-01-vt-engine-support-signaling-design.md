# VT Engine Support Signaling Design

**Date:** 2026-04-01

## Goal

Make it unmistakably clear to humans and agents that cleat's current passthrough VT engine is not intended for real usage and that Ghostty is the only functional VT engine today.

## Context

Today the repository technically encodes the difference between `ghostty` and `passthrough`, but many surfaces still present them as peers:

- `--vt` help text is neutral
- non-`ghostty-vt` builds silently fall back to passthrough semantics
- tests and metadata normalize passthrough as a normal runtime choice
- agents often select passthrough, then complain when replay/capture features are missing

That creates the wrong mental model. The repo should instead teach a much stronger truth:

- `ghostty` is the only functional VT engine today
- `passthrough` is a placeholder/test-only seam, not a real runtime engine
- builds without `ghostty-vt` are non-functional for real cleat usage
- a future Rust VT engine may become another functional engine later, so the messaging should distinguish functional engines from placeholder ones rather than hard-coding permanent Ghostty exceptionalism

## Decision Summary

1. Keep the internal passthrough implementation for development/testing seams.
2. Do not present passthrough as a peer of Ghostty in normal user-facing semantics.
3. Treat builds without `ghostty-vt` as **non-functional placeholder builds** rather than supported real builds.
4. Emit loud build-time and runtime messaging whenever cleat is built without Ghostty support.
5. Update structured and textual interfaces so agents can detect engine support status directly instead of inferring from partial behavior.

## Supported State Model

### Functional build

A build with `ghostty-vt` enabled is the only supported real cleat build today.

Properties:

- default functional VT engine is Ghostty
- replay/screen capture features work
- all README/install/verification paths should point here

### Non-functional placeholder build

A build without `ghostty-vt` may still compile so contributors can work in the repo, but it is not a supported real-use binary.

Properties:

- must identify itself as non-functional for real terminal usage
- must never imply passthrough is a valid normal runtime choice
- should direct the user to `./tools/prepare-ghostty-vt.sh` and `--features ghostty-vt`

## User-Visible Policy

Every user-facing and agent-facing surface should encode these rules:

- Ghostty is currently the only functional VT engine.
- Passthrough is placeholder/test-only and unsupported for real use.
- A cleat binary built without Ghostty support is non-functional for real use.
- Future functional Rust VT engines may be added later.

## Concrete Changes

### 1. Build-time signaling

Add or expand `build.rs` so no-`ghostty-vt` builds emit loud Cargo warnings such as:

- cleat was built without `ghostty-vt`
- this binary is non-functional for real terminal usage
- Ghostty is currently the only functional VT engine
- passthrough is placeholder/test-only
- to build a functional binary, run `./tools/prepare-ghostty-vt.sh` and build with `--features ghostty-vt`

Also expose a compile-time status constant or equivalent so the CLI/runtime can print consistent messaging.

### 2. CLI and help text

Update neutral help text like `Virtual terminal engine` to policy text such as:

- `VT engine. Ghostty is currently the only functional engine; placeholder engines are for testing/development only.`

Add top-level help/about text warning that builds without `ghostty-vt` are non-functional for real use.

If passthrough remains selectable in some builds, its user-facing presentation must be self-disqualifying even if the internal enum variant remains `Passthrough`.

### 3. Runtime guardrails

When cleat is run without `ghostty-vt`:

- commands should not quietly behave as if this is a normal supported mode
- startup and relevant CLI flows should print explicit warnings or fail with guidance
- error messages should say the binary is non-functional without Ghostty support
- passthrough selection errors should call it placeholder/test-only and unsupported for real use

The key semantic shift is:

> no Ghostty support means "you built a non-functional cleat binary", not "passthrough is the default supported runtime"

### 4. Structured metadata for agents

Where structured output already reports VT engine information, add status information that agents can read directly.

Examples:

- `vt_engine: "passthrough"`
- `vt_engine_status: "placeholder"`
- `functional_vt_available: false`

This avoids agents treating the engine name alone as proof of support.

### 5. Documentation and install flow

Update README and related docs so the first message is:

- Ghostty is currently the only functional VT engine
- builds without Ghostty are incomplete/non-functional placeholder builds
- passthrough is not for real use
- a future Rust VT engine may exist later

Quickstart and install flows should point to preparing Ghostty and building with `ghostty-vt`.

### 6. Tests

Adjust tests so the policy is locked in:

- no-`ghostty-vt` tests should assert non-functional-build warnings/behavior
- help text tests should assert Ghostty-only functional wording
- passthrough tests should live under explicit placeholder expectations
- lifecycle tests should stop normalizing passthrough as a normal runtime option

## Non-Goals

- Replacing passthrough with a real Rust VT engine now
- Removing all internal seams needed for testing/development
- Designing the future Rust VT engine implementation

## Recommended Implementation Order

1. Add build-status constant and build-time Cargo warnings.
2. Update CLI help/about and user-visible error text.
3. Add runtime guardrails for no-Ghostty builds.
4. Add structured status fields for inspect/list/JSON where appropriate.
5. Update README/docs/install messaging.
6. Update tests to lock in the new semantics.

## Rationale

The current failure mode is mostly social and semantic: users and agents see a selectable `passthrough` engine and infer it is a lightweight but real option. The repo should instead teach that no-Ghostty builds are incomplete, and that passthrough exists only as a placeholder seam. That framing solves today's confusion while leaving room for a future functional Rust VT engine.

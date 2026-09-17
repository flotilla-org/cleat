# Retain image generations independently of their delivery medium

Status: accepted, 2026-09-17. Implements the direction agreed for [#206](https://github.com/flotilla-org/cleat/issues/206), under the descriptor/payload constraints of [#102](https://github.com/flotilla-org/cleat/issues/102).

Local consumers should not pay for socket pixel transfer and base64 re-encoding when they can share a file. Cleat retains an immutable image generation and chooses a delivery representation for each consumer. Render updates contain descriptors and placements; asset delivery precedes the update that references it.

## Current implementation

Live image capture runs in the session actor, in the same command as render capture. No VT mutation can replace a generation between its descriptor and data acquisition. The captured representation is a daemon-owned regular file, mapped read-only on Unix, with owned bytes as a fallback if file creation fails. A weak cache shares captured generations across viewers without keeping dead generations alive. Ghostty still owns and eagerly decodes its input; materializing the file is one additional payload write per captured generation. This is not a zero-copy ingress path.

Protocol version 8 offers a file for each generation absent from an attachment's committed view. The receiving provider attempts a hard link to a private name and opens/maps it before acknowledging acquisition. Both peers then have independent names for the same immutable content. Each name is removed when its owning reference is dropped. If acquisition fails, ordered 64 KiB byte chunks supply that generation instead. This tests actual filesystem access rather than inferring locality from a socket name. File acquisition is an internal provider operation and does not change the public C ABI.

The sender holds one transfer per channel. Receivers retain the current view and the incoming view, each with a 320 MiB image budget. Render updates commit the next resource set, so a completed update cannot expose partially received pixels. Unchanged generations are reused; leaving and later re-entering a view triggers delivery again if needed. Existing history capture owns exact bytes and enters the same output delivery path. Durable image recording remains the work described in ADR 0003.

CLI attach uses its acquired file for standard Kitty `t=f` uploads. It retains the file while an upload is pending, handles that terminal's reply locally, and falls back to direct upload if the file upload fails. Terminal residency is separate from daemon/provider residency. Producer replies continue to come from the daemon's VT engine. Resolved image placements, including placeholder fragments, are clipped to the attachment content viewport; placeholder glyphs themselves are suppressed by CLI rendering.

File names use per-process UUIDs and private permissions. Ordinary error, replacement and disconnect paths release their references. Abrupt process death can leave names behind; crash reclamation needs a separate owner-liveness policy rather than deleting files merely because they are old. A TTL must not permit mutation of backing still read by another process.

## Later optimizations

Ghostty can expose retained encoded data or owned external backing, with decoding deferred until a consumer needs pixels. Avoiding decoding and avoiding copies are separate changes. The backing abstraction should admit that representation without changing image identity or placement semantics.

A negotiated Kitty extension could permit immutable producer-owned shared memory, producer-managed deletion, and an explicit acquisition deadline or renewable lease. Standard receiver-unlinked POSIX shm cannot be offered by the same name to several ordinary terminals. Define name lifetime separately from backing immutability and buffer reuse. This is the possible zero-extra-intermediary-copy path from Katzensteg through cleat to multiple viewers; it is not implemented here.

Remote Jackstay streaming is deferred. Keep source identity independent of placement geometry so one source can serve several crops, while allowing independently encoded crops when useful. Ingress/egress protocol extensions need their own design.

File offers currently trust the same-user daemon connection: path spelling is not an authorization boundary. Acquisition rejects symlinks and non-regular files and validates length, but does not restrict the source to a naming prefix. Any future privilege-separated or untrusted remote peer must negotiate a stronger backing-access boundary before offering local paths.

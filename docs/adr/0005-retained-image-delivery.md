# Retain image generations independently of their delivery medium

Status: accepted, 2026-09-17. Implements the direction agreed for [#206](https://github.com/flotilla-org/cleat/issues/206), under the descriptor/payload constraints of [#102](https://github.com/flotilla-org/cleat/issues/102).

Local consumers should not pay for socket pixel transfer and base64 re-encoding when they can share a file. Cleat retains an immutable image generation and chooses a delivery representation for each consumer. Render updates contain descriptors and placements; asset delivery precedes the update that references it.

## Current implementation

Live image capture runs in the session actor, in the same command as render capture. No VT mutation can replace a generation between its descriptor and data acquisition. The captured representation is a daemon-owned regular file, mapped read-only on Unix, with owned bytes as a fallback if file creation fails. A weak cache shares captured generations across viewers without keeping dead generations alive. Ghostty still owns and eagerly decodes its input; materializing the file is one additional payload write per captured generation. This is not a zero-copy ingress path.

Protocol version 8 offers a file for each generation absent from an attachment's committed view. The receiving provider attempts a hard link to a private name and opens/maps it before acknowledging acquisition. Both peers then have independent names for the same immutable content. Each name is removed when its owning reference is dropped. If acquisition fails, ordered 64 KiB byte chunks supply that generation instead. This tests actual filesystem access rather than inferring locality from a socket name. File acquisition remains an internal provider operation; the public C ABI now also exposes client-chosen backing access (amendment for #294, 2026-10-02).

The sender holds one transfer per channel. Receivers retain the current view and the incoming view, each with a 320 MiB image budget. Render updates commit the next resource set, so a completed update cannot expose partially received pixels. Unchanged generations are reused; leaving and later re-entering a view triggers delivery again if needed. Existing history capture owns exact bytes and enters the same output delivery path. Durable image recording remains the work described in ADR 0003.

CLI attach uses its acquired file for standard Kitty `t=f` uploads. It retains the file while an upload is pending, handles that terminal's reply locally, and falls back to direct upload if the file upload fails. Terminal residency is separate from daemon/provider residency. Producer replies continue to come from the daemon's VT engine. Resolved image placements, including placeholder fragments, are clipped to the attachment content viewport; placeholder glyphs themselves are suppressed by CLI rendering.

File names use per-process UUIDs and private permissions. Ordinary error, replacement and disconnect paths release their references. Abrupt process death can leave names behind; crash reclamation needs a separate owner-liveness policy rather than deleting files merely because they are old. A TTL must not permit mutation of backing still read by another process.

## Public backing access (amendment, 2026-10-02)

`cleat_session_image_resource_backing` accepts a committed image ID/generation and a requested `FILE` or `SHM` kind. It reports payload length, pixel dimensions, format and compression unchanged from the committed descriptor. Like other output structs, `size` reports the library's struct size. Names are byte slices, not NUL-terminated. Only daemon sessions expose backing initially; other backends return false with an unsupported error, available through `cleat_session_image_resource_backing_error`.

`FILE` borrows the provider's existing private name without copying pixels or creating another link. An acquisition retains the immutable generation until explicit release or session destruction. The name also remains valid while that generation is in the provider's committed view. Leaving the view and releasing the last acquisition removes the provider name. A byte-delivered generation has no file name: requesting `FILE` returns false; callers can request `SHM` or use `cleat_session_with_image_resource_data`.

On Unix, each `SHM` request creates a fresh, privately permissioned POSIX shared memory object and copies the payload once. Ownership of the shm object passes to the caller: the receiving terminal may unlink it for Kitty `t=s`, or the caller must unlink it if it is not handed off. Release frees the library's name storage but never unlinks a successfully returned shm object. Non-Unix platforms return false with an unsupported error. Independent requests never share receiver-unlinked names.

Callers must release each successful result exactly once using its originating session before destroying that session, and must not copy a result and release both copies. Release clears the output. The name slice remains valid until release or session destruction. Neither request nor release changes CLI attach's existing `t=f` retention path. Failure leaves the output unchanged; error text remains borrowed until the next backing request or session destruction.

## Later optimizations

Ghostty can expose retained encoded data or owned external backing, with decoding deferred until a consumer needs pixels. Avoiding decoding and avoiding copies are separate changes. The backing abstraction should admit that representation without changing image identity or placement semantics.

A negotiated Kitty extension could permit immutable producer-owned shared memory, producer-managed deletion, and an explicit acquisition deadline or renewable lease. Standard receiver-unlinked POSIX shm cannot be offered by the same name to several ordinary terminals. Define name lifetime separately from backing immutability and buffer reuse. This is the possible zero-extra-intermediary-copy path from Katzensteg through cleat to multiple viewers; it is not implemented here.

Remote Jackstay streaming is deferred. Keep source identity independent of placement geometry so one source can serve several crops, while allowing independently encoded crops when useful. Ingress/egress protocol extensions need their own design.

File offers currently trust the same-user daemon connection: path spelling is not an authorization boundary. Acquisition rejects symlinks and non-regular files and validates length, but does not restrict the source to a naming prefix. Any future privilege-separated or untrusted remote peer must negotiate a stronger backing-access boundary before offering local paths.

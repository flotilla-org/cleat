# Engine-owned terminal identity

A session's child talks to cleat's VT engine. The terminal that launched the
daemon, and the terminals later attached to it, do not determine the child's
identity. This implements issue [#175](https://github.com/flotilla-org/cleat/issues/175).

| Engine | Preferred TERM | Fallback | Other defaults |
| --- | --- | --- | --- |
| Ghostty | `xterm-ghostty`, when resolvable on the session host | `xterm-256color` | `TERM_PROGRAM=ghostty`, `COLORTERM=truecolor` |
| Passthrough | `dumb` | Same | No program or colour claim |

Ghostty's pinned terminfo entry describes the engine's colour palette, cursor
movement, erase and scrolling operations, alternate screen, styled underlines,
synchronized output, bracketed paste and keyboard enhancements. Cleat uses the
embedded engine for application query replies and keyboard/mouse encoding.
`TERM_PROGRAM=ghostty` identifies this compatibility profile even when TERM must
fall back; this also keeps consumers' zero-based pixel-mouse interpretation
consistent with the Ghostty encoder. It does not claim that the child runs in
the Ghostty GUI or has access to its shell integration.

Passthrough is a byte-relay/test placeholder without a functional VT engine.
It cannot promise an xterm screen model or answer capability queries, so it
advertises `dumb` rather than borrowing the launcher's capabilities.

## Availability and overrides

Before spawning a Ghostty session on Unix, cleat runs `infocmp -x xterm-ghostty`
with the child's inherited environment, session overrides and working directory.
This delegates terminfo lookup to the host's ncurses tools, including `TERMINFO`,
`TERMINFO_DIRS` and the user's database. The check runs before fork and has a
200 ms wait limit. An absent utility, failed lookup or timeout selects the
portable fallback. Native Windows selects that fallback because it has no
standard host terminfo lookup utility. Cleat does not install terminfo entries.

An explicit `--env TERM=...` wins and skips the lookup. The caller owns the
availability and compatibility of that entry. Individual overrides for
`TERM_PROGRAM`, `TERM_PROGRAM_VERSION` and `COLORTERM` also win. Otherwise the
launcher's values for these four identity variables are removed. Cleat does not
invent a Ghostty version or inherit the outer terminal application's version.
The remaining environment policy is unchanged.

For example:

```sh
cleat launch demo --env TERM=xterm-256color
```

Selection happens on each child launch, including recreation, using the engine
and overrides stored in session metadata. Attaching another viewer does not
change the environment of a running child. A login shell can subsequently
change environment variables through its own startup files.

`xterm-256color` remains a widely available fallback, not a promise that every
remote host has Ghostty's terminfo. An `ssh` process inside the session can forward
TERM to a second host that lacks the entry; install the entry there or override
TERM for that SSH command. This is distinct from attaching to a remote cleat
daemon, where selection happens on that daemon's host.

## Verification

Contract tests cover preferred/fallback selection, the passthrough profile,
explicit overrides and removal of inherited identity. A host `tput` probe checks
256 colours and a cursor-position sequence for the selected entry. The daemon
launch test checks the actual child's TERM, a terminfo lookup and an explicit
TERM override. Unix and Windows launch paths use the same identity policy;
Windows keeps its existing case-insensitive environment-name matching.

Live testing: launch a fresh session, inspect `TERM`, `TERM_PROGRAM` and
`COLORTERM`, then try Katzensteg's mouse targeting and a terminfo-based TUI.
Existing sessions retain their original environment.

Sources: [pinned Ghostty terminfo](https://github.com/rjwittams/ghostty/blob/c3dbb925e6cbcfceafba5749f81a486dd2275099/src/terminfo/ghostty.zig),
[Ghostty's terminfo guidance](https://ghostty.org/docs/help/terminfo), and
[ncurses infocmp lookup](https://invisible-island.net/ncurses/man/infocmp.1m.html).

# Ship the ConPTY package beside Windows executables

Status: accepted, 2026-09-24. Records the resolution of [#233](https://github.com/flotilla-org/cleat/issues/233); implemented by [#234](https://github.com/flotilla-org/cleat/issues/234).

The Windows inbox ConPTY (kernel32 `CreatePseudoConsole`, observed on build 26200) silently drops Kitty graphics APC and sixel DCS before Cleat's VT engine sees them. The `Microsoft.Windows.Console.ConPTY` NuGet package's `conpty.dll` and `OpenConsole.exe` pass APC, DCS and OSC through unchanged and in order ([Katzensteg probe](https://github.com/rjwittams/katzensteg/blob/435b1257c95dcc7f1d45823bd12178b3face4a2b/docs/research/windows-2026-09-24.md)). Windows Terminal and WezTerm ship the same package for the same reason.

## Decision

1. **Binding.** Cleat loads `conpty.dll` at runtime with `LoadLibraryExW` from its own executable's directory only (an absolute path with `LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32`, never a general DLL search) and resolves `ConptyCreatePseudoConsole`, `ConptyResizePseudoConsole` and `ConptyClosePseudoConsole`. It does not link `conpty.lib` at build time. The inbox kernel32 ConPTY is the fallback.
2. **Location.** `conpty.dll` and `OpenConsole.exe` sit beside every executable that creates ConPTY sessions: `cleat.exe`, and `wheelhouse.exe` for in-process panes. Each build copies them; there is no shared per-user install.
3. **Acquisition.** The package is pinned in one place, [`tools/conpty.toml`](../../tools/conpty.toml) (version 1.24.260710001 and the `.nupkg` SHA-256). [`tools/prepare-conpty.ps1`](../../tools/prepare-conpty.ps1) fetches it from nuget.org and verifies it, as the Ghostty helpers do for Ghostty; `prepare-ghostty-vt.ps1` runs it too. The package is MIT (microsoft/terminal); its licence ships beside the binaries as `conpty-LICENSE.txt`. Updates are deliberate version bumps that re-run the pass-through regression.
4. **Fallback.** Inbox ConPTY still runs sessions when the bundle is absent, but it is never silent: session info (`list --json`, and the human listing when degraded), `inspect` and the daemon log report which ConPTY a session uses and that graphics pass-through is degraded, with the reason.
5. **Sessions.** The ConPTY is chosen when a session's program starts and recorded on that session's PTY for its lifetime. Existing sessions are disposable, so there is no migration or compatibility work.
6. **Startup queries.** The bundled ConPTY opens its output with `CSI 1 t` (window visible) and a DA1 query (`CSI c`), then holds the program's console connection until DA1 is answered or 3 s pass. Cleat treats these as questions to itself as the pseudoconsole host: it consumes them before clients, recordings and the screen, and answers DA1 from its VT engine, identically whether or not a client is attached. An engine without a DA1 answer (the no-VT build) gets the fixed minimal reply `CSI ? 62 ; 22 c`. `CSI 1 t` needs no reply. Sessions start without delay with no client attached.

## Consequences

- `conpty.dll` finds `OpenConsole.exe` beside itself and, if it is missing, silently falls back to the system `conhost.exe`, which drops graphics like the inbox ConPTY. Cleat therefore requires both files before choosing the bundle.
- `CreatePseudoConsole` flags stay 0, as in the probes. `PSEUDOCONSOLE_INHERIT_CURSOR` would add a cursor-position query to the handshake, and the glyph-width flags do not affect APC/DCS pass-through.
- `CLEAT_CONPTY=inbox` in the daemon's environment forces the inbox ConPTY for diagnostics and for the regression's failing run; the session reports the override as its fallback reason.
- A build without the prepared package still succeeds with a warning and removes any staged copies, so sessions report the fallback rather than running an unpinned bundle.
- Raw-stream clients never see the startup handshake, so their terminal cannot answer DA1 a second time. OpenConsole's input parser consumes only the first DA1 reply (source inference from microsoft/terminal `InputStateMachineEngine`); a later one would not be swallowed as a host reply.
- Embedders other than Cleat's own build (Wheelhouse) must copy the same pinned files beside their executable.

"""Temporary #289 runner experiment; removed after collecting evidence."""

from pathlib import Path
import subprocess
import time


def run(args, *, expected_failure=False):
    started = time.monotonic()
    result = subprocess.run(args, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
    print(result.stdout, flush=True)
    print(f"[startup-289] elapsed={time.monotonic() - started:.3f}s exit={result.returncode}", flush=True)
    if expected_failure:
        assert result.returncode != 0, "old listener unexpectedly passed"
        assert "timed out waiting for named pipe" in result.stdout, "different failure"
        assert "os error 121" in result.stdout, "different Win32 error"
    else:
        assert result.returncode == 0, "validation failed; no retry"


ipc = Path("crates/cleat/src/platform/ipc/windows.rs")
daemon = Path("crates/cleat/src/session.rs")
generations = Path("crates/cleat/tests/generations.rs")
originals = {path: path.read_text() for path in (ipc, daemon, generations)}
test = ["cargo", "test", "-p", "cleat", "--locked", "--features", "ghostty-vt", "--test", "generations", "--", "--nocapture"]
try:
    # Open the original race window without changing any startup budget.
    anchor = "    let mut draining = false;"
    assert originals[daemon].count(anchor) == 1
    daemon.write_text(originals[daemon].replace(anchor, "    thread::sleep(Duration::from_millis(100));\n" + anchor))
    anchor = "    service.create(Some(id.into()), Some(VtEngineKind::Passthrough), None, Some(command.into()), true).unwrap();"
    assert originals[generations].count(anchor) == 1
    generations.write_text(originals[generations].replace(anchor, '    let started = std::time::Instant::now();\n' + anchor + '\n    eprintln!("[startup-289] launch {id}: {:?}", started.elapsed());'))
    # Restore the old synchronous ERROR_NO_DATA behavior, retaining the rest
    # of current main so this is a single-variable comparison.
    anchor = "ERROR_NO_DATA => return self.disconnect_closed_client(),"
    assert originals[ipc].count(anchor) == 1
    ipc.write_text(originals[ipc].replace(anchor, "ERROR_NO_DATA => return Ok(false),"))
    print("[startup-289] OLD listener, delayed first accept: expect exact reported failure", flush=True)
    run(["cargo", "build", "-p", "cleat", "--locked", "--features", "ghostty-vt"])
    run(test, expected_failure=True)
    ipc.write_text(originals[ipc])
    print("[startup-289] FIXED listener, same delayed first accept: require pass", flush=True)
    run(["cargo", "build", "-p", "cleat", "--locked", "--features", "ghostty-vt"])
    run(test)
    daemon.write_text(originals[daemon])
    run(["cargo", "build", "-p", "cleat", "--locked", "--features", "ghostty-vt"])
    for iteration in range(1, 21):
        print(f"[startup-289] normal generations run {iteration}/20", flush=True)
        run(test)
finally:
    for path, contents in originals.items():
        path.write_text(contents)

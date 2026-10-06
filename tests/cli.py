#!/usr/bin/env python3
"""Check the real CLI's paths which exit before any hardware/service access."""
import pathlib
import subprocess
import sys
import tempfile

exe = pathlib.Path(sys.argv[1]).resolve()
checks = 0


def check(args, code, part):
    global checks
    result = subprocess.run([str(exe), *args], capture_output=True, text=True, timeout=5)
    assert result.returncode == code, (args, result.returncode, result.stdout, result.stderr)
    assert part in result.stdout + result.stderr, (args, result.stdout, result.stderr)
    checks += 1


check(["--version"], 0, "macpro-fand 2.0.0")
check(["--help"], 0, "--stopped")
check(["--dry-run", "--release"], 2, "select only one")
check(["--config"], 2, "needs a path")
check(["--unknown"], 2, "unknown option")
with tempfile.TemporaryDirectory(prefix="macpro-fand-cli-") as tmp:
    config = pathlib.Path(tmp) / "config"
    for text in [b"K_GPU=True", b"GPU_CAP={'max':0}", b"GPU_CAP={'max':125.5}",
                 b"__import__('os').system('id')", b"SMC={'bad':([],[1],1,1)}", b"\xff\x00"]:
        config.write_bytes(text)
        check(["--check-config", "--config", str(config)], 78, "configuration")
        check(["--dry-run", f"--config={config}"], 78, "configuration")
    import os
    config.unlink()
    os.mkfifo(config)
    check(["--check-config", "--config", str(config)], 78, "regular file")
print(f"CLI: {checks} checks pass")

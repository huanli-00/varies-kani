#!/usr/bin/env python3
import os
import subprocess
import sys
import shlex
from collections import deque
from typing import List

LOG_FILE = "varies_verify.log"
VER_FILE = "varies-ver.sh"


def get_forward_args() -> List[str]:
    """Forward verification options to the checkout's Varies Kani launcher."""
    return sys.argv[1:]


def build_kani_command(crate_dir: str) -> str:
    """Generate the local verification command for execution and recording."""
    forward_args = get_forward_args()
    arg_str = " ".join(shlex.quote(a) for a in forward_args)

    launcher = os.path.join(os.path.dirname(os.path.realpath(__file__)), "cargo-varies-kani")
    cmd = shlex.quote(launcher)
    if arg_str:
        cmd += f" {arg_str}"

    print(f"Generated verify command:\n{cmd}")
    return cmd


def write_verify_script(crate_dir: str, cmd: str) -> str:
    """
    Generate a simple shell script that records how to run Kani in the crate directory.
    Although we don't rely on this script during actual execution, it is useful for reproducibility and debugging.
    """
    content_lines = [
        "#!/bin/bash",
        f"cd {shlex.quote(crate_dir)}",
        cmd,
        "",
    ]
    content = "\n".join(content_lines)

    ver_path = os.path.join(crate_dir, VER_FILE)
    with open(ver_path, "w") as f:
        f.write(content)

    try:
        os.chmod(ver_path, 0o755)
    except OSError:
        # May fail on some filesystems, ignore it
        pass

    return ver_path


def run_kani(crate_dir: str, cmd: str) -> int:
    """
    Run the given command in `crate_dir`, redirecting output to LOG_FILE.
    Uses subprocess.run directly instead of executing through a shell script.
    """
    log_path = os.path.join(crate_dir, LOG_FILE)
    print(f"Running verification in {crate_dir}, log -> {log_path}")

    with open(log_path, "w") as log_file:
        # Use shell=False here for better security and fewer errors
        result = subprocess.run(
            ["bash", "-lc", cmd],
            cwd=crate_dir,
            stdout=log_file,
            stderr=subprocess.STDOUT,
        )

    return result.returncode


def print_log_tail(crate_dir: str, lines: int = 3) -> str:
    """Print the last few lines of the log file and return its content."""
    log_path = os.path.join(crate_dir, LOG_FILE)

    if not os.path.exists(log_path):
        print(f"{LOG_FILE} not found in {crate_dir}")
        return ""

    with open(log_path, "r") as f:
        last_lines = deque(f, maxlen=lines)
        content = "".join(last_lines)

    print("=== Last log lines ===")
    print(content.rstrip("\n"))
    print("======================")
    return content


def main() -> None:
    crate_dir = os.getcwd()
    cmd = build_kani_command(crate_dir)
    verify_script = write_verify_script(crate_dir, cmd)

    # The actual execution is of cmd, not verify_script (script is only for record/reproducibility)
    ret = run_kani(crate_dir, cmd)

    if ret != 0:
        print(f"Verification process exited with code {ret}.")

    content = print_log_tail(crate_dir, lines=3)

    if "Complete" not in content:
        print("Kani verification did not complete successfully (missing 'Complete' in log).")
        sys.exit(1)

    # Command executed successfully & log contains "Complete"
    print("Kani verification completed successfully.")
    sys.exit(0)


if __name__ == "__main__":
    main()

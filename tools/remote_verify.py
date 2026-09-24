#!/usr/bin/env python3
"""Run heavy verification of this repository on the Netcup host via Docker.

The local machine only packages the worktree and streams output; all
compilation and test execution happens inside an ephemeral Rust container
on the server. This keeps heavy runtime off the Windows workstation and
leaves no host-level toolchain install behind.

Usage::

    python tools/remote_verify.py --image rust:1.94 -- cargo test
    python tools/remote_verify.py --image rust:1.77 -- cargo check --all-targets
    python tools/remote_verify.py --dir integrations/datafusion -- cargo test

Requirements:
    * OpenSSH client with key-based access to the default host
    * ``sudo -n docker`` on the host (passwordless sudo, no host installs)
    * Network access from the host to pull the requested image

The command runs with the repository root (or ``--dir``) as the working
directory inside the container. A JSON receipt is written to
``target/remote_verify_receipt.json`` unless ``--receipt`` is given.
"""

from __future__ import annotations

import argparse
import base64
import json
import shlex
import shutil
import subprocess
import sys
import tarfile
import tempfile
import time
import uuid
from pathlib import Path

DEFAULT_HOST = "minh@159.195.82.241"
DEFAULT_IMAGE = "rust:1.94"
DEFAULT_TIMEOUT_S = 3600

EXCLUDED_DIRS = {
    ".git",
    ".pytest_cache",
    ".ruff_cache",
    ".worktrees",
    "__pycache__",
    "target",
    ".hf8268-audit",
    "second-ecosystem-audit",
    "release-artifacts-v0.2.6",
    "node_modules",
}


def build_tarball(repo_root: Path, out_path: Path) -> int:
    """Package the working tree (honoring excluded dirs) as tar.gz."""
    count = 0
    with tarfile.open(out_path, "w:gz") as archive:
        for path in sorted(repo_root.rglob("*")):
            relative = path.relative_to(repo_root)
            if any(part in EXCLUDED_DIRS for part in relative.parts):
                continue
            if path.is_dir():
                continue
            archive.add(path, arcname=str(relative))
            count += 1
    return count


def require_tool(name: str) -> str:
    resolved = shutil.which(name)
    if resolved is None:
        raise SystemExit(f"required tool not found on PATH: {name}")
    return resolved


def run(
    command: list[str],
    *,
    timeout: int,
    label: str,
) -> subprocess.CompletedProcess[bytes]:
    started = time.perf_counter()
    completed = subprocess.run(command, capture_output=True, timeout=timeout)
    elapsed = time.perf_counter() - started
    print(
        f"[remote-verify] {label}: rc={completed.returncode} in {elapsed:.1f}s",
        file=sys.stderr,
    )
    return completed


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--host", default=DEFAULT_HOST)
    parser.add_argument("--image", default=DEFAULT_IMAGE)
    parser.add_argument(
        "--repo",
        type=Path,
        default=Path(__file__).resolve().parents[1],
    )
    parser.add_argument(
        "--dir",
        default=".",
        help="working directory inside the repository for the command",
    )
    parser.add_argument("--timeout", type=int, default=DEFAULT_TIMEOUT_S)
    parser.add_argument(
        "--receipt",
        type=Path,
        default=None,
        help="receipt path (default: target/remote_verify_receipt.json)",
    )
    parser.add_argument(
        "--no-cache-dir",
        action="store_true",
        help="do not mount a persistent cargo registry volume",
    )
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args()

    command = [part for part in args.command if part != "--"]
    if not command:
        parser.error("missing command after --, e.g. -- cargo test")

    repo_root = args.repo.resolve()
    if not (repo_root / "Cargo.toml").is_file() and not (repo_root / args.dir).is_dir():
        raise SystemExit(f"not a cargo repository root: {repo_root}")

    ssh = require_tool("ssh")
    scp = require_tool("scp")

    token = uuid.uuid4().hex[:12]
    remote_dir = f"/tmp/mmap_chunker_verify_{token}"
    workdir = str(args.dir).replace("\\", "/")
    image = args.image

    with tempfile.TemporaryDirectory(prefix="mmap_chunker_remote_verify_") as temp:
        temp_dir = Path(temp)
        archive_path = temp_dir / "repo.tar.gz"
        script_path = temp_dir / "remote_run.sh"

        file_count = build_tarball(repo_root, archive_path)
        archive_mb = archive_path.stat().st_size / (1024 * 1024)
        print(
            f"[remote-verify] packaged {file_count} files "
            f"({archive_mb:.1f} MiB) for {args.host}",
            file=sys.stderr,
        )

        registry_mount = ""
        if not args.no_cache_dir:
            registry_mount = '-v "$HOME/.cache/mmap_chunker_cargo_registry:/usr/local/cargo/registry"'

        if len(command) >= 3 and command[0] in ("sh", "bash") and command[1] == "-c":
            container_script = command[2]
        else:
            container_script = " ".join(shlex.quote(part) for part in command)
        script_b64 = base64.b64encode(container_script.encode("utf-8")).decode("ascii")

        script = f"""#!/usr/bin/env bash
set -euo pipefail
REMOTE_DIR={remote_dir}
trap 'sudo -n rm -rf "$REMOTE_DIR" >/dev/null 2>&1 || true' EXIT
mkdir -p "$REMOTE_DIR/repo"
tar xzf "$REMOTE_DIR/repo.tar.gz" -C "$REMOTE_DIR/repo"
echo {script_b64} | base64 -d > "$REMOTE_DIR/command.sh"
cd "$REMOTE_DIR/repo/{workdir}"
echo "[remote-verify] host=$(hostname) image={image} cwd=$(pwd)"
set +e
timeout {args.timeout} sudo -n docker run --rm \
  -v "$REMOTE_DIR/repo:/work" \
  -v "$REMOTE_DIR/command.sh:/tmp/remote-command.sh:ro" \
  -w "/work/{workdir}" \
  -e CARGO_TERM_COLOR=never \
  -e CARGO_NET_RETRY=5 \
  {registry_mount} \
  {image} bash /tmp/remote-command.sh
RC=$?
exit $RC
"""
        script_path.write_text(script, encoding="utf-8", newline="\n")

        run(
            [
                ssh,
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=10",
                args.host,
                f"mkdir -p {remote_dir}",
            ],
            timeout=60,
            label="prepare",
        )
        run(
            [
                scp,
                "-q",
                "-o",
                "BatchMode=yes",
                str(archive_path),
                str(script_path),
                f"{args.host}:{remote_dir}/",
            ],
            timeout=600,
            label="upload",
        )

        started = time.perf_counter()
        completed = subprocess.run(
            [
                ssh,
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=10",
                args.host,
                f"bash {remote_dir}/remote_run.sh",
            ],
            capture_output=True,
            timeout=args.timeout + 300,
        )
        elapsed = time.perf_counter() - started

    stdout = completed.stdout.decode("utf-8", errors="replace")
    stderr = completed.stderr.decode("utf-8", errors="replace")
    if stdout:
        print(stdout, end="" if stdout.endswith("\n") else "\n")
    if stderr.strip():
        print(stderr, end="" if stderr.endswith("\n") else "\n", file=sys.stderr)

    receipt_path = args.receipt or (repo_root / "target" / "remote_verify_receipt.json")
    receipt = {
        "host": args.host,
        "image": image,
        "workdir": workdir,
        "command": command,
        "exit_code": completed.returncode,
        "elapsed_s": round(elapsed, 1),
        "files": file_count,
        "archive_mib": round(archive_mb, 1),
        "stdout_tail": stdout[-4000:],
        "stderr_tail": stderr[-2000:],
    }
    receipt_path.parent.mkdir(parents=True, exist_ok=True)
    receipt_path.write_text(json.dumps(receipt, indent=2), encoding="utf-8")
    print(f"[remote-verify] receipt: {receipt_path}", file=sys.stderr)
    return completed.returncode


if __name__ == "__main__":
    raise SystemExit(main())

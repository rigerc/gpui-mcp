#!/usr/bin/env python3
"""Forward-port gpui-pre's three independent patches with Git three-way merges.

This is a preparation tool, not an automatic compatibility guarantee: upstream
changes can conflict and require manual Rust edits, followed by Cargo tests.
It deliberately never changes vendor/ or workspace version requirements.

Examples:
  python3 scripts/port-gpui-pre.py 0.3.12 --archive gpui-pre-0.3.12.crate --work /tmp/gpui-port
  # On conflict, resolve files in /tmp/gpui-port/repo, then git add and
  # git cherry-pick --continue; cherry-pick remaining SHAs printed by the tool.
  python3 scripts/port-gpui-pre.py 0.3.12 --work /tmp/gpui-port --finalize
"""

from __future__ import annotations

import argparse
import hashlib
import io
import json
from pathlib import Path, PurePosixPath
import shutil
import subprocess
import sys
import tarfile
import tomllib
from urllib.request import urlopen

ROOT = Path(__file__).resolve().parents[1]
PARTS = ("automation", "font-fallback", "grid")
MAX_ARCHIVE = 32 * 1024 * 1024


def git(repo: Path, *args: str) -> str:
    result = subprocess.run(
        ["git", *args], cwd=repo, text=True, capture_output=True, check=False
    )
    if result.returncode:
        raise RuntimeError(
            f"git {' '.join(args)} failed:\n{result.stdout}{result.stderr}"
        )
    return result.stdout.strip()


def reset_work_tree(repo: Path) -> None:
    # Keep .git, but remove tracked/untracked material before installing a crate.
    for path in repo.iterdir():
        if path.name == ".git":
            continue
        if path.is_dir() and not path.is_symlink():
            shutil.rmtree(path)
        else:
            path.unlink()


def commit(repo: Path, message: str) -> str:
    git(repo, "add", "--all")
    git(repo, "commit", "--quiet", "-m", message)
    return git(repo, "rev-parse", "HEAD")


def release_archive(version: str, archive: Path | None) -> bytes:
    if archive is not None:
        data = archive.read_bytes()
    else:
        url = f"https://static.crates.io/crates/gpui-pre/gpui-pre-{version}.crate"
        with urlopen(url, timeout=30) as response:  # noqa: S310 - fixed host
            data = response.read(MAX_ARCHIVE + 1)
    if not (0 < len(data) <= MAX_ARCHIVE):
        raise ValueError("crate archive is empty or exceeds 32 MiB")
    return data


def unpack_crate(data: bytes, version: str, destination: Path) -> str:
    prefix = f"gpui-pre-{version}/"
    with tarfile.open(fileobj=io.BytesIO(data), mode="r:gz") as archive:
        for member in archive:
            if member.isdir():
                continue
            path = PurePosixPath(member.name)
            if (
                not member.isfile()
                or not member.name.startswith(prefix)
                or path.is_absolute()
                or ".." in path.parts
            ):
                raise ValueError(f"unsafe crate archive entry {member.name!r}")
            relative = PurePosixPath(member.name[len(prefix) :])
            if not relative.parts or relative.name == "Cargo.lock":
                continue
            file = archive.extractfile(member)
            if file is None:
                raise ValueError(f"unreadable crate entry: {member.name}")
            content = file.read()
            if relative.parts[0] == "src" and relative.suffix == ".rs":
                content = content.replace(b"\r\n", b"\n")
            output = destination.joinpath(*relative.parts)
            output.parent.mkdir(parents=True, exist_ok=True)
            output.write_bytes(content)
    manifest_path = destination / "Cargo.toml"
    manifest = tomllib.loads(manifest_path.read_text())
    if manifest["package"]["version"] != version:
        raise ValueError("crate version does not match requested version")
    return manifest["package"]["metadata"]["gpui-pre"]["zed-rev"]


def begin(args: argparse.Namespace) -> None:
    if args.work.exists():
        raise ValueError("work directory already exists; use a fresh --work directory")
    args.work.mkdir(parents=True)
    repo = args.work / "repo"
    shutil.copytree(ROOT / "vendor/gpui-pre", repo)
    patches = ROOT / "vendor/patches/gpui-pre" / args.base
    # Reconstruct the *published* source from the existing reproducible patched
    # snapshot; this is the actual base of the original patch series.
    for part in reversed(PARTS):
        git(repo, "apply", "--reverse", str(patches / f"{part}.patch"))
    git(repo, "init", "--quiet")
    git(repo, "config", "user.name", "GPUI patch port")
    git(repo, "config", "user.email", "gpui-port@localhost")
    baseline = commit(repo, "Unpatched GPUI baseline")
    patch_commits = []
    for part in PARTS:
        git(repo, "apply", str(patches / f"{part}.patch"))
        patch_commits.append(commit(repo, f"Port {part}"))

    data = release_archive(args.version, args.archive)
    sha256 = hashlib.sha256(data).hexdigest()
    git(repo, "checkout", "--quiet", "-b", "new-upstream", baseline)
    reset_work_tree(repo)
    rev = unpack_crate(data, args.version, repo)
    upstream = commit(repo, f"Unpatched gpui-pre {args.version}")
    state = {
        "version": args.version,
        "sha256": sha256,
        "zed_rev": rev,
        "upstream": upstream,
        "patch_commits": patch_commits,
    }
    (args.work / "state.json").write_text(json.dumps(state, indent=2) + "\n")
    for index, sha in enumerate(patch_commits):
        try:
            git(repo, "cherry-pick", "--no-edit", sha)
        except RuntimeError as exc:
            print(f"Conflict porting {PARTS[index]}: {exc}", file=sys.stderr)
            print("Resolve conflicts inside", repo, file=sys.stderr)
            print("Then run: git add -A && git cherry-pick --continue", file=sys.stderr)
            for pending in patch_commits[index + 1 :]:
                print("Then run: git cherry-pick", pending, file=sys.stderr)
            print("Finally use --finalize with the same --work", file=sys.stderr)
            raise SystemExit(2) from None
    finalize(args)


def finalize(args: argparse.Namespace) -> None:
    repo = args.work / "repo"
    state = json.loads((args.work / "state.json").read_text())
    if state["version"] != args.version:
        raise ValueError("work directory was created for a different version")
    if (repo / ".git/CHERRY_PICK_HEAD").exists():
        raise ValueError("finish the conflicting cherry-pick before finalizing")
    commits = git(repo, "rev-list", "--reverse", f'{state["upstream"]}..HEAD').splitlines()
    if len(commits) != len(PARTS):
        raise ValueError(f"expected three patch commits, got {len(commits)}")
    output = args.work / "patches"
    output.mkdir(exist_ok=True)
    for part, sha in zip(PARTS, commits, strict=True):
        patch = git(repo, "show", "--format=", "--no-renames", sha)
        if not patch.startswith("diff --git "):
            raise ValueError(f"empty or invalid {part} patch")
        (output / f"{part}.patch").write_text(patch + "\n")
    print("Ported patches:", output)
    print("Update vendor/gpui-pre.json with:")
    print(json.dumps({args.version: {
        "sha256": state["sha256"], "zed_rev": state["zed_rev"]
    }}, indent=2))
    print("Inspect each patch, adjust API changes, then run `cargo xtask vendor`. ")
    print("Do not change the supported version range before compilation passes.")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("version", help="target published gpui-pre version")
    parser.add_argument("--base", default="0.3.9", help="existing vendored patch series")
    parser.add_argument("--archive", type=Path, help="offline .crate archive")
    parser.add_argument("--work", required=True, type=Path, help="new persistent work directory")
    parser.add_argument("--finalize", action="store_true", help="collect manually resolved patches")
    args = parser.parse_args()
    try:
        if args.finalize:
            finalize(args)
        else:
            begin(args)
    except (ValueError, OSError, subprocess.SubprocessError, RuntimeError, KeyError) as exc:
        parser.exit(1, f"error: {exc}\n")


if __name__ == "__main__":
    main()

#!/usr/bin/env python3
"""Rule 62: a repo-root Docker build context must be small and readable.

The `server` arm of both bench harnesses builds with the repository root as its
build context (`bench/parity/Dockerfile.ocserve` does `COPY . /src`). On
2026-10-27 the snapshot harness failed every night for ten days with:

    #38 ERROR: error from sender: open .../bench/snapshot-it/data:
        permission denied

because a scratch directory left behind by an earlier containerised run is
root-owned (the containers run as root into bind mounts). Two separate defects
had to be true at once:

  1. runtime scratch state (`work/`, `data/`, `freeze-work/`, `.runs/`) sat
     inside a build context, and
  2. nothing checked that the context was even readable, so the failure
     surfaced ten days later as an opaque build error inside a nightly step.

The context was also 3.57 GB per build, which is slow and pointless: nothing
under `bench/`, `fuzz/`, or `.safety/` is compiled into the binary (guard rule
30 keeps every `include_str!`/`include_bytes!` inside its crate root).

This checker reproduces BuildKit's walk: apply `.dockerignore`, then assert
every surviving path is readable by this user and the total stays bounded. It
is deliberately independent of docker, so it runs in CI on hosts without it.

--selftest plants both halves: an unreadable directory and a bloated context.
"""

import fnmatch
import os
import stat
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

# Context size ceiling. The source tree outside target/.git/bench/fuzz/.safety
# is ~42 MB; 512 MiB leaves room for a few large committed fixtures while still
# failing loudly on anything that sends gigabytes.
MAX_CONTEXT_BYTES = 512 * 1024 * 1024


def load_ignore(root: Path) -> list[str]:
    p = root / ".dockerignore"
    if not p.exists():
        return []
    pats = []
    for line in p.read_text().splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        pats.append(line)
    return pats


def _matches(path_rel: str, patterns: list[str]) -> bool:
    """A small but honest subset of .dockerignore semantics.

    BuildKit patterns are relative to the context root; a pattern with a slash
    is matched against the path, a bare name against any component. `**/x`
    matches at any depth. Negations (`!`) re-include.
    """
    ignored = False
    parts = path_rel.split("/")
    for pat in patterns:
        neg = pat.startswith("!")
        body = pat[1:] if neg else pat
        body = body.rstrip("/")
        if body in ("", "."):
            continue
        hit = False
        if "/" in body or body.startswith("**"):
            # Path-shaped: match the path itself, the path with `**/` stripped,
            # or any ancestor (BuildKit prunes a directory and its subtree).
            hit = (
                fnmatch.fnmatch(path_rel, body)
                or fnmatch.fnmatch(path_rel, body.replace("**/", ""))
                or any(
                    fnmatch.fnmatch("/".join(parts[:i]), body)
                    for i in range(1, len(parts))
                )
                or fnmatch.fnmatch(path_rel, body.replace("**", "*"))
            )
        else:
            hit = any(fnmatch.fnmatch(part, body) for part in parts)
        if hit:
            ignored = not neg
    return ignored


def walk_context(root: Path, patterns: list[str], errors: list[str] | None = None):
    """Yield (path, size) for every path BuildKit would send.

    `errors` collects directories that cannot be LISTED. `os.walk` swallows
    those by default (it simply yields nothing), which is exactly how a
    root-owned scratch dir turns into "context sent successfully, 3 bytes".
    """

    def onerror(e: OSError):
        if errors is not None:
            errors.append(f"{e.filename}: {e.strerror}")

    for dirpath, dirnames, filenames in os.walk(
        root, followlinks=False, onerror=onerror
    ):
        rel_dir = os.path.relpath(dirpath, root)
        rel_dir = "" if rel_dir == "." else rel_dir.replace(os.sep, "/")
        # Prune ignored directories: BuildKit does not descend into them.
        keep = []
        for d in dirnames:
            rel = f"{rel_dir}/{d}" if rel_dir else d
            if not _matches(rel, patterns):
                keep.append(d)
        dirnames[:] = keep
        for f in filenames:
            rel = f"{rel_dir}/{f}" if rel_dir else f
            if _matches(rel, patterns):
                continue
            p = Path(dirpath) / f
            try:
                size = p.lstat().st_size
            except OSError:
                size = 0
            yield p, size


def check(root: Path) -> list[str]:
    problems: list[str] = []
    if not (root / ".dockerignore").exists():
        problems.append(".dockerignore is missing (the root is a build context)")

    # Roots that are used as a Docker build context by any harness.
    contexts = []
    for compose in (root / "bench").rglob("compose.yaml"):
        text = compose.read_text()
        if "context: ../.." in text:
            contexts.append(compose)
    if not contexts:
        problems.append(
            "no harness compose file found with a repo-root context "
            "(if that changed, this check needs updating, not deleting)"
        )

    patterns = load_ignore(root)
    total = 0
    unreadable: list[str] = []
    walk_errors: list[str] = []
    for p, size in walk_context(root, patterns, walk_errors):
        total += size
        rel = str(p.relative_to(root))
        try:
            if p.is_dir():
                os.listdir(p)
            else:
                with open(p, "rb") as fh:
                    fh.read(1)
        except PermissionError:
            unreadable.append(rel)
        except OSError:
            pass

    unreadable.extend(walk_errors)
    if unreadable:
        problems.append(
            f"{len(unreadable)} unreadable path(s) in the build context, e.g. "
            f"{unreadable[0]} (root-owned scratch state; BuildKit fails with "
            "'error from sender')"
        )
    if total > MAX_CONTEXT_BYTES:
        problems.append(
            f"build context is {total / (1024 * 1024):.0f} MiB, over the "
            f"{MAX_CONTEXT_BYTES // (1024 * 1024)} MiB ceiling"
        )

    # In-tree scratch must not exist at all. The containers run as root, so an
    # in-tree scratch dir is root-owned and unreadable afterwards: that broke
    # the build context AND every tool that walks the tree (`cargo mutants`
    # failed with EACCES on bench/snapshot-it/data). Run.sh now puts scratch
    # under the system temp dir; a reappearance means the harness regressed.
    for stale in ("bench/snapshot-it/data", "bench/snapshot-it/work",
                  "bench/snapshot-it/freeze-work"):
        p = root / stale
        if p.exists():
            problems.append(
                f"{stale} exists in the tree (root-owned container scratch; "
                "run.sh must keep it under TMPDIR)"
            )
        if p.exists() and p.stat().st_uid == 0:
            problems.append(f"{stale} is root-owned (unreadable to the build and to test tools)")

    # Runtime scratch dirs must be excluded by NAME, not merely ignored by git:
    # a gitignored dir is still sent to BuildKit.
    must_exclude = [
        "bench/snapshot-it/data",
        "bench/snapshot-it/work",
        "bench/snapshot-it/freeze-work",
        "bench/parity/.runs/probe/upstream",
    ]
    for rel in must_exclude:
        if not _matches(rel, patterns):
            problems.append(f"{rel} is not excluded from the build context")
    return problems


def selftest() -> int:
    import tempfile

    failures: list[str] = []
    real = check(ROOT)
    if real:
        failures.extend(f"real tree: {p}" for p in real)

    base = Path(tempfile.mkdtemp(prefix="ocserve-ctx-"))
    try:
        (base / ".dockerignore").write_text("bench/\n")
        (base / "bench").mkdir()
        (base / "src").mkdir()
        (base / "src" / "main.rs").write_text("fn main() {}\n")
        (base / "bench" / "compose.yaml").write_text(
            "services:\n  x:\n    build:\n      context: ../..\n"
        )
        if check(base):
            failures.append("clean fixture reported problems")

        # Plant 1: unreadable runtime state inside the context.
        locked = base / "scratch"
        locked.mkdir()
        (locked / "x").write_text("x")
        locked.chmod(0)
        (base / "bench" / ".keep").write_text("")
        # move it inside the context by removing the bench/ ignore
        (base / ".dockerignore").write_text("")
        problems = check(base)
        caught = any("unreadable" in p for p in problems)
        if not caught:
            failures.append("plant 1 undetected: unreadable context path reported clean")
        locked.chmod(0o755)

        # Plant 2: bloated context (sparse file, so the test stays cheap).
        big = base / "big.bin"
        with open(big, "wb") as fh:
            fh.truncate(MAX_CONTEXT_BYTES + 1024)
        problems = check(base)
        if not any("over the" in p for p in problems):
            failures.append("plant 2 undetected: oversized context reported clean")
        big.unlink()

        # Plant 3: scratch dir not excluded by name.
        problems = check(base)
        if not any("bench/snapshot-it/data" in p for p in problems):
            failures.append("plant 3 undetected: unexcluded scratch dir reported clean")
    finally:
        try:
            for sub in base.rglob("*"):
                if sub.is_dir():
                    sub.chmod(0o755)
            import shutil

            shutil.rmtree(base)
        except OSError:
            pass

    if failures:
        print("FAIL: build-context check is vacuous or the tree regressed")
        for f in failures:
            print(f"  - {f}")
        return 1
    print(
        "ok (root-context builds exclude scratch state, are readable, and stay "
        f"under {MAX_CONTEXT_BYTES // (1024 * 1024)} MiB; 3 plants caught)"
    )
    return 0


def main() -> int:
    if sys.argv[1:] and sys.argv[1] == "--selftest":
        return selftest()
    problems = check(ROOT)
    if problems:
        for p in problems:
            print(f"FAIL: {p} (rule 62)")
        return 1
    print("ok (root-context Docker builds are readable and bounded)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

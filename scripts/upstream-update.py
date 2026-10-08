#!/usr/bin/env python3
"""Prepare and validate an upstream merge without changing the working branch.

No pushes, force operations, automatic conflict resolutions, or worktree cleanup.
The upstream remote and the current custom branch must already exist.
"""

import argparse
import datetime
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tomllib


class UpdateError(Exception):
    pass


def run(args, cwd, *, check=True, capture=True):
    result = subprocess.run(
        args, cwd=cwd, text=True, capture_output=capture, check=False
    )
    if check and result.returncode:
        detail = (result.stderr or result.stdout or "").strip() if capture else ""
        raise UpdateError(f"{args[0]} failed ({result.returncode}): {detail}")
    return result


def git(root, *args, **kwargs):
    return run(["git", *args], root, **kwargs)


def valid_ref(ref):
    # Inputs are remote branch names or release tags, never revision expressions.
    return bool(re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._/-]*", ref)) and not any(
        x in ref for x in ("..", "//", "@{")
    ) and not ref.endswith(("/", ".", ".lock"))


def prepare(args):
    root = Path(git(Path.cwd(), "rev-parse", "--show-toplevel").stdout.strip())
    branch = git(root, "symbolic-ref", "--quiet", "--short", "HEAD").stdout.strip()
    if not branch.startswith("custom/"):
        raise UpdateError("Start from a custom/* branch; main, release, and detached HEAD are protected.")
    if git(root, "status", "--porcelain").stdout.strip():
        raise UpdateError("Commit or preserve current changes first; the source checkout must be clean.")
    if not valid_ref(args.ref):
        raise UpdateError("Use a remote branch name or tag, not a revision expression.")
    git(root, "remote", "get-url", "upstream")
    # Fetch only the requested upstream ref; do not overwrite fork branches or tags.
    git(root, "fetch", "--no-tags", "upstream", args.ref)
    tip = git(root, "rev-parse", "--verify", "FETCH_HEAD^{commit}").stdout.strip()
    base = git(root, "rev-parse", "--verify", "HEAD^{commit}").stdout.strip()
    if git(root, "merge-base", base, tip, check=False).returncode:
        raise UpdateError("Upstream and the custom branch have no common ancestor; refusing unrelated history.")
    changes = git(root, "log", "--oneline", f"{base}..{tip}").stdout.strip()
    if args.plan:
        print(json.dumps({"source_branch": branch, "source": base, "upstream": tip,
                          "ref": args.ref, "new_commits": changes.splitlines()}, indent=2))
        return 0
    stamp = datetime.datetime.now(datetime.timezone.utc).strftime("%Y%m%dT%H%M%S%fZ")
    label = re.sub(r"[^A-Za-z0-9._-]", "-", args.ref)
    candidate_branch = f"updates/{label}-{tip[:10]}-{stamp}"
    candidate = (Path(args.worktree).expanduser().absolute() if args.worktree else
                 root.parent / "lightcraft-updates" / candidate_branch.removeprefix("updates/"))
    if candidate.resolve().is_relative_to(root.resolve()):
        raise UpdateError("Candidate must be outside the source checkout so its working tree stays clean.")
    if candidate.exists() or candidate.is_symlink():
        raise UpdateError(f"Candidate path already exists; nothing replaced: {candidate}")
    candidate.parent.mkdir(parents=True, exist_ok=True)
    git(root, "worktree", "add", "-b", candidate_branch, str(candidate), base)
    report_dir = candidate / "plan" / "upstream-update"
    report_dir.mkdir(parents=True, exist_ok=True)
    report = {"source_branch": branch, "source": base, "upstream": tip, "ref": args.ref,
              "candidate_branch": candidate_branch, "worktree": str(candidate),
              "status": "preparing", "new_commits": changes.splitlines()}

    def save(status):
        report["status"] = status
        (report_dir / "result.json").write_text(json.dumps(report, indent=2) + "\n")
        print(json.dumps(report, indent=2))

    result = git(candidate, "merge", "--no-ff", "--no-commit", tip, check=False)
    (report_dir / "merge.log").write_text((result.stdout or "") + (result.stderr or ""))
    if result.returncode:
        save("merge-needs-review")
        print("Conflict state is preserved in the candidate worktree; the source branch is unchanged.", file=sys.stderr)
        return 2
    inventory = candidate / "docs" / "custom" / "features.json"
    inventory_changed = False
    if inventory.is_file():
        try:
            data = json.loads(inventory.read_text())
            if not isinstance(data, dict):
                raise ValueError("custom inventory must be an object")
            previous = (data.get("upstream_base"), data.get("upstream_version"))
            recorded = previous[0]
            older_ref = (isinstance(recorded, str) and bool(re.fullmatch(r"[0-9a-f]{40}", recorded))
                         and git(candidate, "merge-base", "--is-ancestor", tip, recorded, check=False).returncode == 0)
            # Rechecking an older release must not roll the recorded integrated base backward.
            if not older_ref:
                data["upstream_base"] = tip
            cargo_manifest = candidate / "Cargo.toml"
            if cargo_manifest.is_file():
                package = tomllib.loads(cargo_manifest.read_text()).get("workspace", {}).get("package", {})
                if not older_ref and isinstance(package.get("version"), str):
                    data["upstream_version"] = "v" + package["version"]
            inventory_changed = previous != (data.get("upstream_base"), data.get("upstream_version"))
            if inventory_changed:
                inventory.write_text(json.dumps(data, indent=2) + "\n")
        except (OSError, ValueError, TypeError, AttributeError) as error:
            report["error"] = str(error)
            save("inventory-needs-review")
            return 3
    if args.prepare_only:
        save("prepared-not-validated")
        return 0
    # A failed gate leaves a complete candidate for inspection and repair, never publishes it.
    env = os.environ.copy()
    env.setdefault("CARGO_TARGET_DIR", str(root / "target" / "agent-upstream"))
    env.setdefault("CARGO_BUILD_JOBS", "2")
    env.setdefault("RUST_TEST_THREADS", "2")
    env.setdefault("CARGO_PROFILE_DEV_DEBUG", "0")
    checks = [[sys.executable, "scripts/check-custom-features.py"], ["cargo", "xtask", "ci"]]
    for number, command in enumerate(checks, 1):
        with (report_dir / f"check-{number}.log").open("w") as log:
            result = subprocess.run(command, cwd=candidate, env=env, stdout=log, stderr=subprocess.STDOUT, check=False)
        if result.returncode:
            save("checks-failed")
            return 3
    # Repository policy: the integration commit is created only after the complete CI gate.
    if inventory_changed:
        git(candidate, "add", "--", "docs/custom/features.json")
    if inventory_changed or git(candidate, "rev-parse", "--verify", "MERGE_HEAD", check=False).returncode == 0:
        git(candidate, "commit", "-m", f"CUSTOM-UPSTREAM: integrate {args.ref} ({tip[:10]})\n\nCo-authored-by: Codex <noreply@openai.com>")
    save("validated")
    print(f"Review {candidate_branch}; integrate explicitly with: git merge --ff-only {candidate_branch}")
    return 0


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--ref", default="main", help="upstream branch or release tag (default: main)")
    parser.add_argument("--worktree", help="new candidate directory; an existing path is always refused")
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--plan", action="store_true", help="fetch and show incoming commits without making a candidate")
    mode.add_argument("--prepare-only", action="store_true", help="leave an uncommitted candidate for review; do not run checks")
    args = parser.parse_args()
    try:
        return prepare(args)
    except (UpdateError, OSError, ValueError) as error:
        print(f"upstream-update: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())

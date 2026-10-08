# Maintaining the custom LightCraft fork

The fork retains upstream Git history and a separate `upstream` remote pointing at
`https://github.com/storytold/lightcraft.git`. `origin` is the user's fork. Custom
development lives on `custom/folder-raw-workflow`; `main` remains the original
base. Do not replace upstream source trees with vendored copies or rewrite history.

## Checking an upstream release

From a clean custom checkout with Python 3.11 or newer:

```sh
python3 scripts/upstream-update.py --ref v0.4.0 --plan
python3 scripts/upstream-update.py --ref v0.4.0
```

Use an actual newer release tag when one is available, or `main` to test the
creator's current branch. The first command fetches and lists incoming commits.
The second creates a new `updates/*` branch and a sibling candidate worktree,
merges the requested commit, checks the custom feature inventory and Python
update regressions, then runs the complete `cargo xtask ci` gate. A merge commit
is made only after both gates pass. The source custom checkout remains unchanged.

No command pushes, force-updates a branch, deletes a worktree, publishes a
release, or silently resolves a conflict. An existing candidate directory is
refused. Failed checks or conflicts leave the candidate and its evidence under
`plan/upstream-update/` for inspection and repair. `--prepare-only` makes an
unvalidated candidate when a manual review is needed first.

After reviewing a validated candidate, integrate it explicitly:

```sh
git merge --ff-only updates/THE-CANDIDATE-BRANCH
git push origin custom/folder-raw-workflow
```

If custom development continued while the candidate was tested, the fast-forward
will refuse; make a fresh candidate from the new custom tip and rerun the gates.
For a conflict, edit only the candidate worktree, run both checks and inspect
the changed behavior before committing. Never accept one side of a shared
source file wholesale just to remove merge markers.

## Compatibility and release checks

`docs/custom/features.json` records the upstream base, custom feature status,
source hooks, and regression files. `scripts/check-custom-features.py` catches
lost hooks. The full Rust CI runs behavior tests; file-existence checks alone
do not establish safety, compatibility, performance, or image quality.

`Custom feature compatibility` runs on custom/update branch pushes and custom
pull requests on Linux and Windows, the two platforms for this custom workflow.
Upstream's other platform paths stay intact to reduce future merge conflicts.
`Check an upstream release` is a
manual GitHub Actions workflow which produces a candidate patch and gate logs.
It has read-only repository permissions and never merges or publishes anything.
There is no recurring schedule. GitHub only exposes manual workflows in its
Actions UI when their definitions are present on the default branch; locally
the script works from the custom branch without that requirement.

The upstream release workflow remains separate. Do not push the custom branch
to upstream's `release` branch to produce fork binaries. Use a distinct custom
release identifier, include the original MIT/Apache notices and asset licences,
and report the exact upstream commit plus the custom commit in release notes.
Native platform and photo-processing acceptance is required before describing
an artifact as ready for the user's desktop.

Keep feature commits small, isolate custom modules, and preserve existing
command and file formats. Reusable fixes may be contributed upstream separately;
personal workflow defaults can remain in the custom fork. This reduces future
merge conflicts without promising that arbitrary future changes merge cleanly.

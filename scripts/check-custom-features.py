#!/usr/bin/env python3
"""Check custom feature hooks survive an upstream merge, then run their safety tests.

This inventory check complements cargo xtask ci; it does not prove image quality.
"""

import json
from pathlib import Path
import subprocess
import sys


def main():
    root = Path(__file__).resolve().parents[1]
    errors = []
    try:
        data = json.loads((root / "docs/custom/features.json").read_text())
        if not isinstance(data, dict) or data.get("schema") != 1:
            raise ValueError("unknown custom feature schema")
        ids = set()
        for feature in data["features"]:
            fid = feature["id"]
            if fid in ids:
                raise ValueError(f"duplicate feature {fid}")
            ids.add(fid)
            if feature["status"] not in {"implemented", "in-progress", "planned"}:
                raise ValueError(f"unknown status for {fid}")
            if feature["status"] != "implemented":
                continue
            if not feature["files"] or not feature["checks"]:
                raise ValueError(f"{fid}: implemented feature needs files and retention checks")
            for name in feature["files"]:
                path = (root / name).resolve()
                if not path.is_relative_to(root) or not path.is_file():
                    errors.append(f"{fid}: missing or invalid file {name}")
            for check in feature["checks"]:
                path = (root / check["file"]).resolve()
                if not path.is_relative_to(root) or not path.is_file() or check["contains"] not in path.read_text():
                    errors.append(f"{fid}: missing hook in {check['file']}")
    except (OSError, ValueError, KeyError, TypeError, AttributeError) as error:
        errors.append(str(error))
    if errors:
        print("Custom feature retention failed:\n" + "\n".join(errors), file=sys.stderr)
        return 1
    print("Custom feature inventory retained; running upstream-update safety regressions.")
    return subprocess.run([sys.executable, "-m", "unittest", "discover", "-s", "scripts/tests", "-v"], cwd=root, check=False).returncode


if __name__ == "__main__":
    raise SystemExit(main())

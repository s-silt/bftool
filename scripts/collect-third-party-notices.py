"""Validate reviewed, pinned desktop notices; optionally assemble package LICENSE.

This intentionally fails after dependency changes. Recollect original notices from
the exact pinned crate/npm/SDK sources before updating the inventory hashes.
"""
import argparse
import hashlib
import json
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def validate():
    inventory = json.loads((ROOT / "licenses/third-party-components.json").read_text(encoding="utf-8"))
    checks = dict(inventory["lock_sha256"])
    checks["THIRD_PARTY_NOTICES.txt"] = inventory["notices_sha256"]
    for name, expected in checks.items():
        actual = hashlib.sha256((ROOT / name).read_bytes()).hexdigest()
        if actual != expected:
            raise SystemExit(f"Reviewed notices are stale or modified: {name}. Review pinned licenses before packaging.")
    return inventory


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    data = validate()
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_bytes((ROOT / "LICENSE").read_bytes() + b"\n\n" + (ROOT / "THIRD_PARTY_NOTICES.txt").read_bytes())
    print(f"Validated reviewed notices for {len(data['components'])} pinned Rust/npm components and WebView2 loader.")

#!/usr/bin/env python3
"""Write a CycloneDX 1.5 SBOM (JSON) of the locked dependency graph.

Usage: python tools/cyclonedx_sbom.py OUTPUT.json   (run from the workspace root)

Reads `cargo metadata --locked` with all features and every target platform, so it lists every crate the
workspace can build with. Needs no tools beyond cargo.
"""
import json
import subprocess
import sys
import uuid
from datetime import datetime, timezone


def purl(p):
    return f"pkg:cargo/{p['name']}@{p['version']}"


def main():
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    meta = json.loads(subprocess.check_output(
        ["cargo", "metadata", "--format-version", "1", "--locked", "--all-features"]))
    pkgs = {p["id"]: p for p in meta["packages"]}
    members = set(meta["workspace_members"])
    used = {n["id"] for n in meta["resolve"]["nodes"]}

    def component(p):
        c = {"type": "application" if p["name"] == "ironav-cli" else "library",
             "bom-ref": purl(p), "name": p["name"], "version": p["version"], "purl": purl(p)}
        if p.get("description"):
            c["description"] = p["description"].strip()
        if p.get("license"):
            c["licenses"] = [{"expression": p["license"]}]
        refs = []
        if p.get("repository"):
            refs.append({"type": "vcs", "url": p["repository"]})
        if p["source"] and p["source"].startswith("registry+"):
            refs.append({"type": "distribution", "url": f"https://crates.io/crates/{p['name']}/{p['version']}"})
        if refs:
            c["externalReferences"] = refs
        if p["id"] in members:
            c["supplier"] = {"name": "Nervosys"}
        return c

    root = next(p for p in pkgs.values() if p["name"] == "ironav-cli")
    bom = {
        "bomFormat": "CycloneDX",
        "specVersion": "1.5",
        "serialNumber": f"urn:uuid:{uuid.uuid4()}",
        "version": 1,
        "metadata": {
            "timestamp": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
            "tools": {"components": [{"type": "application", "name": "tools/cyclonedx_sbom.py"}]},
            "component": {**component(root), "name": "ironav"},
        },
        "components": [component(pkgs[i]) for i in sorted(used, key=lambda i: (pkgs[i]["name"], pkgs[i]["version"])) if i != root["id"]],
        "dependencies": [
            {"ref": purl(pkgs[n["id"]]), "dependsOn": sorted({purl(pkgs[d["pkg"]]) for d in n["deps"]})}
            for n in meta["resolve"]["nodes"]
        ],
    }
    with open(sys.argv[1], "w", encoding="utf-8", newline="\n") as f:
        json.dump(bom, f, indent=1)
        f.write("\n")
    print(f"{len(bom['components']) + 1} components")


if __name__ == "__main__":
    main()

"""Writes the licences of the Rust libraries the app ships into
Sources/tableport/Resources/ThirdPartyLicences.txt. Their licences (MIT, BSD,
Apache and others) ask that binary copies carry the notice text.

Run from the repository root: python3 -I scripts/licences.py
"""

import json
import pathlib
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
OUT = ROOT / "Sources/tableport/Resources/ThirdPartyLicences.txt"
NAMES = ("LICENSE", "LICENCE", "COPYING", "NOTICE", "UNLICENSE")

# Only the libraries linked into the app, not build tools or test helpers.
tree = subprocess.run(
    ["cargo", "tree", "-p", "tableport-core", "-e", "normal,no-proc-macro",
     "--target", "aarch64-apple-darwin", "--prefix", "none", "-f", "{p}"],
    cwd=ROOT, capture_output=True, text=True, check=True,
).stdout
shipped = set()
for line in tree.splitlines():
    parts = line.split()
    if len(parts) >= 2:
        shipped.add((parts[0], parts[1].lstrip("v")))

metadata = json.loads(subprocess.run(
    ["cargo", "metadata", "--format-version", "1"],
    cwd=ROOT, capture_output=True, text=True, check=True,
).stdout)

# Each notice text is written once, with the libraries that carry it. Where a
# library offers MIT, only its MIT text is kept, which keeps the file small.
by_text = {}
missing = []
for package in metadata["packages"]:
    if package["source"] is None or (package["name"], package["version"]) not in shipped:
        continue
    directory = pathlib.Path(package["manifest_path"]).parent
    files = sorted(
        path for path in directory.iterdir()
        if path.is_file() and path.name.upper().startswith(NAMES)
    )
    mit = [path for path in files if "MIT" in path.name.upper()]
    if mit and "MIT" in (package.get("license") or ""):
        files = mit + [path for path in files if path.name.upper().startswith("NOTICE")]
    label = f'{package["name"]} {package["version"]} ({package.get("license") or "see text"})'
    if not files:
        missing.append(label)
        continue
    for path in files:
        by_text.setdefault(path.read_text(errors="replace").strip(), []).append(label)

sections = [
    "TablePort includes the following open-source libraries. Their licences follow.\n"
    "Oracle Instant Client is not included; users download it from Oracle."
]
for text, labels in sorted(by_text.items(), key=lambda item: item[1][0]):
    sections.append("\n".join(sorted(labels)) + "\n\n" + text)
if missing:
    sections.append(
        "These libraries ship no licence file; their licence is named beside them.\n"
        "The Apache-2.0 and MIT texts appear above. MPL-2.0: https://mozilla.org/MPL/2.0/;\n"
        "UniFFI source: https://github.com/mozilla/uniffi-rs\n\n"
        + "\n".join(sorted(missing))
    )
OUT.write_text(("\n\n" + "=" * 72 + "\n\n").join(sections) + "\n")
print(f"{OUT.relative_to(ROOT)}: {len(by_text)} texts, {len(missing)} without", file=sys.stderr)

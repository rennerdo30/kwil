#!/bin/bash
# Run only on the owner's Mac. Produces files for a separate, manual upload.
set -euo pipefail

if [[ $# != 2 ]]; then
  echo "Usage: $0 vX.Y.Z OUTPUT_DIR" >&2
  exit 2
fi
tag=$1
[[ $tag =~ ^v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]] || { echo 'Invalid release tag' >&2; exit 2; }
[[ $(uname -s) == Darwin ]] || { echo 'Run this script on the signing Mac.' >&2; exit 1; }
repo=rennerdo30/kwil
ID=${KWIL_MACOS_SIGN_IDENTITY:-Developer ID Application: Dominic Renner (73L5ZHST4Q)}
profile=${KWIL_MACOS_NOTARY_PROFILE:-renner-notary}
version=${tag#v}
out=$2
mkdir -p "$out"
out=$(cd "$out" && pwd)
assets=(kwil-aarch64-apple-darwin kwil-x86_64-apple-darwin
  "kwil-$version-macos-arm64.tar.gz" "kwil-$version-macos-x64.tar.gz"
  "kwil-$version-macos-arm64.zip" "kwil-$version-macos-x64.zip")
for asset in "${assets[@]}"; do
  [[ ! -e $out/$asset ]] || { echo "Output already exists: $out/$asset" >&2; exit 1; }
done
[[ $(gh release view "$tag" --repo "$repo" --json isDraft --jq .isDraft) == true ]] || {
  echo 'Signing requires a draft release.' >&2; exit 1;
}
work=$(mktemp -d "${TMPDIR:-/tmp}/kwil-sign.XXXXXX")
trap 'rm -rf "$work"' EXIT
mkdir "$work/download" "$work/signed" "$work/pkg"
gh release download "$tag" --repo "$repo" --dir "$work/download" \
  --pattern SHA256SUMS --pattern "${assets[0]}" --pattern "${assets[1]}" \
  --pattern "${assets[2]}" --pattern "${assets[3]}" \
  --pattern "${assets[4]}" --pattern "${assets[5]}"

# Verify ALL six original files before extracting, chmod, or signing any of them.
python3 - "$work/download" "${assets[@]}" <<'PY'
import hashlib
from pathlib import Path
import sys

root = Path(sys.argv[1])
lines = (root / "SHA256SUMS").read_text().splitlines()
for name in sys.argv[2:]:
    matches = [line.split("  ", 1)[0] for line in lines if line.endswith("  " + name)]
    if len(matches) != 1 or hashlib.sha256((root / name).read_bytes()).hexdigest() != matches[0]:
        sys.exit("Checksum missing, duplicated, or mismatched: " + name)
    print("Verified original: " + name)
PY

for pair in aarch64-apple-darwin:arm64 x86_64-apple-darwin:x64; do
  target=${pair%:*}
  arch=${pair#*:}
  bare=kwil-$target
  name=kwil-$version-macos-$arch
  # Check the release.yml layout before extraction; reject links and extra paths.
  python3 - "$work/download/$name.tar.gz" "$name" "$work/download/$name.zip" "$work/download/$bare" <<'PY'
from pathlib import Path
import stat
import sys
import tarfile
import zipfile

with tarfile.open(sys.argv[1]) as archive:
    root = sys.argv[2]
    expected = {root: "dir", **{root + "/" + name: "file" for name in
                ("kwil", "LICENSE.txt", "THIRD-PARTY-NOTICES.txt")}}
    members = archive.getmembers()
    actual = {m.name.rstrip("/"): "dir" if m.isdir() else "file" if m.isfile() else "link" for m in members}
    if len(members) != len(expected) or actual != expected:
        sys.exit("Unexpected archive layout: " + sys.argv[1])
    if archive.getmember(root + "/kwil").mode != 0o755:
        sys.exit("Expected kwil mode 755")
with zipfile.ZipFile(sys.argv[3]) as archive:
    members = archive.infolist()
    actual = {m.filename.rstrip("/"): "dir" if stat.S_ISDIR(m.external_attr >> 16)
              else "file" if stat.S_ISREG(m.external_attr >> 16) else "link" for m in members}
    if len(members) != len(expected) or actual != expected:
        sys.exit("Unexpected archive layout: " + sys.argv[3])
    binary = archive.getinfo(root + "/kwil")
    if binary.create_system != 3 or stat.S_IMODE(binary.external_attr >> 16) != 0o755:
        sys.exit("Expected zip kwil Unix mode 755")
    if archive.read(binary) != Path(sys.argv[4]).read_bytes():
        sys.exit("Zip kwil does not match bare binary")
PY
  tar -xzf "$work/download/$name.tar.gz" -C "$work/pkg"
  cmp "$work/download/$bare" "$work/pkg/$name/kwil"
  cp "$work/download/$bare" "$work/signed/$bare"
  chmod 755 "$work/signed/$bare"
  codesign --force --timestamp --options runtime --sign "$ID" "$work/signed/$bare"
  codesign -d --entitlements - "$work/signed/$bare" > "$work/entitlements.plist"
  # No entitlement file or preserved debug entitlements are allowed.
  python3 - "$work/entitlements.plist" <<'PY'
import pathlib
import plistlib
import sys

data = pathlib.Path(sys.argv[1]).read_bytes()
if data.strip() and plistlib.loads(data):
    sys.exit("Expected no entitlements (including get-task-allow)")
PY
  codesign --verify --strict --verbose=2 "$work/signed/$bare"
  ditto -c -k --sequesterRsrc --keepParent "$work/signed/$bare" "$work/$target.zip"
  submit_status=0
  xcrun notarytool submit "$work/$target.zip" --keychain-profile "$profile" \
    --wait --timeout 30m --output-format json > "$work/notary.json" || submit_status=$?
  submission=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get("id", ""))' "$work/notary.json")
  status=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get("status", ""))' "$work/notary.json")
  if [[ -n $submission ]]; then
    printf 'Notary log: xcrun notarytool log %q --keychain-profile %q\n' "$submission" "$profile"
  else
    echo 'No submission ID returned; use xcrun notarytool history with your Keychain profile.' >&2
  fi
  if [[ $submit_status != 0 || $status != Accepted ]]; then
    echo "Notarization failed for $target (status: $status; exit: $submit_status)." >&2
    exit 1
  fi
  # Overwrite only the executable bytes, preserving the archive's layout and modes.
  cat "$work/signed/$bare" > "$work/pkg/$name/kwil"
  COPYFILE_DISABLE=1 tar -C "$work/pkg" -czf "$work/signed/$name.tar.gz" "$name"
  # Plain zip stores Unix modes without resource forks or __MACOSX sidecars;
  # -X omits extra attributes. Keep the same top folder as tar.gz and Windows.
  (cd "$work/pkg" && zip -X -r "$work/signed/$name.zip" "$name")
  zipinfo -l "$work/signed/$name.zip" "$name/kwil" | grep -E '^-rwxr-xr-x[[:space:]]'
done

# Publish local output only after BOTH architectures have been accepted. No uploads.
for asset in "${assets[@]}"; do
  cp -p "$work/signed/$asset" "$out/$asset"
done
(cd "$out" && shasum -a 256 "${assets[@]}")

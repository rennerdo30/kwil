#!/bin/sh
# Published stable releases only. No sudo, shell profile or association changes.
set -eu
version=${1:-latest}
if [ "$version" != latest ]; then
    printf '%s\n' "$version" | LC_ALL=C grep -Eq '^v[0-9]+\.[0-9]+\.[0-9]+$' || { echo 'Expected latest or vX.Y.Z' >&2; exit 1; }
fi
command -v curl >/dev/null || { echo 'curl is required' >&2; exit 1; }
command -v python3 >/dev/null || { echo 'python3 is required for strict release metadata parsing' >&2; exit 1; }
case "$(uname -s):$(uname -m)" in
    Linux:x86_64) target=x86_64-unknown-linux-gnu ;;
    Linux:aarch64|Linux:arm64) target=aarch64-unknown-linux-gnu ;;
    Darwin:x86_64) target=x86_64-apple-darwin ;;
    Darwin:arm64) target=aarch64-apple-darwin ;;
    *) echo 'Unsupported OS/architecture' >&2; exit 1 ;;
esac
case "${HOME:-}" in /*) ;; *) echo 'HOME must be absolute' >&2; exit 1 ;; esac
root="$HOME/.local/share/kwil"
bin="$HOME/.local/bin"
mkdir -p "$root" "$bin"
stage=$(mktemp -d "$root/.install-XXXXXXXX")
cleanup() { rm -f "$stage/release.json" "$stage/SHA256SUMS" "$stage/kwil"; rmdir "$stage"; }
trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM
fetch() { curl --fail --silent --show-error --location --proto '=https' --proto-redir '=https' --connect-timeout 10 --max-time 60 --max-filesize "$3" --output "$2" "$1"; }
endpoint="https://api.github.com/repos/rennerdo30/kwil/releases/"
if [ "$version" = latest ]; then endpoint="${endpoint}latest"; else endpoint="${endpoint}tags/$version"; fi
fetch "$endpoint" "$stage/release.json" 1048576
tag=$(python3 - "$stage/release.json" "$version" <<'PY'
import json, re, sys
with open(sys.argv[1], encoding='utf-8') as f:
    release = json.load(f)
tag = release.get('tag_name', '')
if release.get('draft') or release.get('prerelease') or not re.fullmatch(r'v[0-9]+\.[0-9]+\.[0-9]+', tag):
    raise SystemExit('Expected a published stable release')
if sys.argv[2] != 'latest' and tag != sys.argv[2]:
    raise SystemExit('Release version mismatch')
print(tag)
PY
)
destination="$root/$tag"
if [ -e "$destination" ] || [ -L "$destination" ] || [ -e "$bin/kwil" ] || [ -L "$bin/kwil" ]; then
    echo 'Existing installation retained; use its package/update mechanism or choose a fresh account.' >&2
    exit 1
fi
base="https://github.com/rennerdo30/kwil/releases/download/$tag"
fetch "$base/SHA256SUMS" "$stage/SHA256SUMS" 1048576
fetch "$base/kwil-$target" "$stage/kwil" 268435456
python3 - "$stage" "kwil-$target" <<'PY'
import hashlib, pathlib, re, sys
stage, name = pathlib.Path(sys.argv[1]), sys.argv[2]
lines = stage.joinpath('SHA256SUMS').read_text(encoding='ascii').splitlines()
matches = [m[1] for line in lines if (m := re.fullmatch(r'([0-9a-fA-F]{64}) [ *]' + re.escape(name), line))]
if len(matches) != 1:
    raise SystemExit('Missing or duplicate checksum')
h = hashlib.sha256()
with stage.joinpath('kwil').open('rb') as f:
    for block in iter(lambda: f.read(65536), b''):
        h.update(block)
if h.hexdigest() != matches[0].lower():
    raise SystemExit('Checksum mismatch')
PY
chmod 755 "$stage/kwil"
rm "$stage/release.json" "$stage/SHA256SUMS"
# rename without overwriting a racing destination; publish symlink without replacement.
python3 - "$stage" "$destination" "$bin/kwil" <<'PY'
import os, sys
# mkdir claims this exact version without replacing a pre-existing installation.
os.mkdir(sys.argv[2], 0o755)
try:
    os.rename(os.path.join(sys.argv[1], 'kwil'), os.path.join(sys.argv[2], 'kwil'))
except BaseException:
    os.rmdir(sys.argv[2])
    raise
os.symlink(os.path.join(sys.argv[2], 'kwil'), sys.argv[3])
PY
echo "Installed Kwil $tag: $bin/kwil"
echo 'No shell profile or persistent PATH was changed. Add ~/.local/bin manually if needed.'

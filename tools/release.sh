#!/usr/bin/env bash
# Builds the release artifacts into dist/<version>/:
#
#   Flint-<v>-x64.msi            per-user installer (wixl)
#   Flint-<v>-x64-portable.zip   portable folder (portable.marker included)
#   flint-plugin-epoch-<v>.zip   sample plugin
#   sbom/*.cdx.json              CycloneDX SBOMs of the shipped binaries
#   THIRD-PARTY-NOTICES.txt      licenses of everything linked in
#   SHA256SUMS                   checksums of all of the above
#   update.json (+ .sig)         update manifest; signed when
#                                FLINT_UPDATE_SIGNING_KEY is set
#
# Reproducibility: path prefixes are remapped, the PE timestamp is not
# inserted, and archives use SOURCE_DATE_EPOCH (the commit time). Rebuilding
# the same commit with the same toolchain gives the same executables and
# zips; `tools/release.sh --check-repro` builds twice in different
# directories and compares the executables. The MSI wrapper is NOT
# byte-reproducible (wixl writes a random package code); verify an MSI by
# comparing the files it contains (`msiextract`) against SHA256SUMS of a
# rebuild.
#
# Code signing (Authenticode) is a hook: set FLINT_SIGN_CMD to a command
# that signs the file given as its last argument (e.g. an osslsigncode or
# SignPath/Azure Trusted Signing wrapper). Without it the artifacts are
# unsigned, and the script says so.
#
# Needs: rustup target x86_64-pc-windows-gnu, mingw-w64, wixl (msitools),
# cargo-cyclonedx, cargo-deny, python3.
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT=$PWD
TARGET=x86_64-pc-windows-gnu
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
REPO_URL=https://github.com/24f2000058/windows-search
COMMIT=$(git rev-parse --short=12 HEAD 2>/dev/null || echo unknown)
export SOURCE_DATE_EPOCH=${SOURCE_DATE_EPOCH:-$(git log -1 --format=%ct 2>/dev/null || echo 315532800)}
export FLINT_BUILD_ID="$COMMIT"
CARGO_HOME_DIR=${CARGO_HOME:-$HOME/.cargo}

build() { # <target-dir>
    RUSTFLAGS="--remap-path-prefix=$ROOT=/flint --remap-path-prefix=$CARGO_HOME_DIR=/cargo -C link-arg=-Wl,--no-insert-timestamp" \
        cargo build --profile dist --locked --target "$TARGET" --target-dir "$1" \
        -p flint-ui-win32 --features flint-ui-win32/gui -p flint-preview -p flint-plugin-epoch
}

if [[ "${1:-}" == "--check-repro" ]]; then
    A=$(mktemp -d) B=$(mktemp -d)
    build "$A" >/dev/null 2>&1
    build "$B" >/dev/null 2>&1
    status=0
    for exe in flint-ui-win32.exe flint-preview-host.exe flint-plugin-epoch.exe; do
        ha=$(sha256sum "$A/$TARGET/dist/$exe" | cut -d' ' -f1)
        hb=$(sha256sum "$B/$TARGET/dist/$exe" | cut -d' ' -f1)
        if [[ "$ha" == "$hb" ]]; then echo "reproducible: $exe $ha"; else echo "DIFFERS: $exe $ha $hb"; status=1; fi
    done
    rm -rf "$A" "$B"
    exit $status
fi

echo "== license gate"
cargo deny --locked check licenses

echo "== build $VERSION ($COMMIT)"
build "$ROOT/target"
REL="$ROOT/target/$TARGET/dist"
DIST="$ROOT/dist/$VERSION"
STAGE="$DIST/stage"
rm -rf "$DIST"
mkdir -p "$STAGE" "$DIST/sbom"

sign() {
    if [[ -n "${FLINT_SIGN_CMD:-}" ]]; then
        $FLINT_SIGN_CMD "$1"
    else
        echo "   (unsigned: $(basename "$1"); set FLINT_SIGN_CMD to sign)"
    fi
}

cp "$REL/flint-ui-win32.exe" "$STAGE/flint.exe"
cp "$REL/flint-preview-host.exe" "$STAGE/"
sign "$STAGE/flint.exe"
sign "$STAGE/flint-preview-host.exe"
cp LICENSE-MIT "$STAGE/LICENSE-MIT.txt"
cp LICENSE-APACHE "$STAGE/LICENSE-APACHE.txt"
python3 tools/notices.py > "$STAGE/THIRD-PARTY-NOTICES.txt"
cp "$STAGE/THIRD-PARTY-NOTICES.txt" "$DIST/"

echo "== installer"
# Product code derived from the version (same version, same code).
PRODUCT_CODE=$(python3 -c "import uuid,sys; print(str(uuid.uuid5(uuid.UUID('7d3f8a64-4c1e-4b8a-9b0e-5f2a1c9e6d31'), 'flint-' + sys.argv[1])).upper())" "$VERSION")
wixl -a x64 -D Version="$VERSION" -D ProductCode="$PRODUCT_CODE" -D SourceDir="$STAGE" -o "$DIST/Flint-$VERSION-x64.msi" installer/flint.wxs
sign "$DIST/Flint-$VERSION-x64.msi"

echo "== portable"
P="$DIST/portable"
mkdir -p "$P"
cp "$STAGE"/* "$P/"
: > "$P/portable.marker"
cat > "$P/README-portable.txt" <<TXT
Flint $VERSION, portable.

Run flint.exe. Settings, the index and plugins are kept in the "data"
folder next to it (created on first run); nothing is written elsewhere and
nothing is added to start-up. Delete the folder to remove Flint completely.
Press Ctrl+Space to search; /diag shows diagnostics.
TXT
python3 tools/zipdet.py "$DIST/Flint-$VERSION-x64-portable.zip" "$P" "Flint-$VERSION"
rm -rf "$P"

echo "== sample plugin"
SPD="$DIST/plugin-epoch"
mkdir -p "$SPD"
cp "$REL/flint-plugin-epoch.exe" crates/flint-plugin-epoch/plugin.toml "$SPD/"
sign "$SPD/flint-plugin-epoch.exe"
python3 tools/zipdet.py "$DIST/flint-plugin-epoch-$VERSION.zip" "$SPD" "epoch"
rm -rf "$SPD"

echo "== SBOM"
# cargo-cyclonedx writes one SBOM per workspace member next to its
# Cargo.toml; keep those of the shipped binaries and remove the rest.
cargo cyclonedx --format json --target "$TARGET" --spec-version 1.5 --override-filename sbom.cdx >/dev/null
for c in flint-ui-win32 flint-preview; do
    mv "crates/$c/sbom.cdx.json" "$DIST/sbom/$c.cdx.json"
done
find crates -name sbom.cdx.json -delete

echo "== update manifest"
DL="$REPO_URL/releases/download/v$VERSION"
cargo run -q --release --locked -p flint-update --bin flint-release -- manifest --version "$VERSION" \
    --notes-url "$REPO_URL/releases/tag/v$VERSION" --released "$(date -u -d "@$SOURCE_DATE_EPOCH" +%Y-%m-%d)" \
    --msi "$DIST/Flint-$VERSION-x64.msi" "$DL/Flint-$VERSION-x64.msi" \
    --portable "$DIST/Flint-$VERSION-x64-portable.zip" "$DL/Flint-$VERSION-x64-portable.zip" > "$DIST/update.json"
if [[ -n "${FLINT_UPDATE_SIGNING_KEY:-}" ]]; then
    cargo run -q --release --locked -p flint-update --bin flint-release -- sign "$DIST/update.json" --id "${FLINT_UPDATE_KEY_ID:-release-1}"
else
    echo "   (update.json unsigned: set FLINT_UPDATE_SIGNING_KEY; clients reject unsigned manifests)"
fi

rm -rf "$STAGE"
(cd "$DIST" && find . -type f ! -name SHA256SUMS | sed 's|^\./||' | LC_ALL=C sort | xargs sha256sum > SHA256SUMS)
echo "== done: $DIST"
ls -la "$DIST"

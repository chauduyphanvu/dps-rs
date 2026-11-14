#!/bin/sh
# Builds a universal macOS binary, signs it with the Developer ID certificate,
# notarises it and leaves the zip and its .sha256 in dist/. It never tags,
# pushes or uploads anything except the notarisation submission to Apple.
# Pushing a vX.Y.Z tag runs it in .github/workflows/release.yml, which then
# publishes what it left in dist/.
#
# It signs with the "Developer ID Application" identity in the keychain and
# notarises with the notarytool keychain profile dps-notary (or the one named
# by DPS_NOTARY_PROFILE). README.md, under Releasing, shows how to create the
# profile from the API key in 1Password.
set -eu

die() {
    echo "release: $*" >&2
    exit 1
}

cd "$(dirname "$0")/.."

version=$(cargo pkgid | sed 's/.*[#@]//')
[ -n "$version" ] || die "could not read the version from Cargo.toml"
archive="dist/dps-$version-macos-universal.zip"

dirty=$(git status --porcelain)
[ -z "$dirty" ] || die "the working tree has uncommitted changes; commit or revert them first"

installed=$(rustup target list --installed)
missing=
for target in aarch64-apple-darwin x86_64-apple-darwin; do
    echo "$installed" | grep -qx "$target" || missing="$missing $target"
done
[ -z "$missing" ] || die "missing Rust target(s); run: rustup target add$missing"

for target in aarch64-apple-darwin x86_64-apple-darwin; do
    cargo build --release --locked --target "$target"
done

mkdir -p dist
rm -f dist/dps "$archive" "$archive.sha256"
lipo -create -output dist/dps \
    target/aarch64-apple-darwin/release/dps \
    target/x86_64-apple-darwin/release/dps
archs=$(lipo -archs dist/dps)
for arch in arm64 x86_64; do
    case " $archs " in
        *" $arch "*) ;;
        *) die "dist/dps lacks $arch (lipo reports: $archs)" ;;
    esac
done

# Same identifier as scripts/install.sh, so audio-recording consent survives
# upgrades. Notarisation requires the hardened runtime and a secure timestamp.
codesign --force --options runtime --timestamp \
    --sign "Developer ID Application" --identifier dev.dps.cli dist/dps
codesign --verify --strict dist/dps

# --keepParent would nest the binary as dist/dps, and without --noextattr ditto
# adds a ._dps entry for the file's extended attributes. This leaves a lone
# executable named dps at the zip root, which the formula installs.
ditto -c -k --norsrc --noextattr dist/dps "$archive"

# A bare binary or zip cannot be stapled (tickets attach only to app bundles,
# disk images and installer packages), so Gatekeeper checks the ticket online.
# notarytool may exit 0 for a rejected submission, so read the final status.
log=$(mktemp)
trap 'rm -f "$log"' EXIT
xcrun notarytool submit "$archive" \
    --keychain-profile "${DPS_NOTARY_PROFILE:-dps-notary}" --wait | tee "$log"
grep -q '^ *status: Accepted$' "$log" ||
    die "notarisation was not Accepted; see the output above (xcrun notarytool log <id> --keychain-profile ...)"

# The bare file name, so `shasum -a 256 -c` works beside the downloaded zip.
(cd dist && shasum -a 256 "${archive#dist/}") >"$archive.sha256"
sha256=$(awk '{print $1}' "$archive.sha256")
cat <<EOF

Built and notarised $archive
sha256 $sha256 (in $archive.sha256)
EOF

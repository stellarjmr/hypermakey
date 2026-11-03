#!/usr/bin/env bash
# Build, sign, verify, and package the release binary.
#
#   scripts/package-release.sh [tag]     # tag defaults to v<version from Cargo.toml>
#
# Output: dist/hypermakey-<tag>-aarch64-apple-darwin.tar.gz and a matching .sha256 file.
#
# The binary is signed ad hoc with the hardened runtime. dyld then ignores DYLD_* environment
# variables, so no other code can be injected into a process that holds Accessibility
# permission. Event taps and IOHIDManager need no entitlements.
set -euo pipefail

cd "$(dirname "$0")/.."

target="aarch64-apple-darwin"
version="$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -n 1)"
tag="${1:-v${version}}"
if [[ "${tag}" != "v${version}" ]]; then
  echo "error: tag ${tag} does not match Cargo.toml version ${version}" >&2
  exit 1
fi

cargo build --release --locked --target "${target}"

binary="target/${target}/release/hypermakey"
codesign --force --sign - --options runtime "${binary}"
codesign --verify --strict --verbose=2 "${binary}"
if ! codesign --display --verbose "${binary}" 2>&1 \
  | grep -Eq '^CodeDirectory .*flags=0x[0-9a-f]+\([^)]*runtime'; then
  echo "error: ${binary} is not signed with the hardened runtime" >&2
  exit 1
fi
"${binary}" --version

archive="hypermakey-${tag}-${target}.tar.gz"
mkdir -p dist
# Keep macOS metadata (AppleDouble ._ entries, extended attributes) out of the archive.
COPYFILE_DISABLE=1 tar --no-xattrs -C "target/${target}/release" -czf "dist/${archive}" hypermakey
(cd dist && shasum -a 256 "${archive}" | tee "${archive}.sha256")

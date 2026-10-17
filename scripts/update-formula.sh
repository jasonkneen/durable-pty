#!/usr/bin/env bash
# After pushing a tag (e.g. v0.1.0): fills packaging/homebrew/dpty.rb with that tag's tarball URL and sha256.
# Copy the result to Formula/dpty.rb in your tap repository (jasonkneen/homebrew-tap).
set -euo pipefail
version="${1:?usage: scripts/update-formula.sh v0.1.0}"
repo="${DPTY_REPO:-jasonkneen/durable-pty}"
formula="$(cd "$(dirname "$0")/.." && pwd)/packaging/homebrew/dpty.rb"
url="https://github.com/$repo/archive/refs/tags/$version.tar.gz"
sha="$(curl -fsSL "$url" | shasum -a 256 | cut -d' ' -f1)"
sed -i.bak -E "s#^  url \".*\"#  url \"$url\"#; s#^  sha256 \".*\"#  sha256 \"$sha\"#" "$formula"
rm -f "$formula.bak"
echo "updated $formula"; echo "  url    $url"; echo "  sha256 $sha"

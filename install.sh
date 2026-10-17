#!/bin/sh
# Installs dpty (durable-pty): downloads the release binary for this machine, checks its sha256, puts it in ~/.local/bin.
#
#   curl -fsSL https://raw.githubusercontent.com/jasonkneen/durable-pty/main/install.sh | sh
#
# Environment:
#   DPTY_VERSION       a release tag such as v0.1.0 (default: the latest release)
#   DPTY_INSTALL_DIR   where to put the binary (default: ~/.local/bin)
#   DPTY_REPO          GitHub owner/name to download from (default: jasonkneen/durable-pty)
#   DPTY_RELEASE_BASE  download from this base URL instead of GitHub (a mirror, or file:// for testing)
#
# If a download is not reachable anonymously (a private mirror), it falls back to the GitHub CLI when you are signed in.
set -eu

REPO="${DPTY_REPO:-jasonkneen/durable-pty}"
VERSION="${DPTY_VERSION:-latest}"
DIR="${DPTY_INSTALL_DIR:-$HOME/.local/bin}"
BASE="${DPTY_RELEASE_BASE:-}"

say() { printf '%s\n' "$*"; }
die() { printf 'install.sh: %s\n' "$*" >&2; exit 1; }

usage() {
  cat <<'USAGE'
Installs dpty: a PTY session daemon, CLI and MCP server in one binary.

Usage: install.sh [--uninstall] [--help]

Environment: DPTY_VERSION, DPTY_INSTALL_DIR (default ~/.local/bin), DPTY_REPO, DPTY_RELEASE_BASE.
After installing, run `dpty install` to start it at login.
USAGE
}

case "${1:-}" in
  -h|--help) usage; exit 0 ;;
  --uninstall)
    rm -f "$DIR/dpty"
    say "removed $DIR/dpty (sessions and history in ~/.durable-pty are kept; run 'dpty uninstall' first if you set up login start)"
    exit 0 ;;
  "") ;;
  *) usage >&2; exit 2 ;;
esac

case "$(uname -s)" in
  Darwin) os="apple-darwin" ;;
  Linux) os="unknown-linux-gnu" ;;
  *) die "unsupported system '$(uname -s)': dpty runs on macOS and Linux" ;;
esac
case "$(uname -m)" in
  arm64|aarch64) arch="aarch64" ;;
  x86_64|amd64) arch="x86_64" ;;
  *) die "unsupported CPU '$(uname -m)'" ;;
esac
target="$arch-$os"
asset="dpty-$target.tar.gz"

if [ -z "$BASE" ]; then
  if [ "$VERSION" = "latest" ]; then
    BASE="https://github.com/$REPO/releases/latest/download"
  else
    BASE="https://github.com/$REPO/releases/download/$VERSION"
  fi
fi

if command -v curl >/dev/null 2>&1; then
  fetch() { curl -fsSL "$1" -o "$2"; }
elif command -v wget >/dev/null 2>&1; then
  fetch() { wget -q "$1" -O "$2"; }
else
  die "need curl or wget"
fi

if command -v sha256sum >/dev/null 2>&1; then
  sha() { sha256sum "$1" | cut -d' ' -f1; }
elif command -v shasum >/dev/null 2>&1; then
  sha() { shasum -a 256 "$1" | cut -d' ' -f1; }
else
  die "need sha256sum or shasum to verify the download"
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT INT TERM

say "downloading $asset ($VERSION)"
if ! { fetch "$BASE/$asset" "$tmp/$asset" && fetch "$BASE/$asset.sha256" "$tmp/$asset.sha256"; } 2>/dev/null; then
  # Not reachable anonymously (for example a private mirror): use the GitHub CLI when you are signed in with it.
  if [ -z "${DPTY_RELEASE_BASE:-}" ] && command -v gh >/dev/null 2>&1 && gh auth status >/dev/null 2>&1; then
    say "not reachable without signing in, trying the GitHub CLI"
    tagarg=""; [ "$VERSION" = "latest" ] || tagarg="$VERSION"
    # shellcheck disable=SC2086
    gh release download $tagarg -R "$REPO" -p "$asset" -p "$asset.sha256" -D "$tmp" --clobber \
      || die "could not download $asset with the GitHub CLI (is there a release for $target, and do you have access to $REPO?)"
  else
    die "could not download $BASE/$asset (is there a release for $target? Check your connection and https://github.com/$REPO/releases)"
  fi
fi

want="$(cut -d' ' -f1 "$tmp/$asset.sha256" | head -n 1)"
got="$(sha "$tmp/$asset")"
[ -n "$want" ] && [ "$want" = "$got" ] || die "checksum mismatch for $asset (expected '$want', got '$got'): not installing"

tar -xzf "$tmp/$asset" -C "$tmp" dpty || die "the download did not contain the dpty binary"
mkdir -p "$DIR"
install -m 755 "$tmp/dpty" "$DIR/dpty"
say "installed $("$DIR/dpty" --version) to $DIR/dpty"

case ":$PATH:" in
  *":$DIR:"*) ;;
  *) say ""; say "$DIR is not on your PATH. Add it:"; say "  export PATH=\"$DIR:\$PATH\"" ;;
esac
say ""
say "Next: dpty new -n work -- claude   (starts a durable session)"
say "      dpty install                (start the daemon at login)"

#!/usr/bin/env bash
# Writes the Homebrew formula for a release to stdout.
#   scripts/gen-formula.sh v0.1.1 source                 build from the tagged source (works for a private repo, needs Rust)
#   scripts/gen-formula.sh v0.1.1 binary path/to/dist    use the release tarballs (public repo only); dist holds the .sha256 files
set -euo pipefail
tag="${1:?tag, e.g. v0.1.1}"; mode="${2:-source}"; dist="${3:-}"
repo="${DPTY_REPO:-jasonkneen/durable-pty}"
# While the repository is private, Homebrew can only fetch the source over ssh (its sandboxed git has no https credentials).
# Set DPTY_GIT_URL=https://github.com/$repo.git once the repository is public.
git_url="${DPTY_GIT_URL:-ssh://git@github.com/$repo.git}"
version="${tag#v}"
header() { cat <<RUBY
class Dpty < Formula
  desc "Durable PTY (dpty): sessions that outlive your app, as a daemon, CLI and MCP server"
  homepage "https://jasonkneen.github.io/durable-pty/"
  license "MIT"
  version "$version"
RUBY
}
service() { cat <<'RUBY'

  # `brew services start dpty` runs the daemon at login. `dpty install` does the same without Homebrew.
  service do
    run [opt_bin/"dpty", "serve"]
    keep_alive true
    log_path var/"log/dpty.log"
    error_log_path var/"log/dpty.log"
    environment_variables PATH: std_service_path_env
  end

  test do
    assert_match "dpty", shell_output("#{bin}/dpty --version")
    assert_match "not running", shell_output("DPTY_HOME=#{testpath}/home #{bin}/dpty status", 3)
  end
end
RUBY
}
header
if [ "$mode" = source ]; then
  rev="$(git rev-list -n 1 "$tag" 2>/dev/null || git ls-remote "https://github.com/$repo.git" "refs/tags/$tag^{}" | cut -f1)"
  [ -n "$rev" ] || { echo "cannot find the commit for $tag" >&2; exit 1; }
  cat <<RUBY
  url "$git_url", tag: "$tag", revision: "$rev"
  head "$git_url", branch: "main"

  depends_on "rust" => :build

  def install
    system "cargo", "install", *std_cargo_args
  end
RUBY
else
  sha() { cut -d' ' -f1 "$dist/dpty-$1.tar.gz.sha256"; }
  base="https://github.com/$repo/releases/download/$tag"
  cat <<RUBY

  on_macos do
    on_arm do
      url "$base/dpty-aarch64-apple-darwin.tar.gz"
      sha256 "$(sha aarch64-apple-darwin)"
    end
    on_intel do
      url "$base/dpty-x86_64-apple-darwin.tar.gz"
      sha256 "$(sha x86_64-apple-darwin)"
    end
  end
  on_linux do
    on_arm do
      url "$base/dpty-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "$(sha aarch64-unknown-linux-gnu)"
    end
    on_intel do
      url "$base/dpty-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "$(sha x86_64-unknown-linux-gnu)"
    end
  end

  def install
    bin.install "dpty"
  end
RUBY
fi
service

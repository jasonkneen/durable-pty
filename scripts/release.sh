#!/usr/bin/env bash
# Cuts a release: bumps the version, rolls the changelog, tests, commits, tags and pushes. The tag starts the release
# workflow (.github/workflows/release.yml), which builds the binaries, publishes them and updates the Homebrew tap.
#
#   scripts/release.sh 0.1.1            # asks before pushing
#   scripts/release.sh 0.1.1 --yes      # no questions
#   scripts/release.sh 0.1.1 --dry-run  # show what would happen, change nothing
set -euo pipefail
cd "$(dirname "$0")/.."

version="${1:-}"; shift || true
yes=0; dry=0
for a in "$@"; do case "$a" in --yes) yes=1 ;; --dry-run) dry=1 ;; *) echo "unknown option $a" >&2; exit 2 ;; esac; done
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "usage: scripts/release.sh X.Y.Z [--yes] [--dry-run]" >&2; exit 2; }
tag="v$version"

branch="$(git rev-parse --abbrev-ref HEAD)"
[ "$branch" = "main" ] || { echo "release from main (you are on $branch)" >&2; exit 1; }
[ -z "$(git status --porcelain)" ] || { echo "the working tree has uncommitted changes" >&2; exit 1; }
git fetch -q origin main
[ "$(git rev-parse HEAD)" = "$(git rev-parse origin/main)" ] || { echo "main is not in sync with origin/main (pull or push first)" >&2; exit 1; }
git rev-parse -q --verify "refs/tags/$tag" >/dev/null && { echo "tag $tag already exists" >&2; exit 1; }

current="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
echo "release $current -> $version ($tag)"
grep -q '^## Unreleased' CHANGELOG.md || { echo "CHANGELOG.md has no 'Unreleased' section" >&2; exit 1; }
notes="$(awk '/^## Unreleased/{f=1;next} /^## /{f=0} f' CHANGELOG.md | sed '/^$/d')"
[ -n "$notes" ] || { echo "nothing under 'Unreleased' in CHANGELOG.md: write what changed first" >&2; exit 1; }
echo "--- notes:"; echo "$notes"; echo "---"
if [ $dry = 1 ]; then echo "dry run: stopping here"; exit 0; fi

cargo test --locked
./scripts/e2e.sh

# Portable across macOS and GNU sed: rewrite only the first `version = "..."` line, then check it really changed.
perl -0pi -e "s/^version = \"[^\"]*\"/version = \"$version\"/m" Cargo.toml
[ "$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)" = "$version" ] || { echo "could not set the version in Cargo.toml" >&2; exit 1; }
cargo build --release >/dev/null            # refreshes Cargo.lock
[ "$(./target/release/dpty --version)" = "dpty $version" ] || { echo "the built binary does not report $version" >&2; exit 1; }
today="$(date +%Y-%m-%d)"
sed -i.bak "s/^## Unreleased/## Unreleased\n\n## $version ($today)/" CHANGELOG.md && rm -f CHANGELOG.md.bak
git add Cargo.toml Cargo.lock CHANGELOG.md
git commit -q -m "Release $tag"
git tag -a "$tag" -m "Durable PTY $version"

if [ $yes = 0 ]; then
  read -r -p "push main and $tag to origin (this starts the release build)? [y/N] " ans
  [[ "$ans" =~ ^[Yy]$ ]] || { echo "not pushed. To undo: git tag -d $tag && git reset --hard HEAD~1"; exit 0; }
fi
git push origin main "$tag"
echo "pushed. Watch the build: gh run watch -R $(git remote get-url origin | sed -E 's#^.*github.com[:/]##; s#\.git$##')"

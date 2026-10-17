# Releasing Durable PTY (`dpty`)

A release is a version tag. Everything else is automated: `scripts/release.sh` cuts the tag, `.github/workflows/release.yml`
builds and publishes the binaries and updates the Homebrew tap, and `install.sh` / `brew upgrade` pick it up.

## Cut a release

1. Merge your work to `main` and write what changed under `## Unreleased` in `CHANGELOG.md` (it becomes the release notes).
2. `scripts/release.sh 0.1.1 --dry-run` shows the notes and the version change. Then `scripts/release.sh 0.1.1`.
   It checks you are on an up-to-date `main` with a clean tree, runs `cargo test` and `scripts/e2e.sh`, bumps `Cargo.toml` and
   `Cargo.lock`, rolls the changelog, commits "Release v0.1.1", tags `v0.1.1` and, after you confirm, pushes both.
3. The tag starts the release workflow. Watch it: `gh run watch`. It produces, per release:
   - `dpty-aarch64-apple-darwin.tar.gz`, `dpty-x86_64-apple-darwin.tar.gz`, `dpty-aarch64-unknown-linux-gnu.tar.gz`,
     `dpty-x86_64-unknown-linux-gnu.tar.gz`, each with a `.sha256` file, on the GitHub release (notes come from the changelog);
   - an updated `Formula/dpty.rb` in `jasonkneen/homebrew-tap` (see "Homebrew" below).
4. Verify: `curl -fsSL .../install.sh | DPTY_VERSION=v0.1.1 sh && dpty --version`, and `brew update && brew upgrade dpty`.

Use semantic versions. Anything that changes how the daemon talks to a shim (`src/shim.rs`, the frame types) or the database
schema must be called out in the changelog and bump the minor version (the major version once past 1.0): a new daemon must
be able to reattach to shims started by the previous release, because upgrading leaves sessions running.

## Upgrading a machine without losing sessions

Sessions live in their own helper processes, so replacing the daemon does not end them:

```
brew upgrade dpty            # or: curl -fsSL .../install.sh | sh
dpty stop                    # the old daemon exits, sessions keep running
dpty ls                      # the next command starts the new daemon, which reattaches to every session
```

If the daemon runs under launchd (`dpty install`), `dpty stop` is enough: launchd starts the new binary on its own.

## Homebrew

The tap is `jasonkneen/homebrew-tap` (`brew install jasonkneen/tap/dpty`). The release workflow's `homebrew` job writes the
formula with `scripts/gen-formula.sh` and pushes it over ssh with a deploy key: the private half is the repository secret
`TAP_DEPLOY_KEY`, the public half is a read-write deploy key on the tap repository, so the credential can write to the tap and
nothing else. To rotate it: `ssh-keygen -t ed25519 -N "" -f key`, `gh repo deploy-key add key.pub -R jasonkneen/homebrew-tap
--allow-write --title "durable-pty release workflow"`, `gh secret set TAP_DEPLOY_KEY -R jasonkneen/durable-pty < key`, then delete
the key files and the old deploy key. Without the secret the job does
nothing and you update the tap by hand:

```
scripts/gen-formula.sh v0.1.1 binary dist > /path/to/homebrew-tap/Formula/dpty.rb   # the release tarballs (download the .sha256 files into dist/ first)
scripts/gen-formula.sh v0.1.1 source > /path/to/homebrew-tap/Formula/dpty.rb        # build from the tag instead (needs Rust)
```

The workflow writes the prebuilt-binary formula (no build tools needed) for a public repository, and the build-from-source
formula for a private one. If the repository is ever private again, the sandboxed git Homebrew uses has no https credentials,
so set `DPTY_GIT_URL=ssh://git@github.com/jasonkneen/durable-pty.git` when generating the source formula.

## The documentation site

`docs/` is a static site published by GitHub Pages from `main` (Settings, Pages, branch `main`, folder `/docs`). Edit the HTML and
push; it rebuilds in about a minute. Keep the CLI page in step with `src/main.rs` and the install commands in step with `install.sh`.

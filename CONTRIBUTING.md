# Contributing to astscan

Open an issue for a matcher discrepancy or a change to the public API. For
a matcher discrepancy, include the input, rule YAML, the astscan and
ast-grep versions, and both outputs. A pull request for a bug fix should
include the smallest fixture that reproduces the difference.

## Development

Install rustup and a C compiler. The checkout pins the development compiler,
rustfmt, and Clippy in `rust-toolchain.toml`. Rust 1.90 is the minimum supported
version for the published crates.

```sh
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace
```

Use `pkf run ci` for the corresponding local checks in release mode. With
ast-grep 0.45.3 installed, compare the match output on a focused corpus.

```sh
cargo build --locked --release -p astscan
AST_GREP=/path/to/ast-grep node tools/diff.mjs --rule rules.yml --cwd /path/to/corpus
```

The root Node tools use the versions from `mise.toml`. To run the secret scan
that CI runs, use the following commands.

```sh
mise install
mise exec -- pnpm install --frozen-lockfile
git ls-files -z | xargs -0 mise exec -- pnpm exec secretlint --
```

The [experiments](experiments/) have their own setup and checks.

## Release

The crates.io release contains `astscan-core` and its companion `astscan`
CLI. Their manifests define the files in each archive.

### Prepare the release candidate

Update both manifests and the CLI's core dependency to the same version,
then refresh the workspace lockfile. Write a short summary with a runnable
example in `.github/releases/v<version>.md`. Generate `CHANGELOG.md` from
Conventional Commits with git-cliff 2.10 or later. The generator filters
commits that only touch `experiments/`.

```sh
export RELEASE_TAG=v0.1.0
GITHUB_TOKEN="$(gh auth token)" git-cliff --tag "$RELEASE_TAG" --output CHANGELOG.md
node tools/release.mjs "$RELEASE_TAG" /tmp/astscan-release-notes.md
```

Review the generated notes and commit the release files. Run the packaging
checks from a clean checkout of that commit. A separate worktree lets you
verify that commit while keeping local experiment work in progress.

```sh
cargo package --locked -p astscan-core -p astscan
cargo publish --dry-run --locked -p astscan-core -p astscan
```

Review both archives under `target/package/` and check the GitHub CI results
for the release commit before publishing.

### First crates.io release

Create both crates with a short-lived crates.io token that permits creating
new crates. Publish the dependency first from the release commit.

```sh
cargo publish --locked -p astscan-core
cargo publish --locked -p astscan
```

After both versions appear in the index, revoke the initial token. Create
the `release` environment in GitHub and allow version tags to deploy to it.
Register `mizchi/astscan`, workflow `release.yml`, and environment `release`
as a Trusted Publisher on each crate. See the
[crates.io Trusted Publishing guide](https://crates.io/docs/trusted-publishing).

### Tag and publish

Push the version tag on the reviewed release commit. The release workflow
runs the CI checks, validates the tag against both manifests, and assembles
the release page from the summary and changelog. It publishes missing crate
versions in dependency order. For the bootstrapped version, it verifies the
versions in the index and creates the GitHub Release.

```sh
git tag "$RELEASE_TAG"
git push origin "$RELEASE_TAG"
```

After the GitHub Release is created, the binary workflow builds the tagged
source, smoke-tests the executable from each archive, and attaches the
archives and `SHA256SUMS`. The Linux builds use Ubuntu 22.04 to set a glibc
baseline and support dynamic loading of custom grammars.
To add binaries to an existing release, run the same workflow with its tag
from the default branch.

```sh
gh workflow run binaries.yml -f tag="$RELEASE_TAG"
```

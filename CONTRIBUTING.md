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

The release contains the `astscan-core` Rust library and the `astscan` CLI.
Update both manifests and the CLI's core dependency to the same version,
then refresh the workspace lockfile. Commit user-visible changes with
Conventional Commits before generating
`CHANGELOG.md` with git-cliff and write a short summary with a runnable example
in `.github/releases/v<version>.md`.

```sh
export RELEASE_TAG=v0.1.0
GITHUB_TOKEN="$(gh auth token)" git-cliff --tag "$RELEASE_TAG" --output CHANGELOG.md
node tools/release.mjs "$RELEASE_TAG" /tmp/astscan-release-notes.md
cargo package --locked -p astscan-core -p astscan
cargo publish --dry-run --locked -p astscan-core -p astscan
```

Review the generated changelog, the release notes, and both crate archives.
Commit the release files before publishing. Use a clean checkout of that
commit for the first upload and the tag.

### First crates.io release

Create both crates with a short-lived crates.io token that permits creating
new crates. Publish the dependency first from the release commit.

```sh
cargo publish --locked -p astscan-core
cargo publish --locked -p astscan
```

After both versions appear in the index, revoke the initial token. Register
`mizchi/astscan`, workflow `release.yml`, and environment `release` as a
Trusted Publisher on each crate. Create the `release` environment in GitHub
and allow version tags to deploy to it. See the
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

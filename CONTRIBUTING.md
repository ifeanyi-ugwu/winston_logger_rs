# Contributing

## Setup

After cloning, activate the git hooks:

```sh
git config core.hooksPath .githooks
```

This enables the pre-push hook that enforces version hygiene (see below).

## Version convention

When you begin editing a crate, bump its version to `x.y.z-dev` in its `Cargo.toml` before your first commit. This signals that the crate has in-progress changes that have not been released.

```toml
version = "0.3.1-dev"
```

The pre-push hook will block pushes to branches where a crate has source changes but its version is unchanged from `main`.

When the work is ready to ship, the publish workflow handles stripping the `-dev` suffix, tagging, and publishing to crates.io — you do not do this manually.

## Publishing

Releases are triggered via the [Publish workflow](.github/workflows/publish.yml) on GitHub Actions. Select the crate and version level (`patch`, `minor`, `major`, or `release` to finalize a `-dev` version).

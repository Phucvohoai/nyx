# Contributing to Nyx

Thanks for your interest in contributing. Nyx is an early-stage project, so focused bug fixes, tests, documentation improvements, and small architectural changes are especially welcome.

## Before you start

- Search existing issues and pull requests to avoid duplicate work.
- For a large feature or a change to public behavior, open an issue first to discuss the approach.
- Keep changes focused and preserve the separation between `nyx-core`, `nyx-pal`, and `nyx-tui`.
- Do not include credentials, cookies, session vaults, API keys, or personal browsing data in commits or issue reports.

## Development setup

Install a current stable Rust toolchain, clone the repository, and run:

```sh
cargo test --workspace
cargo run -p nyx-tui
```

Search requires a SearXNG JSON endpoint. See the setup instructions in [README.md](README.md).

## Making a change

1. Create a branch for your change.
2. Make the smallest complete change that addresses the issue.
3. Add or update tests for behavior changes and bug fixes.
4. Update user-facing documentation when behavior or setup changes.
5. Run the checks below.
6. Open a pull request describing the motivation, implementation, and validation.

```sh
cargo fmt --all -- --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

## Pull request expectations

- Keep the title concise and action-oriented.
- Explain what changed and why; link related issues where applicable.
- Include test results and note any platform-specific limitations.
- Avoid unrelated refactoring, generated build output, and secrets.
- Be respectful and constructive in review discussions.

By submitting a contribution, you agree that it is provided under the project's [Apache-2.0 license](LICENSE).

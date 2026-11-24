# Contributing to Kerf

Thanks for helping. Bug reports, recordings that trip up song detection (a link and the time
where it goes wrong is enough), and pull requests are all welcome. For anything larger than a
fix, please open an issue first so we can agree on the approach.

## Building and testing

Install Rust (the version is pinned in `rust-toolchain.toml`) and, on Linux, the libraries listed
under [Building](README.md#building). Then:

```sh
cargo run -p kerf                 # the app
cargo test --workspace            # everything
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo deny check                  # licences and advisories (cargo install cargo-deny)
```

CI runs the same checks on Linux, Windows and macOS for every pull request. Workflows from
contributors outside the project start once a maintainer approves them.

## How the code is organised

| Crate | What belongs there |
|---|---|
| `kerf-core` | Everything that isn't UI: the project model, edits and undo, mixing, analysis, decoding and export. Fully unit-tested. |
| `kerf-app` | The egui desktop app and real-time playback. |
| `kerf-cli` | The command-line tool. |
| `kerf-mcp` | The MCP server for AI assistants. |

- **New capabilities go into `kerf_core::ops`** as a request type. The CLI's `run` command and
  the MCP server pick them up from there, so all three stay in step.
- **Edits are pure functions on a `Project`**, run through `Document::apply` so each one is a
  single undo step. Audio is never modified in place.
- **The audio thread must not allocate, lock or wait on I/O.** Rendering happens on the feeder
  thread; the output callback only copies from a ring buffer.

## Tests

- Every fix comes with a test that fails without it.
- Tests generate their audio (sines, noise, synthetic concerts). Please don't commit audio
  files: besides their size, recordings usually belong to someone.
- Prefer checking exact results (sample-exact positions, bit-identical audio) where the code
  promises them, and stated tolerances elsewhere (for example ±0.1 LU for loudness).

## Pull requests

- Keep each pull request to one change, with a message that says what it does and why.
- Add user-visible changes to the README, and tick off or update items in `ROADMAP.md`.
- New dependencies must pass `cargo deny check`: permissive licences (see `deny.toml`) and
  crates.io only.

## Releasing (maintainers)

1. Set `version` in the workspace `Cargo.toml`, run `cargo check` so `Cargo.lock` follows, and
   merge that to `main`.
2. Optionally run the *Release* workflow by hand on `main` (Actions → Release → Run workflow): it
   builds every platform and keeps the archives as workflow artifacts without publishing.
3. Tag and push: `git tag v0.1.0 && git push origin v0.1.0`. The workflow refuses a tag that
   doesn't match `Cargo.toml`.
4. Once the builds pass, approve the `release` environment. The release is then published with
   generated notes, `SHA256SUMS` and attestations. Edit the notes on GitHub afterwards if needed.

## License

Unless you say otherwise, anything you contribute is licensed as Kerf is: under both the MIT and
Apache-2.0 licenses (see [LICENSE-MIT](LICENSE-MIT) and [LICENSE-APACHE](LICENSE-APACHE)).

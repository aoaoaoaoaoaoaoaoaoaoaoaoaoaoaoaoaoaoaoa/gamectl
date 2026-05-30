This is the standalone `gamectl` repository.

It inherits the canonical Rust style doctrine:

- /home/main/programming/projects/rust_starter/docs/rust-style-doctrine.md

Build and test as a single strict Cargo package:

- `cargo fmt --all --check`
- `cargo clippy --all-targets --all-features`
- `cargo test --all-targets --all-features`

Install release binaries into `/home/main/.local/bin`; systemd units and shell
scripts should target that installed path rather than Cargo `target/` artifacts.

Compatibility shims are forbidden. Do not add deprecated aliases, hidden legacy
commands, compatibility wrappers, transitional entry points, or fallback command
grammars. When the CLI shape changes, cut cleanly and update all call sites,
tests, generated launchers, and docs in the same change.

Gone means like it never happened. Do not preserve deleted interfaces in tests,
docs, comments, hidden clap aliases, migration branches, compatibility layers,
or negative assertions that prove the old way is gone. A test that checks some
old grammar no longer works is useless; test only the living interface and let
dead names vanish from the tree.

The host currently exposes an inert `.git` directory here; use `.git-local` as
the real repository store when invoking Git manually:

- `git --git-dir=.git-local --work-tree=. status`

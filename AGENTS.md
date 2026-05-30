This is the standalone `gamectl` repository.

It inherits the canonical Rust style doctrine:

- /home/main/programming/projects/rust_starter/docs/rust-style-doctrine.md

Build and test as a single strict Cargo package:

- `cargo fmt --all --check`
- `cargo clippy --all-targets --all-features`
- `cargo test --all-targets --all-features`

Install release binaries into `/home/main/.local/bin`; systemd units and shell
scripts should target that installed path rather than Cargo `target/` artifacts.

The host currently exposes an inert `.git` directory here; use `.git-local` as
the real repository store when invoking Git manually:

- `git --git-dir=.git-local --work-tree=. status`

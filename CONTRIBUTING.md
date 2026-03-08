# Contributing to Devbox

Devbox is a Rust CLI for managing NixOS-powered developer VMs. This guide covers how to build, test, and contribute to the project.

## Prerequisites

- Rust toolchain (edition 2024)
- Cargo

## Building

```sh
cargo build
```

## Testing

```sh
cargo test
```

## Code Style

All code must pass `rustfmt` and `clippy` before merging.

```sh
cargo fmt --check
cargo clippy -- -D warnings
```

Run `cargo fmt` to auto-format your code before committing.

## Project Structure

| Directory | Purpose |
|---|---|
| `src/cli/` | CLI command definitions and argument parsing (clap) |
| `src/runtime/` | Runtime backends (Lima, Incus, Multipass, Docker) |
| `src/sandbox/` | Sandbox and VM lifecycle management |
| `src/nix/` | NixOS integration and declarative Nix set handling |
| `src/tui/` | Terminal UI components (ratatui) |
| `src/tools/` | Supporting tools and utilities |

## Adding New Features

### New CLI command

1. Add a new module or subcommand definition in `src/cli/`.
2. Register it with the top-level clap command hierarchy.
3. Implement the handler logic, delegating to the appropriate subsystem.

### New runtime backend

1. Add a new module in `src/runtime/` implementing the runtime trait.
2. Register the backend so it can be selected via CLI options or configuration.
3. Add integration tests covering VM creation, start, stop, and teardown.

### New Nix set

1. Add the set definition in `src/nix/`.
2. Ensure it integrates with the existing declarative configuration model.
3. Test that the set is correctly applied when provisioning a VM.

## PR Process

1. Fork the repository and create a feature branch from `main`.
2. Make your changes in small, focused commits.
3. Ensure `cargo test`, `cargo fmt --check`, and `cargo clippy` all pass.
4. Open a pull request against `main` with a clear description of the change.
5. Address review feedback and keep the branch up to date with `main`.

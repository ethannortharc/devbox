# Contributing to Devbox

Thank you for your interest in contributing to Devbox. This guide covers how to build, test, and submit changes.

## Contributor License Agreement

By contributing to this project, you agree that your contributions will be licensed under the Apache License 2.0. If you are contributing on behalf of your employer, you represent that you have the necessary authority to make such contributions and to bind your employer to these terms.

## Prerequisites

- Rust toolchain 1.85+ (edition 2024)
- Cargo
- A VM runtime for end-to-end testing (Lima on macOS, Incus on Linux)

## Building

```bash
cargo build              # Debug build
cargo build --release    # Release build
cargo install --path .   # Install to ~/.cargo/bin/
```

## Testing

```bash
# Unit and integration tests (no VM required)
cargo test

# End-to-end tests (requires a VM runtime)
# See docs/E2E_TEST_GUIDE.md for detailed steps
```

## Code Style

All code must pass `rustfmt` and `clippy` before merging.

```bash
cargo fmt --check
cargo clippy -- -D warnings
```

Run `cargo fmt` to auto-format your code before committing.

## Project Structure

| Directory | Purpose |
|-----------|---------|
| `src/cli/` | CLI command definitions and handlers (clap) |
| `src/runtime/` | Runtime backends (Lima, Incus, Multipass, Docker) |
| `src/sandbox/` | Sandbox lifecycle, state persistence, OverlayFS |
| `src/nix/` | NixOS integration and package set management |
| `src/tui/` | Terminal UI components (ratatui) |
| `src/tools/` | Project detection and tool registry |
| `nix/` | NixOS configuration files (embedded in binary) |
| `nix/sets/` | Package set definitions (.nix files) |
| `layouts/` | Zellij workspace layout definitions (.kdl files) |
| `help/` | Tool cheat sheets (embedded in binary) |
| `configs/` | Tool configurations pushed into VMs |

## Adding New Features

### New CLI command

1. Create a new module in `src/cli/` with a clap `Args` struct and `run()` function.
2. Register the subcommand in `src/cli/mod.rs`.
3. Add tests covering the new command's behavior.

### New runtime backend

1. Implement the `Runtime` trait in a new module under `src/runtime/`.
2. Register it in the detection system (`src/runtime/detect.rs`).
3. Add integration tests for VM creation, start, stop, and teardown.

### New package set

1. Create a `.nix` file in `nix/sets/` defining the packages.
2. Add the `include_str!` reference in `src/sandbox/provision.rs`.
3. Register the set in `generate_state_toml()` and the NixOS module.
4. Add the set to the tool registry in `src/tools/registry.rs`.
5. Test that the set installs correctly during provisioning.

### New Zellij layout

1. Create a `.kdl` file in `layouts/`.
2. Register it in the layout registry (`src/tui/layout_picker.rs`).
3. Use `bash -lc` for command panes to ensure Nix tools are in PATH.

## Pull Request Process

1. Fork the repository and create a feature branch from `main`.
2. Make your changes in small, focused commits.
3. Write tests for new functionality.
4. Ensure `cargo test`, `cargo fmt --check`, and `cargo clippy -- -D warnings` all pass.
5. Open a pull request against `main` with a clear description.
6. Address review feedback promptly.

## Reporting Issues

When reporting bugs, please include:
- Your OS and version
- VM runtime and version (`lima --version`, `incus --version`, etc.)
- The full command you ran
- Complete error output
- The output of `devbox doctor`

## Code of Conduct

Be respectful and constructive. We are here to build good software together.

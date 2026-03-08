# Package Reference

This document catalogs every package available in the devbox project, organized by set. Each set groups related packages for a specific purpose and can be toggled on or off depending on your workflow.

---

## System (24 packages)

Essential POSIX and GNU utilities that form the baseline environment. *Status: always on*

| Package | Description | Homepage |
|---------|-------------|----------|
| coreutils | GNU core utilities (ls, cp, mv, cat, etc.) | https://www.gnu.org/software/coreutils/ |
| gnugrep | GNU implementation of grep for pattern matching | https://www.gnu.org/software/grep/ |
| gnused | GNU stream editor for text transformation | https://www.gnu.org/software/sed/ |
| gawk | GNU implementation of the AWK programming language | https://www.gnu.org/software/gawk/ |
| findutils | GNU find, xargs, and locate utilities | https://www.gnu.org/software/findutils/ |
| diffutils | GNU file comparison utilities (diff, cmp, sdiff) | https://www.gnu.org/software/diffutils/ |
| gzip | GNU compression utility | https://www.gnu.org/software/gzip/ |
| gnutar | GNU tar archiving utility | https://www.gnu.org/software/tar/ |
| xz | General-purpose data compression tool using LZMA2 | https://tukaani.org/xz/ |
| bzip2 | Block-sorting file compressor | https://sourceware.org/bzip2/ |
| file | Determine file type using magic numbers | https://www.darwinsys.com/file/ |
| which | Locate a command on the PATH | https://carlowood.github.io/which/ |
| tree | Recursive directory listing in tree format | https://oldmanprogrammer.net/source.php?dir=projects/tree |
| less | Terminal pager for viewing file contents | https://www.greenwoodsoftware.com/less/ |
| curl | Command-line tool for transferring data with URLs | https://curl.se/ |
| wget | Non-interactive network file retriever | https://www.gnu.org/software/wget/ |
| openssh | Secure shell client and server | https://www.openssh.com/ |
| openssl | Cryptography and TLS toolkit | https://www.openssl.org/ |
| cacert | Mozilla CA certificate bundle | https://curl.se/docs/caextract.html |
| gnupg | GNU Privacy Guard for encryption and signing | https://gnupg.org/ |
| gcc | GNU Compiler Collection (C/C++ compilers) | https://gcc.gnu.org/ |
| gnumake | GNU build automation tool | https://www.gnu.org/software/make/ |
| pkg-config | Helper tool for compiling applications and libraries | https://www.freedesktop.org/wiki/Software/pkg-config/ |
| man-db | Manual page reader and database | https://man-db.nongnu.org/ |

---

## Shell (10 packages)

Terminal multiplexer, shell configuration, prompt, and navigation tools. *Status: on by default*

| Package | Description | Homepage |
|---------|-------------|----------|
| zellij | Terminal workspace and multiplexer with a layout system | https://zellij.dev/ |
| zsh | Z shell with advanced scripting and interactive features | https://www.zsh.org/ |
| zsh-autosuggestions | Fish-like autosuggestions for zsh | https://github.com/zsh-users/zsh-autosuggestions |
| zsh-syntax-highlighting | Syntax highlighting for the zsh shell | https://github.com/zsh-users/zsh-syntax-highlighting |
| starship | Minimal, fast, and customizable cross-shell prompt | https://starship.rs/ |
| fzf | General-purpose command-line fuzzy finder | https://github.com/junegunn/fzf |
| zoxide | Smarter cd command that learns your habits | https://github.com/ajeetdsouza/zoxide |
| direnv | Load and unload environment variables per directory | https://direnv.net/ |
| nix-direnv | Fast, persistent use_nix/use_flake implementation for direnv | https://github.com/nix-community/nix-direnv |
| yazi | Blazing-fast terminal file manager with async I/O | https://yazi-rs.github.io/ |

---

## Tools (21 packages)

Modern command-line replacements and productivity utilities. *Status: on by default*

| Package | Description | Homepage |
|---------|-------------|----------|
| ripgrep | Recursively search directories for a regex pattern | https://github.com/BurntSushi/ripgrep |
| fd | Simple, fast alternative to find | https://github.com/sharkdp/fd |
| bat | Cat clone with syntax highlighting and git integration | https://github.com/sharkdp/bat |
| eza | Modern replacement for ls with colors and icons | https://eza.rocks/ |
| delta | Syntax-highlighting pager for git, diff, and grep output | https://github.com/dandavison/delta |
| sd | Intuitive find-and-replace CLI (sed alternative) | https://github.com/chmln/sd |
| choose | Human-friendly alternative to cut and awk for field selection | https://github.com/theryangeary/choose |
| jq | Lightweight command-line JSON processor | https://jqlang.github.io/jq/ |
| yq-go | YAML, JSON, and XML processor (Go implementation) | https://github.com/mikefarah/yq |
| fx | Terminal JSON viewer and processor | https://fx.wtf/ |
| htop | Interactive process viewer for Unix systems | https://htop.dev/ |
| procs | Modern replacement for ps written in Rust | https://github.com/dalance/procs |
| dust | More intuitive version of du (disk usage) | https://github.com/bootandy/dust |
| duf | Disk usage/free utility with a modern interface | https://github.com/muesli/duf |
| tokei | Count lines of code quickly and accurately | https://github.com/XAMPPRocky/tokei |
| hyperfine | Command-line benchmarking tool | https://github.com/sharkdp/hyperfine |
| tealdeer | Fast tldr client for concise command examples | https://github.com/dbrgn/tealdeer |
| httpie | Human-friendly HTTP client for the command line | https://httpie.io/ |
| dog | Command-line DNS client with colorful output | https://github.com/ogham/dog |
| glow | Render Markdown in the terminal with style | https://github.com/charmbracelet/glow |
| entr | Run arbitrary commands when files change | https://eradman.com/entrproject/ |

---

## Editor (3 packages)

Terminal-based text editors. *Status: on by default*

| Package | Description | Homepage |
|---------|-------------|----------|
| neovim | Hyperextensible Vim-based text editor | https://neovim.io/ |
| helix | Post-modern modal text editor with built-in LSP support | https://helix-editor.com/ |
| nano | Simple and easy-to-use terminal text editor | https://www.nano-editor.org/ |

---

## Git (6 packages)

Version control tools and GitHub integration. *Status: on by default*

| Package | Description | Homepage |
|---------|-------------|----------|
| git | Distributed version control system | https://git-scm.com/ |
| lazygit | Simple terminal UI for git commands | https://github.com/jesseduffield/lazygit |
| gh | GitHub CLI for pull requests, issues, and more | https://cli.github.com/ |
| git-lfs | Git extension for versioning large files | https://git-lfs.com/ |
| git-crypt | Transparent file encryption in git | https://github.com/AGWA/git-crypt |
| pre-commit | Framework for managing git pre-commit hooks | https://pre-commit.com/ |

---

## Container (6 packages)

Container runtime, orchestration, and inspection tools. *Status: off by default*

| Package | Description | Homepage |
|---------|-------------|----------|
| docker | Container engine for building and running applications | https://www.docker.com/ |
| docker-compose | Define and run multi-container Docker applications | https://docs.docker.com/compose/ |
| lazydocker | Simple terminal UI for Docker management | https://github.com/jesseduffield/lazydocker |
| dive | Explore each layer in a Docker image to reduce size | https://github.com/wagoodman/dive |
| buildkit | Concurrent, cache-efficient container build toolkit | https://github.com/moby/buildkit |
| skopeo | Work with remote container images and registries | https://github.com/containers/skopeo |

---

## Network (7 packages)

Network diagnostics, monitoring, and connectivity tools. *Status: off by default*

| Package | Description | Homepage |
|---------|-------------|----------|
| tailscale | Zero-config mesh VPN built on WireGuard | https://tailscale.com/ |
| mosh | Mobile shell that supports roaming and intermittent connectivity | https://mosh.org/ |
| nmap | Network discovery and security auditing tool | https://nmap.org/ |
| tcpdump | Command-line packet analyzer | https://www.tcpdump.org/ |
| bandwhich | Terminal bandwidth utilization tool by process | https://github.com/imsnif/bandwhich |
| trippy | Network diagnostic tool combining traceroute and ping | https://trippy.cli.rs/ |
| doggo | Modern command-line DNS client with DNS-over-HTTPS support | https://doggo.mrkaran.dev/ |

---

## AI (10 packages)

AI coding assistants, local inference, and LLM tooling. *Status: off by default*

| Package | Description | Homepage |
|---------|-------------|----------|
| claude-code | Anthropic's agentic coding assistant for the terminal | https://docs.anthropic.com/en/docs/claude-code |
| aider-chat | AI pair programming in the terminal via LLMs | https://aider.chat/ |
| ollama | Run large language models locally | https://ollama.com/ |
| open-webui | Self-hosted web UI for interacting with LLMs | https://openwebui.com/ |
| codex | OpenAI's CLI coding agent | https://github.com/openai/codex |
| huggingface-hub | CLI and Python library for the Hugging Face Hub | https://huggingface.co/docs/huggingface_hub/ |
| mcp-hub | Central hub for managing Model Context Protocol servers | https://github.com/modelcontextprotocol/servers |
| litellm | Unified interface to call 100+ LLM APIs in OpenAI format | https://litellm.ai/ |
| continue | Open-source AI code assistant IDE extension | https://continue.dev/ |
| opencode | Terminal-based AI coding assistant | https://opencode.ai/ |

---

## lang-go (6 packages)

Go language toolchain and development tools. *Status: enabled by detection*

| Package | Description | Homepage |
|---------|-------------|----------|
| go | The Go programming language compiler and tools | https://go.dev/ |
| gopls | Official Go language server for editor integration | https://pkg.go.dev/golang.org/x/tools/gopls |
| golangci-lint | Fast, configurable Go linters aggregator | https://golangci-lint.run/ |
| delve | Debugger for the Go programming language | https://github.com/go-delve/delve |
| gotools | Supplementary Go tools (goimports, gorename, etc.) | https://pkg.go.dev/golang.org/x/tools |
| gore | Go REPL with line editing and code completion | https://github.com/x-motemen/gore |

---

## lang-rust (6 packages)

Rust language toolchain and development tools. *Status: enabled by detection*

| Package | Description | Homepage |
|---------|-------------|----------|
| rustup | Rust toolchain installer and version manager | https://rustup.rs/ |
| rust-analyzer | Rust language server for IDE features | https://rust-analyzer.github.io/ |
| cargo-watch | Watch Cargo project source and run commands on changes | https://github.com/watchexec/cargo-watch |
| cargo-edit | Cargo subcommands for managing dependencies (add, rm, upgrade) | https://github.com/killercup/cargo-edit |
| cargo-expand | Show the result of macro expansion in Rust code | https://github.com/dtolnay/cargo-expand |
| sccache | Shared compilation cache for C/C++ and Rust | https://github.com/mozilla/sccache |

---

## lang-python (6 packages)

Python language runtime and development tools. *Status: enabled by detection*

| Package | Description | Homepage |
|---------|-------------|----------|
| python 3.12 | Python programming language interpreter (version 3.12) | https://www.python.org/ |
| uv | Extremely fast Python package and project manager | https://github.com/astral-sh/uv |
| ruff | Fast Python linter and formatter written in Rust | https://docs.astral.sh/ruff/ |
| pyright | Fast type checker for Python from Microsoft | https://github.com/microsoft/pyright |
| ipython | Enhanced interactive Python shell | https://ipython.org/ |
| pytest | Full-featured Python testing framework | https://docs.pytest.org/ |

---

## lang-node (6 packages)

Node.js runtime and JavaScript/TypeScript development tools. *Status: enabled by detection*

| Package | Description | Homepage |
|---------|-------------|----------|
| nodejs 22 | JavaScript runtime built on V8 (version 22 LTS) | https://nodejs.org/ |
| bun | All-in-one JavaScript runtime, bundler, and package manager | https://bun.sh/ |
| pnpm | Fast, disk-space-efficient package manager for Node.js | https://pnpm.io/ |
| typescript | Typed superset of JavaScript that compiles to plain JS | https://www.typescriptlang.org/ |
| typescript-language-server | Language Server Protocol implementation for TypeScript | https://github.com/typescript-language-server/typescript-language-server |
| biome | Fast formatter and linter for JavaScript, TypeScript, and more | https://biomejs.dev/ |

---

## lang-java (4 packages)

Java language runtime and build tools. *Status: enabled by detection*

| Package | Description | Homepage |
|---------|-------------|----------|
| jdk 21 | Java Development Kit (version 21 LTS) | https://openjdk.org/ |
| gradle | Build automation tool for multi-language projects | https://gradle.org/ |
| maven | Project management and build tool for Java | https://maven.apache.org/ |
| jdt-language-server | Eclipse JDT Language Server for Java IDE features | https://github.com/eclipse-jdtls/eclipse.jdt.ls |

---

## lang-ruby (4 packages)

Ruby language runtime and development tools. *Status: enabled by detection*

| Package | Description | Homepage |
|---------|-------------|----------|
| ruby 3.3 | Dynamic, open-source programming language (version 3.3) | https://www.ruby-lang.org/ |
| bundler | Dependency manager for Ruby gems | https://bundler.io/ |
| solargraph | Ruby language server with IntelliSense and diagnostics | https://solargraph.org/ |
| rubocop | Ruby static code analyzer and formatter | https://rubocop.org/ |

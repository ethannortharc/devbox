/// Tool set definitions — mirrors the Nix set structure.

#[derive(Debug)]
pub struct ToolSet {
    pub name: &'static str,
    pub description: &'static str,
    pub package_count: usize,
    pub locked: bool,
}

/// All available tool sets.
pub static TOOL_SETS: &[ToolSet] = &[
    ToolSet {
        name: "system",
        description: "OS Foundation (coreutils, gcc, curl, ssh...)",
        package_count: 24,
        locked: true,
    },
    ToolSet {
        name: "shell",
        description: "Terminal & Shell (zellij, zsh, starship, fzf, yazi, micro...)",
        package_count: 11,
        locked: true,
    },
    ToolSet {
        name: "tools",
        description: "Modern CLI (ripgrep, fd, bat, eza, delta, jq, htop, btm...)",
        package_count: 22,
        locked: true,
    },
    ToolSet {
        name: "editor",
        description: "Terminal Editors (neovim, helix, nano)",
        package_count: 3,
        locked: false,
    },
    ToolSet {
        name: "git",
        description: "Git & Collaboration (git, lazygit, gh, git-lfs...)",
        package_count: 6,
        locked: false,
    },
    ToolSet {
        name: "container",
        description: "Container & Virtualization (docker, compose, lazydocker, dive...)",
        package_count: 6,
        locked: false,
    },
    ToolSet {
        name: "network",
        description: "Networking (tailscale, mosh, nmap, tcpdump...)",
        package_count: 7,
        locked: false,
    },
    ToolSet {
        name: "ai-code",
        description: "AI Coding (claude-code, aichat, codex, opencode, aider...)",
        package_count: 5,
        locked: false,
    },
    ToolSet {
        name: "ai-infra",
        description: "AI Infra (ollama, open-webui, litellm, mcp-hub...)",
        package_count: 5,
        locked: false,
    },
    ToolSet {
        name: "lang-go",
        description: "Go Development (go, gopls, golangci-lint, delve...)",
        package_count: 6,
        locked: false,
    },
    ToolSet {
        name: "lang-rust",
        description: "Rust Development (rustup, rust-analyzer, cargo-watch...)",
        package_count: 6,
        locked: false,
    },
    ToolSet {
        name: "lang-python",
        description: "Python Development (python3, uv, ruff, pyright...)",
        package_count: 6,
        locked: false,
    },
    ToolSet {
        name: "lang-node",
        description: "Node.js Development (nodejs, bun, pnpm, typescript...)",
        package_count: 6,
        locked: false,
    },
    ToolSet {
        name: "lang-java",
        description: "Java Development (jdk, gradle, maven, jdtls)",
        package_count: 4,
        locked: false,
    },
    ToolSet {
        name: "lang-ruby",
        description: "Ruby Development (ruby, bundler, solargraph, rubocop)",
        package_count: 4,
        locked: false,
    },
];

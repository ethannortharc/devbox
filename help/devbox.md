# Devbox — Quick Reference

## Everyday Commands
  devbox                Start or attach to sandbox
  devbox stop           Stop sandbox (preserves state)
  devbox diff           Show changes vs host files
  devbox commit         Sync changes back to host

## Workspace
  devbox shell --layout ai-pair    Switch layout
  devbox packages                  Manage tools (TUI)
  devbox exec -- make test         Run one-off command

## Safety
  devbox snapshot save NAME        Create checkpoint
  devbox snapshot restore NAME     Rollback
  devbox discard                   Throw away all changes

## Configuration
  devbox init                      Generate devbox.toml
  devbox config show               Show global defaults
  devbox upgrade --tools rust      Add tools to sandbox

## Troubleshooting
  devbox doctor         Diagnose issues
  devbox status         Detailed sandbox info
  devbox guide <tool>   Tool-specific help

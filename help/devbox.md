# Devbox — Quick Reference

## Everyday Commands
  devbox                Start or attach to sandbox
  devbox stop           Stop sandbox (preserves state)
  devbox destroy        Remove sandbox (warns on uncommitted changes)
  devbox list           List all sandboxes

## Workspace
  devbox shell --layout ai-pair    Switch layout
  devbox use /path/to/project      Remount different directory
  devbox packages                  Manage tools (TUI)
  devbox exec -- make test         Run one-off command

## Layer Management (OverlayFS)
  devbox layer status              Show overlay changes summary
  devbox layer diff                Diff changes vs host
  devbox layer commit              Sync changes back to host
  devbox layer discard             Throw away all changes
  devbox layer stash               Stash current changes
  devbox layer stash-pop           Restore stashed changes

## Layout Management
  devbox layout list               List available layouts
  devbox layout preview NAME       ASCII preview of layout
  devbox layout save               Save layout preference for sandbox
  devbox layout reset              Reset to built-in default layout
  devbox layout create NAME        Create a custom layout
  devbox layout edit NAME          Edit a layout in $EDITOR
  devbox layout set-default NAME   Set global default layout

## Safety
  devbox snapshot save NAME        Create checkpoint
  devbox snapshot restore NAME     Rollback
  devbox diff                      Show changes vs host files
  devbox commit                    Sync changes back to host
  devbox discard                   Throw away all changes

## Configuration
  devbox init                      Generate devbox.toml
  devbox config show               Show global defaults
  devbox upgrade --tools rust      Add tools to sandbox
  devbox reprovision               Re-push configs after update
  devbox nix add <pkg>             Add a Nix package
  devbox nix remove <pkg>          Remove a Nix package

## Troubleshooting
  devbox doctor         Diagnose issues
  devbox status         Detailed sandbox info
  devbox guide <tool>   Tool-specific help
  devbox prune          Remove all stopped sandboxes

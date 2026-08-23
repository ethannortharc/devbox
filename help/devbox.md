# Devbox — Quick Reference

## Everyday Commands
  devbox                Ensure this project has a box, then open the console
  devbox web            Open the console without touching any box
  devbox shell          Attach a terminal (no browser needed)
  devbox stop           Stop sandbox (preserves state)
  devbox destroy        Remove sandbox (warns on uncommitted changes)
  devbox list           List all sandboxes

## Console
  devbox web                       Start the local web console
  devbox web --port 8080           Use a specific port
  devbox web --no-open             Print the URL, do not open a browser

The console is loopback-only and the URL carries a per-launch token. It does
everything the CLI does: manage boxes, browse overlay changes, and open a
terminal in the browser.

## Workspace
  cd /path/to/project && devbox use <name>   Remount a Lima/Incus box to the current directory
  devbox exec -- make test         Run one-off command

## Layer Management (OverlayFS)
  devbox layer status              Show overlay changes summary
  devbox layer diff                Diff changes vs host
  devbox layer commit              Sync changes back to host
  devbox layer discard             Throw away all changes
  devbox layer stash               Stash current changes
  devbox layer stash-pop           Restore stashed changes

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

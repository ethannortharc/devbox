# Devbox — Quick Reference

## Naming a box

Every command that acts on an existing box takes the box name as an optional
first positional argument:

  devbox status              The box registered for the current directory
  devbox status devtest      That box, from anywhere

Leave it out and devbox uses the box registered for the directory you are
standing in, which is why almost every example below has no name in it. Where a
command already has a positional of its own the box comes second, after it:
`devbox snapshot save nightly devtest`, `devbox layer restore 01kfx9 devtest`.

The pre-v5 `--name <NAME>` spelling still works everywhere it used to, so old
scripts keep running, but it no longer appears in `--help`: one argument should
have one spelling. `devbox policy allow` and `devbox report` are the two
exceptions and still show `--name` — the first because its list of entries
leaves no room for another positional, the second because omitting it there
means "search every box for this run" rather than "use this directory's box".

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
  devbox layer refresh             Pick up host-side changes
  devbox layer stash               Stash current changes
  devbox layer stash-pop           Restore stashed changes

## Checkpoints (a saved overlay you can go back to)
  devbox layer checkpoint          Save the overlay as a checkpoint
  devbox layer checkpoint --label base   ...under a name you will recognise
  devbox layer checkpoints         List saved checkpoints
  devbox layer diff --from ID      Diff a checkpoint against the box now
  devbox layer diff --from ID --to ID    ...or against another checkpoint
  devbox layer restore ID          Put the overlay back to a checkpoint
  devbox layer checkpoint-rm ID    Delete a checkpoint

A checkpoint id can be shortened to any unique prefix. `devbox run` takes one
before and after every run, and those are refused by `checkpoint-rm` unless you
pass `--force`, because the run's report cites them for what it changed.

## Safety
  devbox snapshot save SNAP        Create checkpoint
  devbox snapshot restore SNAP     Rollback
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

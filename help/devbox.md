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
  devbox exec -- <cmd>  Run a one-off command in the box
  devbox run -- <cmd>   Run a command and record what it did
  devbox stop           Stop sandbox (preserves state)
  devbox destroy        Remove sandbox (warns on uncommitted changes)
  devbox list           List all sandboxes

## Runs (a command, and the receipt for it)
  devbox run -- <cmd>              Record files, network, processes, credentials
  devbox run --label "…" -- <cmd>  Name the run
  devbox run --posture isolated -- <cmd>   Hold a posture for this run only
  devbox runs                      List this box's recorded runs
  devbox report <RUN_ID>           Print the report (--format md|json|html)
  devbox report <RUN_ID> --open    Open the HTML report in a browser

The summary's last line is the coverage line: which capture backends were live,
how many events were attributed to the run, and how many were dropped. `exec`
and `shell` are recorded as runs too, but do not render a report.

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

## What the box did
  devbox watch                     Recent activity
  devbox watch --type dns,tls      Only these event kinds
  devbox watch --tree              Grouped by process
  devbox behavior summary          Domains, processes, files, traffic, policy
  devbox behavior diff --from T --at T     Compare two windows
  devbox export --run ID --format ocsf     OCSF 1.3, one object per line
  devbox export --format otlp-json         One OTLP/JSON request

## What the box may reach
  devbox policy show               Posture and allowlist
  devbox policy set mirror-only    open | allowlist | mirror-only | isolated
  devbox policy allow github.com   Add to the allowlist
  devbox policy test pypi.org      Exits non-zero if it would be denied
  devbox policy rules              The nftables ruleset the posture generates

## Credentials (they never enter the box)
  devbox secret set anthropic --from-env ANTHROPIC_API_KEY
  devbox secret ls                 Names and backends, never values
  devbox secret scope github --repo owner/name
  devbox secret rm <provider>
  devbox broker status             The host-side broker
  devbox broker reach              How this box reaches it

A host-side broker proxies each request to the real upstream and injects the
credential there. The box holds only a per-box token, rotated at box start, and
every use is recorded as a `credential` event.

## MCP servers, inside a box
  devbox mcp add <name> -- <cmd>   Register a server
  devbox mcp add <name> --global -- <cmd>   ...visible from every directory
  devbox mcp ls                    List registered servers
  devbox mcp run <name>            What the agent launches
  devbox mcp rm <name>             Forget one (its log is kept)

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

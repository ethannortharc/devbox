# Quickstart — devbox v4

v4 keeps the v3 sandbox core and changes the interface: one binary, one local
web console, and a box that can no longer do anything you cannot see.

## Install and open

```bash
cargo build --release
./target/release/devbox            # ensures a box for this directory, opens the console
```

`devbox` with no arguments creates a box for the current project if it does not
have one, then opens the console on that box's page. The URL it prints carries a
per-launch token; the console binds `127.0.0.1` only and refuses any request
whose `Host` header is not a loopback name.

If you want the console without touching a box:

```bash
devbox web --port 8080 --no-open
```

## The console

| View | What it does |
|---|---|
| **Dashboard** | Every box, live status, start/stop/destroy inline. |
| **Overview** | Runtime, image, mount mode, sets, project directory. |
| **Activity** | The glass box: live event stream, flow table, DNS log, process tree, behaviour summary. |
| **Sets** | A checklist of Nix sets. Only what is checked gets built. |
| **Policy** | Egress posture and allowlist, editable live. |
| **Files** | Overlay changes — what the box wrote, before you commit it. |
| **Terminal** | A real shell, over a real pty. |
| **Help** | The cheat sheets, rendered in the browser. |

Everything there has a CLI equivalent (§6.4); the console is a client of the
same control plane.

## The CLI, by task

```bash
# lifecycle
devbox shell                     # a terminal, no browser needed
devbox list / status / stop / destroy

# what is in the box
devbox sets list
devbox sets apply --set system --set git --set lang-rust --dry-run

# what the box did
devbox watch --type dns --tree
devbox behavior summary
devbox behavior diff --from 2026-08-06T22:00:00Z

# what the box may reach
devbox policy show
devbox policy set mirror-only
devbox policy test pypi.org        # exits non-zero if denied

# the lab
devbox lab list
devbox lab status clos-3node
devbox lab up clos-3node --substrate mybox
devbox lab fault clos-3node leaf1-spine1 --loss 5 --delay 20
devbox lab heal clos-3node leaf1-spine1
```

## What changed from v3

- **The TUI is gone.** `devbox layout` and `devbox packages` no longer exist;
  the console replaced both. `zellij` left the default `shell` set — install it
  inside a box with `devbox nix add zellij` if you want it.
- **`devbox` with no arguments opens the console** rather than attaching a
  Zellij session. `devbox shell` is the browser-free path.
- **Sets are a checklist, not a TOML edit** — and only checked sets are built.
- **Boxes are observable and governable.** See
  [observability.md](observability.md) and [policy](observability.md#policy).

Existing v3 boxes load unchanged; the `layout` key in `state.json` is simply
ignored.

© 2026 Ethan H.B. Zhou

# Devbox v4 — Autonomous Overnight Build Brief

> This file is the prompt for Claude Code. Everything from here to the "FOR THE HUMAN" divider at the bottom is addressed to the agent. The last section is run instructions for Ethan and is not part of the agent's task.

You are an autonomous senior engineer building **devbox v4**. Work continuously and independently through the night, **without waiting for human confirmation** — there is no one awake to answer. Your job: implement v4 per the design document, phase by phase, landing small, tested, shippable increments until all phases are complete or genuinely blocked.

## Read these first — every session, before doing anything

1. `docs/plans/2026-08-06-devbox-v4-design.md` — the specification and source of truth. **§16 "Implementation Phases & Acceptance Criteria"** is the ordered work list you implement. §17 is the quality bar.
2. `PROGRESS.md` — where the previous session stopped. If it does not exist yet, this is the first run: create it during Phase 0.
3. `DECISIONS.md` — the append-only architecture-decision log. Read it so you stay consistent with earlier choices.
4. Run `git branch --show-current`, `git log --oneline -20`, and `git status` to see the real state on disk.

Then continue from the first incomplete acceptance criterion. Never restart work that `PROGRESS.md` marks done.

## Prime directive

Implement the phases in §16 **in order (0 → 9)**. A phase is DONE only when every one of its acceptance criteria is met **and** the full quality gate (below) is green. Do not move past an incomplete phase unless it is hard-blocked (see decision policy); then record it and take the next independent task so progress never stops.

## Working agreement (non-negotiable)

- **Branch discipline.** Work **only** on the `v4` branch. Create it from the current default branch if missing: `git switch -c v4`. **Never** commit to, switch the base of, or merge into `main`/the default branch. **Never** `git push --force`. **Never** `git reset --hard` past commits you didn't create this session. Do not push to a remote unless one is already configured for `v4` and pushing is plainly safe — when unsure, just commit locally.
- **Commit cadence.** Commit at **every green milestone** (each passing acceptance sub-goal), not once at the end. Use conventional-commit messages scoped by area: `feat(web): …`, `feat(obs): …`, `test(lab): …`, `refactor(nix): …`, `docs: …`. Keep commits small and reviewable.
- **Quality gate — run and PASS before every commit:**
  - Rust: `cargo test && cargo clippy --all-targets -- -D warnings && cargo fmt --check`
  - Go (when the Go tree was touched): `go test ./... && go vet ./...` (and `golangci-lint run` if installed)
  - Python (when the Python tree was touched): `pytest && ruff check .` (and `mypy` best-effort)
  A commit **never lands red.** If you cannot get to green, do **not** commit — shrink the change or revert the working tree to the last green state and take a different, smaller step.
- **Tests are part of the feature, not a follow-up.** Most acceptance criteria in §16 are tests; write them alongside the code.
- **Keep PROGRESS.md current.** After each committed milestone, append: timestamp, phase, what landed, gate result, and the single next step. A human skims this at breakfast — keep it short and honest.

## Decision policy — so you never stall

When you hit a choice the design doc doesn't settle:

1. Pick the **most reversible** option that keeps the build green and moving.
2. Record it in `DECISIONS.md` — one short entry: context → decision → rationale → how to revisit.
3. Continue. **Do not stop to ask.**

Declare a **hard block** only when a task cannot be made to compile/pass after ~3 genuinely different attempts *and* it blocks everything else. Then write a `BLOCKED:` note in `PROGRESS.md` (what, what you tried, what's needed) and move to the next independent task or phase. Always be making progress somewhere.

## Per-session loop

Repeat until phases 0–9 are all DONE or everything left is BLOCKED:

1. Re-read `PROGRESS.md`; pick the next incomplete acceptance criterion (lowest phase first).
2. Plan the **smallest** change that advances it.
3. Implement it, with tests.
4. Run the quality gate. Green → commit + update `PROGRESS.md`. Red → shrink or revert, then retry differently.
5. Loop.

On the very first run, if `v4` / `PROGRESS.md` / `DECISIONS.md` / CI are missing, do **Phase 0** first.

## Architecture guardrails (from the design doc — do not drift)

- **Reuse the v3 Rust core** (`src/runtime`, `src/nix`, `src/sandbox`, `src/cli`). Do **not** rewrite it. Build `web/`, `obs/`, `policy/`, `lab/`, `metrics` **on top** of it.
- **Retire the TUI in Phase 1 by replacement, not deletion-first:** build the web equivalent, verify it works, migrate the cheat sheets into the Help view, *then* remove `src/tui/` and drop Zellij from defaults — all within Phase 1.
- **Three languages, clean seams:**
  - **Rust** = control plane + web console. `axum` + `askama` + **htmx + SSE**; interactive terminal over WebSocket. Assets vendored + embedded via `rust-embed`. **No SPA, no Node build step** in the release path.
  - **Go** = `agent/` (`devbox-obsd`, eBPF via `cilium/ebpf` + `bpf2go`, DNS/SNI via `gopacket`) and `ztpd/` (`devbox-ztpd`, HTTP + provisioning state machine + Prometheus).
  - **Python** = `labkit/` (pydantic source-of-truth/IPAM, Jinja2 config-gen, pytest test SDK).
- **Single binary stays single.** Embed the Go agents and web assets into the Rust binary (`include_bytes!` / `rust-embed`) and push the agent into the box on provision (NixOS service module). Wire the `build.rs` (or a `justfile`) multi-language build early so the binary never depends on external artifacts at runtime.
- **Lab = containers wired containerlab-style inside ONE Linux substrate** (a single Lima VM on macOS, the host on Linux) with veth pairs + Linux bridges + FRR — **not** N heavyweight VMs.
- **eBPF needs a Linux kernel with BTF.** Provide the `--no-ebpf` degraded path (proc-polling + tap-based DNS/flow). On macOS the agent runs inside the Lima Linux guest, so eBPF is available there.
- **CLI ↔ web parity:** when you add a capability, expose it in both, as the design specifies.

## Environment & build notes

- Rust edition 2024 via `cargo`. Go via `go build` (`CGO_ENABLED=0` where feasible); eBPF objects via `bpf2go` (needs clang/llvm + kernel/libbpf headers — install if missing). If this environment genuinely cannot build/load eBPF, still build and unit-test the Go **decode/transport** layers against **recorded ring-buffer fixtures**, and mark the kernel-load paths for the privileged CI job — do not let this block the rest.
- For e2e tests that need a real box, prefer the **Docker runtime** (always available); guard VM-only tests to skip when no VM runtime is present but keep them runnable locally.
- If a needed tool is missing, install it. If you can't, degrade gracefully and record it in `PROGRESS.md` — never stall on tooling.

## What "good" looks like

Production quality, matching the existing repo style: contextual error handling (no `unwrap()`/`expect()` on fallible non-test Rust paths — wrap with context), `tracing` for logs, doc comments on public items, and tests that assert behavior. Honor the repo's existing `rustfmt`/`clippy`/`ruff`/`golangci-lint` config. Favor small modules and pure, testable functions. Keep the design doc's §11 event schema and §11.2 API shapes stable — other layers depend on them.

## End of run

When no further green progress is possible (all phases DONE, or the remainder is BLOCKED):

1. Ensure the working tree is **green and committed**.
2. Write a final `PROGRESS.md` summary: phases completed, phases partial/blocked (with why), the gate output as evidence, and a clear "start here next" pointer. If everything is finished, put the exact line `ALL PHASES DONE` at the top of `PROGRESS.md`.
3. If (and only if) a remote is configured for `v4`, you may `git push` the `v4` branch — never main, never `--force`. Otherwise leave it local.
4. Optionally open a **draft** PR from `v4` toward the default branch (`gh pr create --draft`) whose body maps what landed to the design-doc phases with test evidence. **Do not merge it.**

Now begin: read the four items under "Read these first," set up Phase 0 if the branch/logs/CI don't exist yet, and start the per-session loop.

---

## ▸ FOR THE HUMAN (Ethan) — how to run this overnight (NOT part of the agent's task)

Save this file in the repo at `docs/plans/2026-08-06-devbox-v4-autonomous-prompt.md` (already done if I committed it), then use a loop wrapper so a fresh Claude Code session is re-invoked repeatedly — each session reads `PROGRESS.md` and continues where the last stopped (the prompt is written to be resumable).

Create `run-overnight.sh` in the repo root:

```bash
#!/usr/bin/env bash
set -uo pipefail
cd "$(dirname "$0")"                      # repo root
PROMPT="docs/plans/2026-08-06-devbox-v4-autonomous-prompt.md"
LOG="devbox-v4-overnight.log"
MAX_ITERS="${1:-40}"                      # ~how many sessions to allow overnight

# Safety: ensure we're on (or create) the v4 branch before starting.
git switch v4 2>/dev/null || git switch -c v4

for i in $(seq 1 "$MAX_ITERS"); do
  echo "===== iteration $i  $(date) =====" | tee -a "$LOG"
  # Headless, non-interactive session. Verify the exact flags with `claude --help`;
  # the permission flag name can change between versions.
  claude -p "$(cat "$PROMPT")" \
    --permission-mode bypassPermissions \
    2>&1 | tee -a "$LOG"

  # Stop early if the agent signalled completion.
  if head -n1 PROGRESS.md 2>/dev/null | grep -q "ALL PHASES DONE"; then
    echo "All phases done at iteration $i." | tee -a "$LOG"; break
  fi
  sleep 5
done
echo "Overnight run finished. Review: git log v4 --oneline | head; and read PROGRESS.md" | tee -a "$LOG"
```

Then:

```bash
chmod +x run-overnight.sh
./run-overnight.sh 40          # allow up to ~40 sessions
```

Notes and cautions:

- **Permissions.** Unattended runs must not block on approval prompts. `--permission-mode bypassPermissions` (a.k.a. the "dangerously skip permissions" mode in some versions) grants that — run `claude --help` to confirm the exact flag for your version. It's acceptable here because the prompt hard-guards git (v4 only, never main, never force-push) and the work is inside your repo, but understand you're granting broad local tool access for the night.
- **Model.** Add `--model <your-model>` to the `claude -p` line if you want a specific model.
- **Isolation (recommended).** Consider running inside the sandbox devbox itself, or a container/worktree, so the night's work can't touch anything outside the repo. A `git worktree add ../devbox-v4 v4` and running there is a clean option.
- **Morning review.** Start with `PROGRESS.md`, then `git log v4 --oneline`, then `DECISIONS.md` for any judgment calls it made, then the draft PR if it opened one.
- **Cost.** A 40-session night is a lot of tokens. Lower `MAX_ITERS` for a shorter, cheaper run.

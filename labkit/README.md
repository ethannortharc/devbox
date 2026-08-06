# devbox-lab (`labkit`)

The Python side of devbox v4: the lab's source of truth, its config generator,
and the pytest SDK that tests assert with.

| Module | What it owns |
|---|---|
| `labkit.sot` | Pydantic models for sites, devices, roles, links, and the address plan; IPAM allocation with overlap detection. A small, opinionated NetBox-in-a-file. |
| `labkit.gen` | Jinja2 templates rendering per-device config (FRR, interfaces, NTP) from intent + SoT, with render → diff → apply → verify. Golden-file tested and idempotent. |
| `labkit.sdk` | pytest fixtures that bring a lab up, drive a scenario, inject faults, and assert on observed behaviour — the observability plane is the test oracle. |

## Development

```bash
cd labkit
uv sync --all-extras --dev
uv run pytest
uv run ruff check . && uv run ruff format --check .
uv run mypy .
```

Full design: [`docs/plans/2026-08-06-devbox-v4-design.md`](../docs/plans/2026-08-06-devbox-v4-design.md) §10.

© 2026 Ethan H.B. Zhou

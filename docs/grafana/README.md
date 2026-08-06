# Grafana dashboard

`devbox-observability.json` is an importable dashboard for the metrics the
console exposes at `/metrics` (§7.7).

## Scraping

The console binds loopback only, so Prometheus has to run on the same host:

```yaml
scrape_configs:
  - job_name: devbox
    static_configs:
      - targets: ["127.0.0.1:7878"]
```

`/metrics` needs no console token — it exposes counts and statuses, never box
contents.

## Importing

Grafana → Dashboards → New → Import → upload the JSON, then pick the
Prometheus data source.

## What to look at first

**`devbox_events_dropped_total`.** §7.3 promises events are never *silently*
dropped; anything above zero means the collector could not keep up and the
timeline has a hole in it. The panel is red at the first dropped event on
purpose.

**`devbox_events_rejected_total`.** Events that did not decode or validate. A
non-zero rate means an agent and the collector disagree about the schema —
almost always a version skew after an upgrade.

**received/s vs stored/s.** A persistent gap between the two is backpressure
building before it becomes drops.

© 2026 Ethan H.B. Zhou

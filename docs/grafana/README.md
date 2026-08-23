# Grafana dashboard

- `devbox-observability.json` covers the collector and box metrics the console
  exposes at `/metrics` (§7.7).
- `devbox-ztp.json` covers the provisioning SLOs emitted by the ZTP operator
  listener: convergence, p95 latency, states, missing devices and retries.

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

The ZTP listener defaults to `127.0.0.1:9090` **inside the service network
namespace**. Run its Prometheus scraper in that namespace, or bind `-metrics`
to a dedicated management address when starting ztpd. The provisioning
listener intentionally does not expose inventory or metrics.

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

On the ZTP dashboard, convergence must become one and p95 must stay below 90
seconds. Missing nodes, failed state, or unknown serials are actionable. An
attempt count above one is expected during a chaos recovery; a value that keeps
growing is not.

© 2026 Ethan H.B. Zhou

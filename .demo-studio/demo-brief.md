# Demo brief

## Release and outcome

- Release/feature: devbox v4 completion.
- Build or commit: `c64bf826173c` plus current working-tree implementation.
- Viewer should understand: a zero-touch fabric is a first-class, inspectable topology in the console.
- Viewer should do next: follow the v4 quickstart or inspect `examples/labs`.

## Audience and destinations

- Primary audience: GitHub README readers.
- Primary destination: repository README.
- Secondary destinations: v4 docs.
- Destination rules: static PNG with readable text and useful alt text.

## Deliverables

| Deliverable | Purpose | Format/aspect | Status |
|---|---|---|---|
| `docs/screenshot-console.png` | Replace the obsolete v3 Zellij screenshot with truthful v4 ZTP topology UI | PNG, 1257×712 | Complete |

## Narrative

- Problem: network behavior is hard to reproduce and inspect in heavyweight fleets.
- Decisive action: open the built-in ZTP fabric in the local console.
- Visible result: service, spine, blank leaves, ASNs and live per-link counters in one graph.
- Why it matters: real Linux namespaces, FRR, faults and ZTP are a first-class product surface.

## Constraints

- Required state: current build, `/labs`, built-in synthetic scenarios only.
- Privacy/security: no project paths, users, tokens, notifications, desktop or browser chrome.
- Accessibility: readable at README width; nearby prose and descriptive alt text.

## Definition of done

- [x] Product truth verified against the running build
- [x] Final PNG visually and technically inspected
- [x] Output inventory and retrospective completed

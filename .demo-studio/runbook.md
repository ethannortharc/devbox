# Capture runbook

## Environment

- Release/campaign key: `v4-completion-2026-08-11`
- Product build: `c64bf826173c` plus current working tree
- Host: macOS; local loopback server
- Recorder: in-app browser page screenshot
- Capture source: semantic page content for `/labs`
- Display/window: normal 1280 × 720 browser viewport
- Audio: none
- Fixture/reset: reload `/labs`; built-in scenarios are immutable
- Raw destination: `.demo-studio/capture/v4-completion-2026-08-11/`
- Publishable destination: `docs/screenshot-console.png`

## Preflight

- [x] Current product build serves `/labs/ztp-fabric`
- [x] Only built-in synthetic fixture data is visible
- [x] Page screenshot excludes notifications, browser chrome and desktop
- [x] Viewport and theme are fixed
- [x] All four frame edges are clean
- [x] Page is fully settled before capture

## Steps

| Step | Mode | Action | Expected state | Recovery/checkpoint |
|---|---|---|---|---|
| 1 | deterministic | Start `devbox web --no-open` on loopback | Authenticated URL printed | Change port only if occupied |
| 2 | interactive | Open the printed URL, navigate through Labs to `ztp-fabric` | Labs nav selected; ZTP graph visible | Reload and inspect visible text |
| 3 | checkpoint | Confirm no project path, token, user data, browser chrome or animation appears in content | Synthetic product-only page | Stop if any private data is visible |
| 4 | deterministic | Capture the normal viewport after the graph settles | High-quality raw JPEG from the browser backend | Retake rather than editing UI state |
| 5 | deterministic | Optimize a derivative and strip metadata | README PNG | Preserve raw capture unchanged |
| 6 | checkpoint | Inspect full image and actual README-scale rendering | Text readable, edges clean, truthful UI | Reject and recapture on any failure |

## Stop conditions

- Any personal box/project information or authentication token appears.
- The Labs page differs from the identified build or fails to settle.
- The screenshot includes browser/desktop chrome or unrelated UI.
- A permission or publishing prompt appears.

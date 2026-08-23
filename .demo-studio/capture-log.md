# Capture log

| Take | Date/time | Build | Recorder | Resolution | Result | Notes/source path |
|---|---|---|---|---|---|---|
| S01-T1 | 2026-08-11 | `c64bf826173c` + working tree | in-app browser | 1440 × 1000 | Rejected | Full-page backend produced a narrow-column canvas with excess whitespace |
| S01-T2/T3 | 2026-08-11 | same | in-app browser | default / oversized | Rejected | Confirmed the defect was limited to full-page capture |
| S01-T4 | 2026-08-11 | same | in-app browser | 1265 × 712 raw | Selected | `.demo-studio/capture/v4-completion-2026-08-11/ztp-S01-T4.jpg` |

## Session preflight evidence

- Release/campaign key: `v4-completion-2026-08-11`
- Recorder source identity: page-content screenshot of loopback `/labs`
- Audio-stream policy: none
- Clean-frame test: passed; no project paths, box selector, token, desktop, browser chrome, or notifications visible

## Selected master

- Shot/take: S01-T4
- Reason: normal-viewport capture renders the full responsive desktop ZTP graph truthfully.
- Known limitation: lower node/link tables and controls are intentionally below the fold; the screenshot makes no claim to show them.
- First/middle/final review: still inspected at original size; title, graph and all five nodes are readable and unclipped.
- Audio verification: not applicable to a still image.

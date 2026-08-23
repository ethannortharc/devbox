# Demo retrospective

## Outcome

- Accepted deliverable: `docs/screenshot-console.png` for the README.
- Publication: repository file only; no external upload or publishing action.

## Production evidence

- Semantic browser navigation and DOM inspection proved the page and synthetic fixture before capture.
- Full-page screenshot mode rendered a false narrow-column canvas despite desktop DOM geometry; three such takes were rejected.
- The truthful fallback was a normal-viewport ZTP detail still, which captured the strongest product outcome without user box data.
- Raw browser output was preserved; the README derivative was converted to PNG, metadata-stripped, palette-optimized and visually re-inspected.

## Classification

- Product-specific: ZTP topology is a stronger README proof than the scenario picker.
- Product-neutral: do not trust full-page capture geometry without inspecting the emitted asset; normal viewport is the safe fallback.
- Shared Demo Studio change: none yet; one backend-specific occurrence is not enough to generalize.

## Follow-up

- [x] No shared change needed

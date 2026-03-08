# Devbox v3 — Node.js language set
{ pkgs }:
with pkgs;
[
  nodejs_22 bun pnpm typescript
  nodePackages.typescript-language-server biome
]

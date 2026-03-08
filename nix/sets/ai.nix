# Devbox v3 — AI set (AI Engines & MCP)
{ pkgs }:
with pkgs;
[
  claude-code aider-chat ollama open-webui
  codex python312Packages.huggingface-hub
  mcp-hub litellm continue opencode
]

# fzf — Fuzzy Finder

## Shell Integration
  Ctrl+t     Paste selected file path
  Ctrl+r     Search command history
  Alt+c      cd into selected directory

## Inside fzf
  ↑↓         Navigate results
  Enter      Select
  Tab        Multi-select (with -m)
  Ctrl+c     Cancel

## Common Patterns
  fzf                          Interactive file picker
  cat file | fzf               Filter lines
  git log --oneline | fzf      Pick a commit
  kill -9 $(ps aux | fzf | awk '{print $2}')

## Useful Options
  --preview 'bat {}'           Preview with syntax highlighting
  --height 40%                 Inline (don't take full screen)
  --multi                      Allow multi-select with Tab
  --exact                      Exact match (no fuzzy)

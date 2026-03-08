# delta — Syntax-Highlighted Diffs

## Usage (automatic via git)
  git diff                 Diffs shown through delta
  git log -p               Patches shown through delta
  git show commit          Commit details through delta

## Standalone
  delta file_a file_b      Compare two files
  diff -u a b | delta      Pipe unified diff

## Features (auto-configured in devbox)
  - Syntax highlighting in diffs
  - Line numbers
  - Side-by-side with -s flag
  - Navigate between files with n/N

## Config (set via git config)
  git config delta.side-by-side true
  git config delta.line-numbers true
  git config delta.navigate true

## Navigation (in pager)
  n / N       Next / previous file
  q           Quit
  /pattern    Search

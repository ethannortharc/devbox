# bat — Syntax-Highlighted File Viewer

## Basic Usage
  bat file.go               View with syntax highlighting
  bat file1 file2            View multiple files
  bat -n file                Show line numbers only (no header)

## Common Flags
  -l go          Force language (for stdin)
  -p             Plain output (no decorations)
  --paging=never No pager
  -A             Show non-printable characters
  -r 10:20       Show only lines 10-20
  --diff         Show git diff markers

## Piping
  cmd | bat -l json          Highlight piped JSON
  bat --style=numbers file   Minimal style
  bat -pp file               Plain, no pager (cat replacement)

## Themes
  bat --list-themes          List available themes
  export BAT_THEME="Dracula" Set default theme

## Integration
  Aliased as: cat → bat --paging=never
  Used by: delta (git diffs), fzf (--preview 'bat {}')

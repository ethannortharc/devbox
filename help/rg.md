# ripgrep (rg) — Fast Search

## Basic Usage
  rg pattern                    Search recursively
  rg pattern file.txt           Search specific file
  rg pattern src/               Search in directory

## Common Flags
  -i           Case insensitive
  -w           Match whole words only
  -l           List files with matches (no content)
  -c           Count matches per file
  -n           Show line numbers (default)
  -C 3         Show 3 lines of context
  -A 2 -B 2   Show 2 lines after/before

## File Filtering
  -t py        Only Python files
  -T js        Exclude JavaScript files
  -g '*.go'    Glob pattern filter
  --hidden     Include hidden files

## Advanced
  -e pat1 -e pat2   Multiple patterns (OR)
  -F 'literal'      Fixed string (no regex)
  -o              Only show matching part
  --json            Machine-readable output
  rg -l TODO | xargs sed -i 's/TODO/DONE/g'

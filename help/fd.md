# fd — Fast File Finder

## Basic Usage
  fd pattern              Find files matching pattern
  fd pattern dir/         Search in specific directory
  fd                      List all files (like find .)

## Common Flags
  -t f         Files only
  -t d         Directories only
  -t l         Symlinks only
  -e go        Filter by extension
  -H           Include hidden files
  -I           No ignore (include .gitignored)
  -d 2         Max depth 2

## Patterns
  fd test                 Files containing "test"
  fd '^main'              Files starting with "main"
  fd '.go$'               Files ending with .go
  fd -g '*.json'          Glob mode

## Actions
  fd -x cmd {}            Execute cmd for each result
  fd -X cmd               Execute cmd with all results
  fd -e tmp -x rm         Delete all .tmp files

## Examples
  fd -e go -x wc -l       Count lines in Go files
  fd Dockerfile            Find all Dockerfiles
  fd -t d node_modules     Find all node_modules dirs

# Neovim — Editor Quick Reference

## Essential
  i          Enter insert mode
  Esc        Return to normal mode
  :w         Save          :q  Quit      :wq  Save & quit
  u          Undo          Ctrl+r  Redo

## Movement
  h j k l    Left/Down/Up/Right
  w b        Next/prev word
  0 $        Start/end of line
  gg G       Top/bottom of file
  Ctrl+d/u   Half-page down/up

## Editing
  dd         Delete line      yy  Copy line
  p          Paste            x   Delete char
  ciw        Change word      ci" Change inside quotes
  >>  <<     Indent/unindent

## Search
  /pattern   Search forward   n  Next match
  ?pattern   Search backward  N  Prev match
  :%s/old/new/g   Replace all

## Splits & Tabs
  :sp        Horizontal split
  :vsp       Vertical split
  Ctrl+w h/j/k/l  Navigate splits
  :tabnew    New tab    gt/gT  Next/prev tab

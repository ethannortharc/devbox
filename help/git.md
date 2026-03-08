# Git — Quick Reference

## Everyday
  git status               Check state
  git add -p               Stage interactively
  git commit -m "msg"      Commit
  git push                 Push to remote
  git pull                 Pull from remote

## Branching
  git branch               List branches
  git checkout -b name     New branch
  git switch name          Switch branch
  git merge name           Merge branch
  git branch -d name       Delete branch

## History
  git log --oneline        Compact log
  git log --graph          Visual branch graph
  git diff                 Unstaged changes
  git diff --staged        Staged changes
  git show commit          Show a commit

## Undo
  git restore file         Discard changes
  git restore --staged f   Unstage file
  git reset --soft HEAD~1  Undo last commit (keep changes)
  git stash                Stash changes
  git stash pop            Restore stash

## Remote
  git remote -v            List remotes
  git fetch                Fetch without merge
  git push -u origin name  Push new branch

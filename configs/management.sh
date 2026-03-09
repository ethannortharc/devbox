#!/usr/bin/env bash
# DevBox Management Panel — interactive menu
set -e

SAVED_LAYOUT="$HOME/.config/devbox/saved-layout.kdl"
OVERLAY_UPPER="/var/devbox/overlay/upper"
OVERLAY_STASH="/var/devbox/overlay/stash"
LOWER="/mnt/host"

# Detect if overlay mode is active
is_overlay() {
    mountpoint -q /workspace 2>/dev/null && [ -d "$OVERLAY_UPPER" ]
}

show_info() {
    clear
    echo ""
    echo "  ╔══════════════════════════════════════╗"
    echo "  ║         DevBox Management            ║"
    echo "  ╚══════════════════════════════════════╝"
    echo ""
    echo "  System:"
    echo "    OS:       $(uname -s) $(uname -m)"
    echo "    Hostname: $(hostname)"
    echo "    Kernel:   $(uname -r)"
    if command -v nix >/dev/null 2>&1; then
        echo "    Nix:      $(nix --version 2>/dev/null)"
    fi
    echo ""
    echo "  Resources:"
    echo "    CPU:      $(nproc) cores"
    echo "    Memory:   $(free -h 2>/dev/null | awk '/Mem:/{print $2}' || echo 'N/A')"
    echo "    Disk:     $(df -h /workspace 2>/dev/null | awk 'NR==2{print $3"/"$2" used"}' || echo 'N/A')"
    echo ""
    echo "  Installed Sets:"
    if [ -f /etc/devbox/devbox-state.toml ]; then
        grep '= true' /etc/devbox/devbox-state.toml | sed 's/^/    [x] /' | sed 's/ = true//'
    fi
    echo ""

    # Layer status
    if is_overlay; then
        echo "  Layer (OverlayFS):"
        local added=0 modified=0 deleted=0
        if [ -d "$OVERLAY_UPPER" ]; then
            while IFS=' ' read -r kind path; do
                [ -z "$kind" ] && continue
                if [ "$kind" = "c" ]; then
                    deleted=$((deleted + 1))
                elif [ -e "$LOWER/$path" ]; then
                    modified=$((modified + 1))
                else
                    added=$((added + 1))
                fi
            done < <(find "$OVERLAY_UPPER" -not -path "$OVERLAY_UPPER" -printf "%y %P\n" 2>/dev/null | grep -v "^d ")
        fi
        if [ $((added + modified + deleted)) -eq 0 ]; then
            echo "    Status:   Clean (no changes)"
        else
            echo "    Changes:  +${added} added, ~${modified} modified, -${deleted} deleted"
        fi
        if [ -d "$OVERLAY_STASH" ] && [ "$(ls -A "$OVERLAY_STASH" 2>/dev/null)" ]; then
            echo "    Stash:    Yes (saved changes)"
        fi
    else
        echo "  Mount Mode: Writable (direct, no overlay)"
    fi
    echo ""

    # Layout status
    echo "  Layout:"
    if [ -f "$SAVED_LAYOUT" ]; then
        echo "    Status:   Custom (saved)"
    else
        echo "    Status:   Built-in default"
    fi
    echo ""
}

show_menu() {
    echo "  ─────────────────────────────────────"
    echo "  Layers:"
    if is_overlay; then
        echo "    s) Layer status      (detailed)"
        echo "    d) Layer diff        (file changes)"
        echo "    c) Layer commit      (sync to host)"
        echo "    x) Layer discard     (throw away changes)"
        echo "    t) Layer stash       (save & clean)"
        echo "    p) Layer stash pop   (restore saved)"
    else
        echo "    (writable mode — changes go directly to host)"
    fi
    echo ""
    echo "  Layout:"
    echo "    1) Save current layout"
    echo "    2) Reset to default layout"
    echo "    3) List available layouts"
    echo ""
    echo "  Other:"
    echo "    4) Tool guides"
    echo "    5) Refresh"
    echo "    q) Exit to shell"
    echo ""
}

# ── Layer actions ──────────────────────────────────────

do_layer_status() {
    echo ""
    echo "  Layer Status:"
    echo "  ─────────────"
    if [ -d "$OVERLAY_UPPER" ]; then
        local count=0
        while IFS=' ' read -r kind path; do
            [ -z "$kind" ] && continue
            if [ "$kind" = "c" ]; then
                echo "    \033[31m-\033[0m $path"
            elif [ -e "$LOWER/$path" ]; then
                echo "    \033[33m~\033[0m $path"
            else
                echo "    \033[32m+\033[0m $path"
            fi
            count=$((count + 1))
        done < <(find "$OVERLAY_UPPER" -not -path "$OVERLAY_UPPER" -not -type d -printf "%y %P\n" 2>/dev/null)
        if [ "$count" -eq 0 ]; then
            echo "    No changes."
        else
            echo ""
            echo "    $count file(s) changed."
        fi
    else
        echo "    Overlay not active."
    fi
    if [ -d "$OVERLAY_STASH" ] && [ "$(ls -A "$OVERLAY_STASH" 2>/dev/null)" ]; then
        echo ""
        echo "    Stash: saved changes available (use 'p' to restore)"
    fi
    echo ""
    read -p "  Press Enter to continue..." _
}

do_layer_diff() {
    echo ""
    echo "  Layer Diff:"
    echo "  ───────────"
    if [ -d "$OVERLAY_UPPER" ]; then
        find "$OVERLAY_UPPER" -not -path "$OVERLAY_UPPER" -not -type d -printf "%P\n" 2>/dev/null | while read -r path; do
            [ -z "$path" ] && continue
            if [ -e "$LOWER/$path" ] && [ -f "$OVERLAY_UPPER/$path" ] && [ -f "$LOWER/$path" ]; then
                echo ""
                echo "  --- a/$path"
                echo "  +++ b/$path"
                diff -u "$LOWER/$path" "$OVERLAY_UPPER/$path" 2>/dev/null | tail -n +3 | head -40 | sed 's/^/  /'
            fi
        done
    fi
    echo ""
    read -p "  Press Enter to continue..." _
}

do_layer_commit() {
    echo ""
    if ! is_overlay; then
        echo "  Not in overlay mode."
        read -p "  Press Enter to continue..." _
        return
    fi

    local count=0
    count=$(find "$OVERLAY_UPPER" -not -path "$OVERLAY_UPPER" -not -type d 2>/dev/null | wc -l)
    if [ "$count" -eq 0 ]; then
        echo "  No changes to commit."
        read -p "  Press Enter to continue..." _
        return
    fi

    echo "  $count file(s) will be synced to host."
    read -p "  Commit? [y/N] " confirm
    if [ "$confirm" != "y" ] && [ "$confirm" != "Y" ]; then
        echo "  Aborted."
        read -p "  Press Enter to continue..." _
        return
    fi

    # Sync upper to lower
    find "$OVERLAY_UPPER" -not -path "$OVERLAY_UPPER" -not -type d -printf "%y %P\n" 2>/dev/null | while IFS=' ' read -r kind path; do
        [ -z "$path" ] && continue
        if [ "$kind" = "c" ]; then
            # Whiteout — delete from lower
            sudo rm -rf "$LOWER/$path" 2>/dev/null
            echo "    - $path"
        else
            # Added/Modified — copy to lower
            local parent
            parent=$(dirname "$LOWER/$path")
            sudo mkdir -p "$parent" 2>/dev/null
            sudo cp -a "$OVERLAY_UPPER/$path" "$LOWER/$path"
            echo "    + $path"
        fi
    done

    # Clear upper layer
    sudo rm -rf "$OVERLAY_UPPER"/* "$OVERLAY_UPPER"/.[!.]* 2>/dev/null
    echo ""
    echo "  Changes committed to host."
    read -p "  Press Enter to continue..." _
}

do_layer_discard() {
    echo ""
    local count=0
    count=$(find "$OVERLAY_UPPER" -not -path "$OVERLAY_UPPER" -not -type d 2>/dev/null | wc -l)
    if [ "$count" -eq 0 ]; then
        echo "  No changes to discard."
        read -p "  Press Enter to continue..." _
        return
    fi

    echo "  $count file(s) will be discarded."
    read -p "  Discard ALL changes? [y/N] " confirm
    if [ "$confirm" != "y" ] && [ "$confirm" != "Y" ]; then
        echo "  Aborted."
        read -p "  Press Enter to continue..." _
        return
    fi

    sudo rm -rf "$OVERLAY_UPPER"/* "$OVERLAY_UPPER"/.[!.]* 2>/dev/null
    echo "  All changes discarded."
    read -p "  Press Enter to continue..." _
}

do_layer_stash() {
    echo ""
    if [ -d "$OVERLAY_STASH" ] && [ "$(ls -A "$OVERLAY_STASH" 2>/dev/null)" ]; then
        echo "  Stash already exists. Pop it first (p) or discard it."
        read -p "  Press Enter to continue..." _
        return
    fi

    local count=0
    count=$(find "$OVERLAY_UPPER" -not -path "$OVERLAY_UPPER" -not -type d 2>/dev/null | wc -l)
    if [ "$count" -eq 0 ]; then
        echo "  No changes to stash."
        read -p "  Press Enter to continue..." _
        return
    fi

    sudo mv "$OVERLAY_UPPER" "$OVERLAY_STASH"
    sudo mkdir -p "$OVERLAY_UPPER"
    echo "  $count file(s) stashed. Workspace is clean."
    read -p "  Press Enter to continue..." _
}

do_layer_stash_pop() {
    echo ""
    if [ ! -d "$OVERLAY_STASH" ] || [ -z "$(ls -A "$OVERLAY_STASH" 2>/dev/null)" ]; then
        echo "  No stash to restore."
        read -p "  Press Enter to continue..." _
        return
    fi

    sudo cp -a "$OVERLAY_STASH"/* "$OVERLAY_UPPER"/ 2>/dev/null
    sudo cp -a "$OVERLAY_STASH"/.[!.]* "$OVERLAY_UPPER"/ 2>/dev/null
    sudo rm -rf "$OVERLAY_STASH"
    echo "  Stash restored."
    read -p "  Press Enter to continue..." _
}

# ── Layout actions ─────────────────────────────────────

do_layout_save() {
    echo ""
    mkdir -p "$(dirname "$SAVED_LAYOUT")"
    if zellij action dump-layout > "$SAVED_LAYOUT" 2>/dev/null; then
        echo "  Layout saved. Will be used on next login."
    else
        echo "  Failed to save layout. Is Zellij running?"
        rm -f "$SAVED_LAYOUT"
    fi
    echo ""
    read -p "  Press Enter to continue..." _
}

do_layout_reset() {
    echo ""
    if [ -f "$SAVED_LAYOUT" ]; then
        rm -f "$SAVED_LAYOUT"
        echo "  Saved layout removed. Next login uses built-in default."
    else
        echo "  No saved layout found. Already using built-in default."
    fi
    echo ""
    read -p "  Press Enter to continue..." _
}

do_layout_list() {
    echo ""
    echo "  Available layouts:"
    echo "    default      AI coding + brainstorm + files"
    echo "    ai-pair      AI assistant + editor + output"
    echo "    fullstack    Frontend + backend + containers"
    echo "    tdd          Editor + auto-running tests"
    echo "    debug        Source + debugger + logs + monitor"
    echo "    monitor      System monitoring dashboard"
    echo "    git-review   Code review: lazygit + diff + PR"
    echo "    presentation Minimal clean mode for demos"
    echo "    plain        No layout, just a shell"
    echo ""
    echo "  Use: devbox shell --layout <name>"
    echo ""
    read -p "  Press Enter to continue..." _
}

do_guides() {
    echo ""
    echo "  Available guides:"
    if [ -d /etc/devbox/help ]; then
        ls /etc/devbox/help/*.md 2>/dev/null | xargs -I{} basename {} .md | sed 's/^/    /'
    fi
    echo ""
    read -p "  Enter tool name (or Enter to go back): " tool
    if [ -n "$tool" ] && [ -f "/etc/devbox/help/$tool.md" ]; then
        if command -v glow >/dev/null 2>&1; then
            glow -p "/etc/devbox/help/$tool.md"
        else
            less "/etc/devbox/help/$tool.md"
        fi
    fi
}

# ── Main loop ──────────────────────────────────────────

while true; do
    show_info
    show_menu
    read -p "  Choice: " choice
    case "$choice" in
        s|S) do_layer_status ;;
        d|D) do_layer_diff ;;
        c|C) do_layer_commit ;;
        x|X) do_layer_discard ;;
        t|T) do_layer_stash ;;
        p|P) do_layer_stash_pop ;;
        1) do_layout_save ;;
        2) do_layout_reset ;;
        3) do_layout_list ;;
        4) do_guides ;;
        5) continue ;;
        q|Q) break ;;
        *) echo "  Invalid choice."; sleep 1 ;;
    esac
done

exec bash

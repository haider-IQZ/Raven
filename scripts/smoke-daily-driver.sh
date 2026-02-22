#!/usr/bin/env bash
set -euo pipefail

echo "[smoke] cargo check"
cargo check

if command -v nix >/dev/null 2>&1; then
  echo "[smoke] nix develop -c cargo check"
  nix develop -c cargo check
else
  echo "[smoke] nix not found, skipping nix develop check"
fi

cat <<'EOF'

[smoke] Manual checklist (mark pass/fail)

[ ] 1. Firefox fullscreen video:
      - Enter fullscreen
      - Switch to another workspace and back
      - Exit fullscreen
      - Expected: no overlap with Waybar

[ ] 2. Per-workspace fullscreen:
      - Keep one fullscreen window on workspace A
      - Keep another fullscreen window on workspace B
      - Switch A <-> B repeatedly
      - Expected: neither drops fullscreen

[ ] 3. Xwayland game flow:
      - Launch Steam, then launch a game
      - Switch workspaces while game is running
      - Close game
      - Expected: no ghost tiles, no stuck focus, no crash

[ ] 4. Native app map/unmap:
      - Launch foot and Nautilus on current workspace
      - Switch away and back
      - Close/reopen once
      - Expected: no invisible spawn, no ghost right-side tile

EOF

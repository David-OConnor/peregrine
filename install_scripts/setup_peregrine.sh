#!/usr/bin/env bash
set -euo pipefail

# Install Peregrine in ~/peregrine and create a desktop entry for the current user.
# Bookmarks and history are shared with Shell in ~/shell_state.ss.

NAME_UPPER="Peregrine"
NAME="peregrine"

SOURCE_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
APP_DIR="$HOME/${NAME}"
DESKTOP_PATH="${XDG_DATA_HOME:-$HOME/.local/share}/applications/${NAME}.desktop"

for file in "$NAME" icon.png; do
  if [[ ! -f "$SOURCE_DIR/$file" ]]; then
    printf 'Error: %s was not found in %s. Extract the release before running this script.\n' \
      "$file" "$SOURCE_DIR" >&2
    exit 1
  fi
done

mkdir -p -- "$APP_DIR" "$(dirname -- "$DESKTOP_PATH")"
for file in "$NAME" icon.png; do
  if [[ ! "$SOURCE_DIR/$file" -ef "$APP_DIR/$file" ]]; then
    cp -- "$SOURCE_DIR/$file" "$APP_DIR/$file"
  fi
done
chmod +x "$APP_DIR/$NAME"

# We create a .desktop file dynamically here; one fewer file to manage.
cat > "$DESKTOP_PATH" <<EOF
[Desktop Entry]
Name=${NAME_UPPER}
Exec="${APP_DIR}/${NAME}"
Icon=${APP_DIR}/icon.png
Path=${APP_DIR}
Type=Application
Terminal=false
Categories=System;FileTools;TerminalEmulator;
Comment=File browser and terminal
EOF

chmod +x "$DESKTOP_PATH"

printf '\n%s is installed in %s.\n' "$NAME_UPPER" "$APP_DIR"
printf 'Bookmarks and history are shared with Shell in ~/shell_state.ss.\n'
printf 'You can launch %s from your application launcher and/or add it to favorites.\n' "$NAME_UPPER"

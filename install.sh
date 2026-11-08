#!/usr/bin/env bash
# wisplet installer (macOS; also works on Linux).
#
#   ./install.sh                  build in release mode and install to ~/.local/bin
#   ./install.sh --prefix DIR     install into DIR instead
#   ./install.sh --from BINARY    install an already built binary (skips the build)
#   ./install.sh --no-path        never touch your shell configuration
#   ./install.sh --yes            do not ask before adding DIR to your PATH
#   ./install.sh --no-autostart   do not start the daemon automatically at login
#   ./install.sh --features LIST  cargo features to build with (comma separated)
#   ./install.sh --uninstall      remove the binary and the login item (downloads and state are kept)
#
# An installation made when the program was still called tDownloader is replaced: its daemon and
# login item are stopped, its binary removed, and its state directory moved to the new name.
set -euo pipefail

NAME="wisplet"
PREFIX="${HOME}/.local/bin"
FROM=""
FEATURES=""
MODIFY_PATH="ask"   # ask | yes | no
UNINSTALL=0
AUTOSTART=1

die() { echo "error: $*" >&2; exit 1; }

usage() { sed -n '2,14p' "$0" | sed 's/^# \{0,1\}//'; }

while [ $# -gt 0 ]; do
  case "$1" in
    --prefix)    [ $# -ge 2 ] || die "--prefix needs a directory"; PREFIX="$2"; shift 2 ;;
    --from)      [ $# -ge 2 ] || die "--from needs a binary path"; FROM="$2"; shift 2 ;;
    --features)  [ $# -ge 2 ] || die "--features needs a list"; FEATURES="$2"; shift 2 ;;
    --no-path)   MODIFY_PATH="no"; shift ;;
    --yes|-y)    MODIFY_PATH="yes"; shift ;;
    --no-autostart) AUTOSTART=0; shift ;;
    --uninstall) UNINSTALL=1; shift ;;
    -h|--help)   usage; exit 0 ;;
    *)           die "unknown option: $1 (try --help)" ;;
  esac
done

# Expand a leading ~ (a quoted "~/bin" reaches us unexpanded).
case "$PREFIX" in "~"|"~/"*) PREFIX="${HOME}${PREFIX#\~}" ;; esac
TARGET="${PREFIX}/${NAME}"

if [ "$UNINSTALL" = 1 ]; then
  if [ -e "$TARGET" ]; then
    if [ -x "$TARGET" ]; then
      "$TARGET" autostart off >/dev/null 2>&1 || true   # remove the login item
      "$TARGET" stop >/dev/null 2>&1 || true            # stop a running daemon
    fi
    rm -f "$TARGET"
    echo "Removed ${TARGET}"
  else
    echo "Nothing to remove: ${TARGET} does not exist"
  fi
  echo "Your downloads and the state directory (~/.local/share/wisplet) were left untouched."
  exit 0
fi

[ "$(uname -s)" = "Darwin" ] || echo "note: this installer targets macOS; continuing on $(uname -s)."

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

if [ -n "$FROM" ]; then
  [ -f "$FROM" ] || die "binary not found: $FROM"
  SOURCE="$FROM"
else
  command -v cargo >/dev/null 2>&1 || die "Rust is required but 'cargo' was not found.
Install it with:  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"
  echo "Building ${NAME} (release)..."
  (cd "$ROOT" && cargo build --release --bin "$NAME" ${FEATURES:+--features "$FEATURES"})
  SOURCE="${ROOT}/target/release/${NAME}"
  [ -f "$SOURCE" ] || die "build finished but ${SOURCE} is missing"
fi

# Replace an installation made under the previous name (tDownloader), keeping its state.
migrate_legacy() {
  local old_name="tDownloader" found=0 dir old
  local old_state="${HOME}/.local/share/tdownloader"
  local new_state="${WISPLET_HOME:-${XDG_DATA_HOME:-${HOME}/.local/share}/${NAME}}"
  for dir in "$PREFIX" "${HOME}/.local/bin"; do
    old="${dir}/${old_name}"
    if [ -e "$old" ]; then
      found=1
      echo "Found a previous installation (${old}); replacing it."
      "$old" autostart off >/dev/null 2>&1 || true   # its login item
      "$old" stop >/dev/null 2>&1 || true            # its daemon
      rm -f "$old"
    fi
  done
  rm -f "${HOME}/Library/LaunchAgents/com.tdownloader.daemon.plist" 2>/dev/null || true
  if [ -d "$old_state" ] && [ ! -e "$new_state" ]; then
    found=1
    mkdir -p "$(dirname "$new_state")"
    mv "$old_state" "$new_state"
    # session.txt stores percent-encoded absolute paths into the state directory
    if [ -f "${new_state}/session.txt" ]; then
      perl -pi -e 's/share%2Ftdownloader/share%2Fwisplet/g' "${new_state}/session.txt"
    fi
    echo "Moved your torrents and settings: ${old_state} -> ${new_state}"
  fi
  [ "$found" = 1 ] || return 0
}
migrate_legacy

# A running daemon keeps the old binary busy; stop it so the new one takes over cleanly.
if [ -x "$TARGET" ]; then "$TARGET" stop >/dev/null 2>&1 || true; fi

mkdir -p "$PREFIX"
cp "$SOURCE" "${TARGET}.new"
chmod 755 "${TARGET}.new"
mv -f "${TARGET}.new" "$TARGET"          # atomic replace
if [ "$(uname -s)" = "Darwin" ]; then
  xattr -d com.apple.quarantine "$TARGET" 2>/dev/null || true
fi
"$TARGET" --help >/dev/null || die "the installed binary does not run"
echo "Installed ${TARGET}"

# Start the daemon at login (macOS LaunchAgent). Takes effect at the next login.
# If autostart is already on it is left exactly as it is.
if [ "$AUTOSTART" = 1 ] && [ "$(uname -s)" = "Darwin" ]; then
  if status_out="$("$TARGET" autostart status 2>/dev/null)"; then
    echo "Autostart is already enabled; leaving it as it is."
    REAL_TARGET="$(cd "$PREFIX" && pwd -P)/${NAME}"   # the agent records the resolved path
    case "$status_out" in
      *"(binary: ${REAL_TARGET})"*|*"(binary: ${TARGET})"*) ;;
      *) echo "note: it points to a different binary. Run '${NAME} autostart on' if you want it to use ${TARGET}." ;;
    esac
  else
    "$TARGET" autostart on || echo "warning: could not enable autostart; run '${NAME} autostart on' later."
  fi
elif [ "$AUTOSTART" = 1 ]; then
  echo "note: autostart is only available on macOS; skipped."
fi

# Is PREFIX already on PATH?
case ":${PATH}:" in
  *":${PREFIX}:"*) echo "Run it with:  ${NAME}"; exit 0 ;;
esac

LINE="export PATH=\"${PREFIX}:\$PATH\""
case "${SHELL##*/}" in
  zsh)  RC="${HOME}/.zshrc" ;;
  bash) RC="${HOME}/.bash_profile" ;;
  *)    RC="" ;;
esac

if [ "$MODIFY_PATH" = "no" ] || [ -z "$RC" ]; then
  echo
  echo "${PREFIX} is not on your PATH. Add this line to your shell configuration:"
  echo "  ${LINE}"
  exit 0
fi

if [ "$MODIFY_PATH" = "ask" ]; then
  if [ -t 0 ]; then
    printf "%s is not on your PATH. Add it to %s? [Y/n] " "$PREFIX" "$RC"
    read -r answer || answer="n"
    case "$answer" in ""|y|Y|yes|YES) MODIFY_PATH="yes" ;; *) MODIFY_PATH="no" ;; esac
  else
    MODIFY_PATH="no"
  fi
fi

if [ "$MODIFY_PATH" = "yes" ]; then
  if [ -f "$RC" ] && grep -qF "$LINE" "$RC"; then
    echo "${RC} already contains the PATH line."
  else
    printf '\n# Added by the wisplet installer\n%s\n' "$LINE" >> "$RC"
    echo "Added ${PREFIX} to PATH in ${RC}"
  fi
  echo "Open a new terminal (or run: source ${RC}) and then run:  ${NAME}"
else
  echo
  echo "${PREFIX} is not on your PATH. Add this line to ${RC:-your shell configuration}:"
  echo "  ${LINE}"
fi

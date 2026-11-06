#!/usr/bin/env bash
# Stands in for the Windows target's C compiler, assembler and archiver when the Windows lint runs on
# Linux: it writes each output a tool was asked for, empty, and exits 0. clippy links nothing and
# never reads these outputs, so the lint is the one Windows runs, and a build script that runs what
# it compiled fails loudly here.
set -euo pipefail
outputs=()
scan() {
  local next=
  for arg in "$@"; do
    if [ -n "$next" ]; then outputs+=("$arg"); next=; continue; fi
    case "$arg" in
      -o) next=1 ;;
      [-/][Oo][Uu][Tt]:?*) outputs+=("${arg:5}") ;;
      [-/]Fo?*) outputs+=("${arg:3}") ;;
      -o?*) outputs+=("${arg#-o}") ;;
      @?*)
        if [ -f "${arg#@}" ]; then
          read -r -a words <<< "$(tr '\r\n' '  ' < "${arg#@}" | tr -d '"')"
          scan ${words[@]+"${words[@]}"}
        fi ;;
    esac
  done
}
scan "$@"
# `ar <operation> <archive> <members...>`
case "$(basename "$0")" in *ar*) [[ "${1:-}" =~ ^[a-zA-Z]+$ ]] && [ -n "${2:-}" ] && outputs+=("$2") ;; esac
for out in ${outputs[@]+"${outputs[@]}"}; do
  case "$out" in */) mkdir -p "$out" ;; *) mkdir -p "$(dirname "$out")"; : > "$out" ;; esac
done

#!/bin/sh
set -eu

listen="${LISTEN:-0.0.0.0:7700}"
set -- --wayland --listen "$listen"

set -f
set -- "$@" ${PSD_OPTS:-}
set +f

if [ -n "${APP_1:-}" ]; then set -- "$@" --spawn "$APP_1"; fi
if [ -n "${APP_2:-}" ]; then set -- "$@" --spawn "$APP_2"; fi
if [ -n "${APP_3:-}" ]; then set -- "$@" --spawn "$APP_3"; fi
if [ -n "${APP_4:-}" ]; then set -- "$@" --spawn "$APP_4"; fi
if [ -n "${APP_5:-}" ]; then set -- "$@" --spawn "$APP_5"; fi
if [ -n "${APP_6:-}" ]; then set -- "$@" --spawn "$APP_6"; fi
if [ -n "${APP_7:-}" ]; then set -- "$@" --spawn "$APP_7"; fi
if [ -n "${APP_8:-}" ]; then set -- "$@" --spawn "$APP_8"; fi

exec /usr/local/bin/psd "$@"

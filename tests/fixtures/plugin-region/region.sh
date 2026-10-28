#!/bin/sh
# Minimal region process: record its pid when asked, print a marker, then stay alive until stopped.
if [ -n "${REGION_FIXTURE_PIDFILE:-}" ]; then
  echo $$ > "$REGION_FIXTURE_PIDFILE"
fi
printf 'region fixture %s\n' "${HERDR_PLUGIN_ENTRYPOINT_ID:-unknown}"
while :; do
  sleep 1
done

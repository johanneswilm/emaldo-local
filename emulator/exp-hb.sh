#!/bin/sh
# screen_state heartbeat (mimic a live mcu driver refreshing the file)
pkill -f "screen-state-hear[t]" 2>/dev/null
setsid sh -c "while true; do printf V1.0.0 > /run/screen_state.tmp; mv /run/screen_state.tmp /run/screen_state; sleep 2; done" </dev/null >/dev/null 2>&1 &
# ensure a single manual app instance (systemd unit is failed/stopped)
sleep 2
for p in /proc/[0-9]*; do
  c=$(cat $p/comm 2>/dev/null)
  case "$c" in emaldo*) kill -9 ${p#/proc/} 2>/dev/null;; esac
done
sleep 1
setsid sh -c "exec /app/emaldo_iot_fw" </dev/null >/tmp/app_hb.log 2>&1 &
exit 0

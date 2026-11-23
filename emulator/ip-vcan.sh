#!/bin/sh
# Emaldo emulator: install as /usr/local/sbin/ip (ahead of /usr/sbin on PATH).
# canapp and the app's CAN monitor re-initialise can0 with
# "ip link set can0 up type can bitrate 500000" (after taking it down). A vcan
# device rejects "type can ...", so can0 stays DOWN and the cabinet disappears.
# Drop the CAN-only arguments and just bring the link up. No forks (see gotcha 14).
read -r t _ < /proc/uptime
case " $* " in
  *" can0 "*) echo "up=${t%%.*} ip $*" >> /var/log/stubs.log ;;
esac
case " $* " in
  *" type can "*) exec /usr/bin/ip link set can0 up ;;
esac
exec /usr/bin/ip "$@"

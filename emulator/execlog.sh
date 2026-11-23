#!/bin/sh
# Emaldo emulator: exec census. Wrap selected binaries with a logging
# passthrough so the app's command surface (including every `/bin/sh -c ...`
# it builds, and the arguments it puts in them) is recorded in
# /var/log/exec.log. Run install.sh, let the app work, then read the log;
# run restore.sh to undo.
#
# SPEED MATTERS (see tool-stub.sh): the app wraps shell calls in ~1 s context
# timeouts, so the shim must not fork - /proc/uptime read, no date, no basename.
#
# Why: the strings in emaldo_iot_fw.bin mention /bin/sh x12, dhclient x26,
# route x19, wpa_cli x8, pkill, iw, ifconfig, insmod - i.e. a real exec surface.
# The open question (Route A) is whether any cloud-supplied value reaches a
# command line or a file path; the log answers that without patching anything.
set -u
REAL=/usr/lib/execlog-real
LOG=/var/log/exec.log
# NOTE: do NOT wrap sh/dash. The wrapper's own `#!/bin/sh` shebang then points at
# the wrapper (ELOOP: "bad interpreter: Too many levels of symbolic links"), and
# every shell script in the image - including the other stubs - stops working.
# The app builds its commands through /bin/bash -c (verified in the saved
# network trace), so wrapping bash captures the command strings.
TARGETS="bash dhclient wpa_cli iw ip route ping pkill date logger sync tar gzip
hostname insmod lsmod rm basename mount umount killall iwconfig wpa_supplicant crontab udevadm"

case "${1:-install}" in
  install)
    mkdir -p "$REAL"
    for n in $TARGETS; do
      src=$(command -v "$n" 2>/dev/null) || continue
      [ -x "$src" ] || continue
      case "$src" in
        "$REAL"*) continue;;
      esac
      if [ ! -e "$REAL/$n" ]; then
        cp -fL "$src" "$REAL/$n" 2>/dev/null || continue
        chmod 755 "$REAL/$n"
        cat > "$src.new" <<EOF
#!/bin/sh
read -r t _ < /proc/uptime
echo "up=\${t%%.*} $n \$@" >> $LOG
exec $REAL/$n "\$@"
EOF
        chmod 755 "$src.new"
        # rename, not truncate: running processes keep the old inode, and
        # `cat > busy-binary` fails with ETXTBSY (that is how /bin/sh escaped
        # the census the first time)
        mv -f "$src.new" "$src"
        echo "wrapped $n ($src)"
      fi
    done
    ;;
  restore)
    for n in $TARGETS; do
      [ -e "$REAL/$n" ] || continue
      src=$(command -v "$n" 2>/dev/null) || src=/usr/bin/$n
      cp -f "$REAL/$n" "$src" && chmod 755 "$src" && echo "restored $n"
    done
    ;;
  status)
    for n in $TARGETS; do
      src=$(command -v "$n" 2>/dev/null) || continue
      if head -1 "$src" 2>/dev/null | grep -q "$REAL"; then echo "wrapped: $n"; fi
    done
    wc -l < "$LOG" 2>/dev/null
    ;;
  stub)
    # The VM has no wifi/dhcp tooling (networkd instead of dhclient, no ping, no
    # route, no wpa_cli), yet the firmware's strings reference them and its
    # netmonitor is built around them. Logging-only stubs answer two questions
    # at once: does the app try to run them, and with which arguments. Later the
    # stubs can be given plausible behaviour; first get the invocations.
    mkdir -p /usr/sbin /usr/bin
    for n in dhclient udhcpc wpa_cli wpa_supplicant iw iwconfig route ping pppd ip-up; do
      for d in /usr/sbin /usr/bin; do
        [ -e "$d/$n" ] && continue
        cat > "$d/$n" <<EOF
#!/bin/sh
read -r t _ < /proc/uptime
echo "up=\${t%%.*} STUB $d/$n \$@" >> $LOG
exit 0
EOF
        chmod 755 "$d/$n"
        echo "stubbed $d/$n"
      done
    done
    ;;
esac

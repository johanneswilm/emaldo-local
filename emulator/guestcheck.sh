#!/bin/sh
# Emaldo emulator: one-shot guest status for the cloud-stage investigation.
# Push with vmput.sh, run as: /usr/local/bin/guestcheck.sh [seconds-of-boot]
#   keeps the app running that long before sampling.
SEC=${1:-45}
LOG=/var/log/app.log

printf "== config.toml\n"; cat /root/config.toml 2>/dev/null
printf "== identity files\n"
for f in .device_id .model .regtoken; do
  printf "  %s = [%s]\n" "$f" "$(cat /root/$f 2>/dev/null | tr -d "\n")"
done
printf "== filedb\n"; ls -l /root/filedb 2>/dev/null | tail -4
printf "== sandbox api log tail\n"; tail -3 /var/log/sandbox_api.log 2>/dev/null | cut -c1-190
printf "== doh query tally\n"
grep -ao "qname=[^ ]*" /var/log/dns_queries.log 2>/dev/null | sort | uniq -c | sort -rn | head -6
printf "== app connections\n"; ss -tnp 2>/dev/null | grep -E "emaldo|SYN-SENT" | head -6
printf "== loggers this boot\n"
python3 - "$LOG" <<'PY'
import json, collections, sys, subprocess, re
log = sys.argv[1]
try:
    out = subprocess.run(["ps", "-C", "emaldo_iot_fw", "-o", "etimes="],
                         capture_output=True, text=True).stdout.split()
    up = int(out[0]) if out else 0
except Exception:
    up = 0
start = None
for line in open("/proc/uptime"):
    start = float(line.split()[0]) - up
    break
c = collections.Counter()
warn = []
for line in open(log, errors="replace"):
    try:
        d = json.loads(line)
    except Exception:
        continue
    ts = d.get("ts", "")
    if not ts:
        continue
    m = re.match(r".*T(\d\d):(\d\d):(\d\d)\.(\d+)([+-]\d\d)(\d\d)", ts)
    if not m:
        continue
    h, mi, s, frac, oh, om = m.groups()
    t = int(h) * 3600 + int(mi) * 60 + int(s) + int(frac) / 1000 \
        - (int(oh) * 60 + int(om)) * 60
    with open("/proc/uptime") as f:
        now = float(f.read().split()[0])
    with open("/etc/localtime", "rb") as f:
        pass
    if start is not None and t % 86400 > (now + 0) % 86400:
        pass
    c[d.get("logger")] += 1
    if d.get("level") in ("ERROR", "WARN") and len(warn) < 8:
        warn.append((ts[11:19], d.get("level"), d.get("logger"),
                     str(d.get("msg"))[:120]))
print(" app uptime(s):", up, "log lines:", sum(c.values()))
print(" loggers:", dict(c))
for w in warn:
    print("  ", *w)
PY
printf "== state runner (pprof)\n"
python3 - <<'PY'
import urllib.request, re
try:
    d = urllib.request.urlopen(
        "http://127.0.0.1:16061/debug/pprof/goroutine?debug=1", timeout=10).read().decode()
    open("/tmp/gor.txt", "w").write(d)
    runners = sorted(set(re.findall(r"staterunners\.\(\*(\w+)\)\.Run", d)))
    print(" goroutines:", d.splitlines()[0], "state runners:", runners)
    print(" subsystems:", sorted(set(re.findall(
        r"bmt-go/internal/(\w+)/\(\*", d)))[:14])
except Exception as e:
    print(" pprof ERR", type(e).__name__, str(e)[:100])
PY
printf "== counters\n"
printf "  api=%s stubs=%s reboots=%s execs=%s\n" \
  "$(cat /var/log/sandbox_api.log 2>/dev/null | wc -l)" \
  "$(wc -l < /var/log/stubs.log 2>/dev/null)" \
  "$(grep -ac reboot /var/log/stubs.log 2>/dev/null)" \
  "$(wc -l < /var/log/exec.log 2>/dev/null)"

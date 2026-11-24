#!/usr/bin/env python3
"""Emaldo emulator: one-shot boot health sample inside the guest.

Waits for the app's pprof endpoint, then reports the four things that decide
whether an experiment is measurable:

  config  - did cfgloader reject /root/config.toml this boot? (a rejected file
            means the app runs on defaults, so the experiment proves nothing)
  loggers - which zap loggers are actually emitting (module logging is gated by
            enable_log_modules, which is a list, not a bool)
  state   - the appwizard state runner from the goroutine dump
  msgs    - the app's own log lines since the restart, for the current boot

Usage: bootcheck.py [wait_seconds] [--no-restart] [--lines N]
"""
import json
import re
import subprocess
import sys
import time
import urllib.request

LOG = "/var/log/app.log"


def app_up():
    out = subprocess.run(["ps", "-C", "emaldo_iot_fw", "-o", "etimes="],
                         capture_output=True, text=True).stdout.split()
    return int(out[0]) if out else None


def pprof(path):
    return urllib.request.urlopen("http://127.0.0.1:16061" + path, timeout=10).read().decode()


def wait_ready(wait, deadline):
    """Sample late: the runners only exist a while after "app started"."""
    time.sleep(wait)
    while time.time() < deadline:
        up = app_up()
        if up is not None:
            try:
                pprof("/debug/pprof/goroutine?debug=1")
                return up
            except Exception:
                pass
        time.sleep(5)
    return app_up()


def boot_lines(since):
    """Log lines from the current boot: everything after the last 'app started'."""
    rows = []
    try:
        data = open(LOG, errors="replace").read().splitlines()
    except Exception:
        return rows
    start = 0
    for i, line in enumerate(data):
        if "app started with version" in line:
            start = i
    for line in data[start:]:
        try:
            rows.append(json.loads(line))
        except Exception:
            continue
    if since is not None:
        rows = [r for r in rows if r.get("caller")]
    return rows


def main():
    wait = int(sys.argv[1]) if len(sys.argv) > 1 else 60
    restart = "--no-restart" not in sys.argv
    lines = 40
    for i, a in enumerate(sys.argv):
        if a == "--lines":
            lines = int(sys.argv[i + 1])
    if restart:
        subprocess.run(["/bin/systemctl", "restart", "emaldo"], check=False)
    deadline = time.time() + wait + 90
    up = wait_ready(wait, deadline)
    rows = boot_lines(None)
    loggers = {}
    cfg_err = []
    for r in rows:
        loggers[r.get("logger")] = loggers.get(r.get("logger"), 0) + 1
        if "config file err" in str(r.get("msg", "")):
            cfg_err.append(str(r.get("msg"))[:180])
    state = "unreachable"
    try:
        d = pprof("/debug/pprof/goroutine?debug=1")
        runners = sorted(set(re.findall(r"staterunners\.\(\*(\w+)\)\.Run", d)))
        state = ",".join(runners) if runners else "none"
    except Exception as exc:
        state = "unreachable:%s" % type(exc).__name__
    print("app_uptime_s=%s boot_log_lines=%d" % (up, len(rows)))
    print("config_errors=%d %s" % (len(cfg_err), cfg_err[:1]))
    print("loggers=%s" % loggers)
    print("state_runner=%s" % state)
    for r in rows[-lines:]:
        print("  %s %-5s %-14s %s" % (str(r.get("ts", ""))[11:19],
                                      r.get("level"), r.get("logger"),
                                      str(r.get("msg"))[:150]))
    return 0


if __name__ == "__main__":
    sys.exit(main())

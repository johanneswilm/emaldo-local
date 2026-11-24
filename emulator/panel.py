#!/usr/bin/env python3
"""Test panel for the emulator: one page and a small JSON API (stdlib only).

Runs inside the guest next to the fakes and only reads and writes what they
already use:
  house      the "var" lines of the tap register file (plant.py)
  SOC        <BAK_SOC_FILE>.set, picked up by fake_bak.py
  commands   e2e_try.py (charge, sell-back, manual selling, override slots)
  readings   the app's spy feed (spy.out), the tap log (setpoints), e2e power flow
  messages   what the app sent "to the cloud": sandbox API log, e2e relay log,
             MQTT broker log, DNS and NTP logs

  GET  /              the page
  GET  /api/state     house variables, setpoints, readings, power flow
  POST /api/plant     {"LOAD1": 500, "PV": 1500, "LINE": 0, "FREQ": 49900, "SOC": 60}
  POST /api/cmd       {"cmd": "charge on"}   (see COMMANDS; "mqtt {json}" publishes
                      to the app's MQTT command topic)
  GET  /api/messages  ?n=300  newest last, repeats folded

usage: panel.py            (PANEL_PORT, default 8088; listens on all addresses)
"""
import http.server, json, os, re, socketserver, subprocess, sys, threading, time

SB = os.environ.get("SB_DIR", "/sandbox")
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import plant

PORT = int(os.environ.get("PANEL_PORT", "8088"))
SOC_SET = os.environ.get("BAK_SOC_FILE", SB + "/bak_soc") + ".set"
E2E_TRY = os.path.join(os.path.dirname(os.path.abspath(__file__)), "e2e_try.py")
INVERTER_PORTS = ("USB0", "USB2", "USB3")
COMMANDS = re.compile(r"^(charge (on|off)|sellback (on|off)|sell (on \d+(\.\d+)?|off)|fuse( \d{1,3})?|"
                      r"slots (clear|\d+|0x[0-9a-fA-F]+)|raw 0x[0-9a-fA-F]+( [0-9a-fA-F]*)?)$")
LOGS = (("api", SB + "/api.log"), ("api", "/var/log/sandbox_api.log"),
        ("e2e", SB + "/relay_live.log"), ("dns", "/var/log/dns_queries.log"),
        ("ntp", "/var/log/ntp_stub.log"), ("mqtt", SB + "/mqtt.log"))


def tail(path, size=400_000):
    try:
        with open(path, "rb") as f:
            f.seek(0, 2)
            n = f.tell()
            f.seek(max(0, n - size))
            lines = f.read().decode("utf-8", "replace").split("\n")
            return lines[1:] if n > size else lines
    except OSError:
        return []


def s16(v):
    return v - 0x10000 if v & 0x8000 else v


def setpoints():
    """Last 0x0FD8..0x0FDD the app wrote to each inverter, from the tap log."""
    out = {p: {} for p in INVERTER_PORTS}
    for line in tail(SB + "/tap.out"):
        port = next((p for p in INVERTER_PORTS if p + " " in line), None)
        if not port:
            continue
        if " WRITE " in line:
            for a, v in re.findall(r"0x(0fd[89a-d])=(\d+)", line):
                out[port][a] = s16(int(v))
        else:
            m = re.search(r": 01100fd800060c((?:[0-9a-f]{4}){6})", line)
            if m:
                for i in range(6):
                    out[port]["0fd%x" % (8 + i)] = s16(int(m.group(1)[4 * i:4 * i + 4], 16))
    return out


def spy():
    """Newest decoded reading per device from the app's spy feed."""
    out = {}
    for line in tail(SB + "/spy.out", 250_000):
        m = re.search(r'\{"channel":"spy".*\}', line)
        if not m or '"cmd":"readdata"' not in line:
            continue
        try:
            p = json.loads(m.group(0).replace("\\'", "'"))["payload"]
            d = p["data"]
            d.pop("RawData", None)
            out[p["name"]] = d
        except (ValueError, KeyError):
            pass
    return out


FLOW = {"t": 0, "data": None}


def flow_loop():
    while True:
        try:
            r = subprocess.run([sys.executable, E2E_TRY, "flow"], capture_output=True, text=True, timeout=30)
            FLOW.update(t=time.time(), data=json.loads(r.stdout.strip().splitlines()[-1]))
        except Exception:
            FLOW.update(data=None)
        time.sleep(8)


def state():
    try:
        switches = [open("/sys/class/mcu/mcu1/inverter_%d" % i).read(1) for i in range(3)]
    except OSError:
        switches = []
    return {"time": time.strftime("%H:%M:%S"), "vars": plant.read(), "setpoints": setpoints(),
            "switches": switches, "spy": spy(), "flow": FLOW["data"],
            "flow_age": round(time.time() - FLOW["t"]) if FLOW["t"] else None}


def messages(n):
    rows = []
    for chan, path in LOGS:
        part, day, prev = [], 0, ""
        for line in tail(path, 300_000):
            if not line.strip():
                continue
            if chan == "api":
                try:
                    r = json.loads(line)
                except ValueError:
                    continue
                t = r.get("t", "")
                path_ = re.sub(r"/[A-Za-z0-9]{32}$", "", r.get("path", ""))
                if "reply" in r:
                    item = ("api", "reply " + path_, json.dumps(r["reply"]))
                elif "path" in r:
                    item = ("api", "call " + path_, r.get("plain") or json.dumps(r.get("decoded", {})))
                else:
                    continue
            elif chan == "mqtt":
                try:
                    r = json.loads(line)
                except ValueError:
                    continue
                t, ev = r.pop("t", ""), r.pop("ev", "?")
                if r.get("client") == "sandbox-pub" and ev != "publish":
                    continue                   # the test publisher's own connects
                who = "sandbox" if r.get("client") == "sandbox-pub" else "app"
                item = ("mqtt", "%s %s %s" % (who, ev, r.get("topic", "")), r.get("payload") if ev == "publish" else json.dumps(r))
            else:
                m = re.match(r"(\d\d:\d\d:\d\d) (.*)", line)
                if not m:
                    continue
                t, text = m.group(1), m.group(2)
                # one summary per kind of line, so keep-alives fold together
                summary = re.sub(r"\([^)]*\)|\b[0-9a-f]{24,}\b|\d+\.\d+\.\d+\.\d+|\b\d{3,}\b|and_\w+", "", text)[:70]
                item = (chan, " ".join(summary.split()), text)
            if t < prev:
                day += 1                       # the logs carry no date: clock went back = next day
            prev = t
            part.append((day, t) + item)
        rows += [(d - day, t, c, s_, det) for d, t, c, s_, det in part]   # newest day = 0
    rows.sort(key=lambda r: r[:2])
    out, last = [], {}
    for _, t, chan, summary, detail in rows:
        row = last.get((chan, summary))
        if row is not None and row in out[-15:]:   # fold repeats, also interleaved ones
            out.remove(row)
            row.update(count=row["count"] + 1, last=t, detail=detail)
        else:
            row = last[(chan, summary)] = {"t": t, "last": t, "chan": chan, "summary": summary,
                                           "detail": detail, "count": 1}
        out.append(row)
    return out[-n:]


def apply_plant(values):
    soc = values.pop("SOC", None)
    clean = {k: int(float(v)) for k, v in values.items() if re.fullmatch(r"[A-Z][A-Z0-9]{0,11}", k)}
    if clean:
        plant.write(clean)
    if soc is not None:
        with open(SOC_SET, "w") as f:
            f.write("%s\n" % max(0.0, min(100.0, float(soc))))
    return plant.read()


def run_cmd(cmd):
    cmd = " ".join(str(cmd).split())
    if re.fullmatch(r"(fcr [1-3]|mfrr (0|11|12)) \d{1,3} \d{1,5}", cmd):    # FCR / mFRR task: kind, minutes, max power W
        r = subprocess.run([sys.executable, os.path.join(os.path.dirname(E2E_TRY), "mqtt_pub.py")] + cmd.split(),
                           capture_output=True, text=True, timeout=30)
        return {"ok": r.returncode == 0, "output": (r.stdout + r.stderr[-600:]).strip()}
    if cmd.startswith("mqtt "):                # JSON payload to iot/<device id>/down/cmd
        try:
            payload = json.dumps(json.loads(cmd[5:]), separators=(",", ":"))
        except ValueError as e:
            return {"ok": False, "output": "not JSON: %s" % e}
        r = subprocess.run([sys.executable, os.path.join(os.path.dirname(E2E_TRY), "mqtt_pub.py"), payload],
                           capture_output=True, text=True, timeout=30)
        return {"ok": r.returncode == 0, "output": (r.stdout + r.stderr[-600:]).strip()}
    if not COMMANDS.match(cmd):
        return {"ok": False, "output": "not a known command: " + cmd}
    r = subprocess.run([sys.executable, E2E_TRY] + cmd.split(), capture_output=True, text=True, timeout=40)
    return {"ok": r.returncode == 0, "output": (r.stdout + r.stderr[-600:]).strip()}


PAGE = r"""<!doctype html><html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1"><title>Emaldo emulator panel</title>
<style>
:root{--bg:#f6f7f8;--card:#fff;--ink:#1c2227;--mute:#667480;--line:#d9dee3;--acc:#0b6bcb;--good:#1a7f4b;--bad:#b3261e}
@media(prefers-color-scheme:dark){:root{--bg:#14181c;--card:#1d2329;--ink:#e6eaee;--mute:#93a0ac;--line:#313a43;--acc:#6db3ff;--good:#5fcf95;--bad:#ff8a80}}
*{box-sizing:border-box}body{margin:0;background:var(--bg);color:var(--ink);font:14px/1.45 system-ui,sans-serif}
main{max-width:1180px;margin:0 auto;padding:16px;display:grid;gap:14px;grid-template-columns:repeat(auto-fit,minmax(330px,1fr))}
h1{font-size:18px;margin:14px 16px 0;max-width:1180px}h1 small{color:var(--mute);font-weight:400;font-size:13px;margin-left:8px}
section{background:var(--card);border:1px solid var(--line);border-radius:8px;padding:12px 14px;min-width:0}
section.wide{grid-column:1/-1}h2{font-size:13px;margin:0 0 8px;color:var(--mute);text-transform:uppercase;letter-spacing:.04em}
table{border-collapse:collapse;width:100%}td,th{padding:3px 6px;text-align:right;border-bottom:1px solid var(--line);font-variant-numeric:tabular-nums}
td:first-child,th:first-child{text-align:left}th{color:var(--mute);font-weight:500}
input[type=number]{width:84px;padding:4px 6px;background:var(--bg);color:var(--ink);border:1px solid var(--line);border-radius:5px;font:inherit}
select,button{padding:5px 10px;background:var(--bg);color:var(--ink);border:1px solid var(--line);border-radius:5px;font:inherit;cursor:pointer}
button.go{background:var(--acc);color:#fff;border-color:var(--acc)}button:hover{border-color:var(--acc)}
.row{display:flex;flex-wrap:wrap;gap:8px;align-items:center;margin:6px 0}.mute{color:var(--mute)}
pre{margin:6px 0 0;white-space:pre-wrap;word-break:break-all;font:12px/1.4 ui-monospace,monospace;max-height:160px;overflow:auto}
#msgs{max-height:460px;overflow:auto}#msgs td{text-align:left;vertical-align:top;font-size:13px}
#msgs td.d{font:12px/1.35 ui-monospace,monospace;word-break:break-all;color:var(--mute)}
.chan{display:inline-block;min-width:34px;padding:0 5px;border-radius:4px;border:1px solid var(--line);font-size:12px;text-align:center}
.big{font-size:20px;font-weight:600}.grid4{display:grid;grid-template-columns:repeat(4,1fr);gap:8px}.grid4 div span{display:block;color:var(--mute);font-size:12px}
.overflow{overflow-x:auto}
</style></head><body>
<h1>Emaldo emulator panel <small id="clock"></small></h1>
<main>
<section><h2>House</h2>
 <table><tr><th></th><th>Phase 1</th><th>Phase 2</th><th>Phase 3</th></tr>
 <tr><td>Load W</td><td><input type="number" id="LOAD1" step="100"></td><td><input type="number" id="LOAD2" step="100"></td><td><input type="number" id="LOAD3" step="100"></td></tr>
 <tr><td>Solar W</td><td><input type="number" id="PV1" step="100"></td><td><input type="number" id="PV2" step="100"></td><td><input type="number" id="PV3" step="100"></td></tr></table>
 <div class="row"><label>Load is on <select id="LINE"><option value="1">the inverter output</option><option value="0">the grid meter side only</option></select></label></div>
 <div class="row"><label>Grid frequency Hz <input type="number" id="FREQ" step="0.01" min="45" max="55"></label></div>
 <div class="row"><label>Battery SOC % <input type="number" id="SOC" min="0" max="100" placeholder="keep"></label>
 <button class="go" onclick="applyHouse()">Apply</button><button onclick="zero()">All zero</button><span id="houseMsg" class="mute"></span></div>
</section>
<section><h2>Commands to the firmware</h2>
 <div class="row"><b>Emergency charge</b><button onclick="cmd('charge on')">On</button><button onclick="cmd('charge off')">Off</button></div>
 <div class="row"><b>Discharge override</b><label>above % <input type="number" id="dis" value="20" min="1" max="100" style="width:64px"></label>
  <button onclick="cmd('slots '+(256-(+dis.value)))">Set</button><button onclick="cmd('slots clear')">Clear</button></div>
 <div class="row"><b>Sell back to grid</b><button onclick="cmd('sellback on')">On</button><button onclick="cmd('sellback off')">Off</button></div>
 <div class="row"><b>Main fuse</b><label>3 x <input type="number" id="fa" value="25" min="6" max="200" style="width:60px"> A</label>
  <button onclick="cmd('fuse '+fa.value)">Set</button><button onclick="cmd('fuse')">Read</button></div>
 <div class="row"><b>Manual selling</b><label>kWh <input type="number" id="kwh" value="5" min="1" style="width:64px"></label>
  <button onclick="cmd('sell on '+kwh.value)">Start</button><button onclick="cmd('sell off')">Stop</button></div>
 <div class="row"><b>Raw</b><input id="rawt" value="0x81" style="width:70px;padding:4px 6px"><input id="rawp" placeholder="payload hex" style="width:150px;padding:4px 6px">
  <button onclick="cmd('raw '+rawt.value+' '+rawp.value)">Send</button></div>
 <div class="row"><b>FCR task</b><select id="fk"><option value="1">FCR-N</option><option value="2">FCR-D down</option><option value="3">FCR-D up</option></select>
  <label>min <input type="number" id="fm" value="3" min="1" style="width:56px"></label><label>W <input type="number" id="fp" value="3000" step="500" style="width:76px"></label>
  <button onclick="cmd('fcr '+fk.value+' '+fm.value+' '+fp.value)">Start</button></div>
 <div class="row"><b>mFRR task</b><select id="mk"><option value="11">mFRR up</option><option value="12">mFRR down</option></select>
  <label>min <input type="number" id="mm" value="15" min="1" style="width:56px"></label><label>W <input type="number" id="mp" value="3000" step="500" style="width:76px"></label>
  <button onclick="cmd('mfrr '+mk.value+' '+mm.value+' '+mp.value)">At next quarter</button><button onclick="cmd('mfrr 0 15 0')">Stop</button></div>
 <div class="row"><b>MQTT command</b><input id="mq" value='{"cmd":"hello"}' style="width:230px;padding:4px 6px">
  <button onclick="cmd('mqtt '+mq.value)">Publish</button></div>
 <pre id="cmdOut" class="mute">Replies appear here.</pre>
</section>
<section><h2>Power flow as the firmware reports it <span id="flowAge" class="mute"></span></h2>
 <div class="grid4" id="flow"></div>
 <p class="mute" style="margin:8px 0 0">Battery: negative = charging. Grid: positive = import.</p>
</section>
<section><h2>Batteries (firmware's reading)</h2><div class="overflow"><table id="bat"></table></div></section>
<section class="wide"><h2>Inverters and grid meter</h2><div class="overflow"><table id="inv"></table></div></section>
<section class="wide"><h2>Messages from the firmware to the "cloud"</h2>
 <div class="row"><label><input type="checkbox" class="f" value="api" checked> API calls</label>
 <label><input type="checkbox" class="f" value="e2e" checked> Command channel</label>
 <label><input type="checkbox" class="f" value="mqtt" checked> MQTT</label>
 <label><input type="checkbox" class="f" value="dns" checked> DNS</label>
 <label><input type="checkbox" class="f" value="ntp" checked> Time</label>
 <label><input type="checkbox" id="replies"> show the sandbox's replies</label>
 <label><input type="checkbox" id="pause"> pause</label></div>
 <div id="msgs"><table id="msgT"></table></div>
</section>
</main>
<script>
const $=id=>document.getElementById(id), esc=s=>String(s).replace(/[&<>]/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;'}[c]));
let filled=false;
async function j(url,body){const r=await fetch(url,body?{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify(body)}:{});return r.json()}
function fillVars(v){for(const k of ['LOAD1','LOAD2','LOAD3','PV1','PV2','PV3'])$(k).value=v[k]??0;$('LINE').value=String(v.LINE??1);$('FREQ').value=((v.FREQ??50000)/1000).toFixed(2)}
async function applyHouse(){const b={LINE:+$('LINE').value,FREQ:Math.round(+$('FREQ').value*1000)||50000};for(const k of ['LOAD1','LOAD2','LOAD3','PV1','PV2','PV3'])b[k]=+$(k).value||0;
 if($('SOC').value!=='')b.SOC=+$('SOC').value;const v=await j('/api/plant',b);fillVars(v);$('SOC').value='';$('houseMsg').textContent='applied '+new Date().toLocaleTimeString()}
function zero(){for(const k of ['LOAD1','LOAD2','LOAD3','PV1','PV2','PV3'])$(k).value=0;applyHouse()}
async function cmd(c){$('cmdOut').textContent='sending: '+c+' ...';const r=await j('/api/cmd',{cmd:c.trim()});$('cmdOut').textContent=c+'\n'+r.output}
function row(name,cells){return '<tr><td>'+name+'</td>'+cells.map(c=>'<td>'+(c??'')+'</td>').join('')+'</tr>'}
async function refresh(){
 let s;try{s=await j('/api/state')}catch(e){$('clock').textContent='panel not reachable';return}
 $('clock').textContent='guest time '+s.time;if(!filled){fillVars(s.vars);filled=true}
 const f=s.flow;$('flowAge').textContent=f?'('+s.flow_age+' s ago)':'(no answer from the firmware)';
 $('flow').innerHTML=f?[['Solar',f.solar_w],['Battery',f.battery_w],['Grid',f.grid_w],['Load',-(f.addition_load_w+f.other_load_w)]].map(x=>'<div><span>'+x[0]+'</span><b class="big">'+x[1]+' W</b></div>').join('')+
   '<div><span>on inverter output</span>'+(-f.other_load_w)+' W</div><div><span>on meter side</span>'+(-f.addition_load_w)+' W</div><div><span>EV</span>'+f.ev_w+' W</div>':'';
 const ports=['USB0','USB2','USB3'],sp=s.setpoints,y=s.spy,iv=i=>y['inverter/'+i]||{},b=y.bsensor||{};
 $('inv').innerHTML='<tr><th></th><th>Inverter 1</th><th>Inverter 2</th><th>Inverter 3</th></tr>'+
  row('Switch (MCU file)',s.switches.map(x=>x==='1'?'on':'off'))+
  row('Setpoint 0x0FD8 W',ports.map(p=>sp[p]['0fd8']))+row('Second setpoint 0x0FDD W',ports.map(p=>sp[p]['0fdd']))+
  row('PowerOnGrid',[0,1,2].map(i=>iv(i).PowerOnGrid))+row('PowerOnBattery',[0,1,2].map(i=>iv(i).PowerOnBattery))+
  row('PowerOnMPPT',[0,1,2].map(i=>iv(i).PowerOnMPPT))+row('PowerOnLine',[0,1,2].map(i=>iv(i).PowerOnLine))+
  row('Grid meter power W',[b.Power1,b.Power2,b.Power3])+row('Grid meter total W',[b.TotalPower,'','']);
 const packs=Object.keys(y).filter(k=>k.startsWith('battery/')).sort((a,c)=>a.split('/')[1]-c.split('/')[1]);
 $('bat').innerHTML='<tr><th>Pack</th><th>SOC %</th><th>Current A</th><th>Voltage V</th><th>Charging</th></tr>'+
  packs.map(k=>row(k.split('/')[1],[y[k].SOC,(y[k].Current/1000).toFixed(2),(y[k].Voltage/1000).toFixed(2),y[k].IsMOSCharging?'allowed':'off'])).join('');
}
async function refreshMsgs(){
 if($('pause').checked)return;const on=[...document.querySelectorAll('.f')].filter(c=>c.checked).map(c=>c.value),rep=$('replies').checked;
 const m=(await j('/api/messages?n=400')).filter(x=>on.includes(x.chan)&&(rep||!x.summary.startsWith('reply ')));
 $('msgT').innerHTML=m.reverse().map(x=>'<tr><td>'+x.last+'</td><td><span class="chan">'+x.chan+'</span></td><td>'+esc(x.summary)+(x.count>1?' <span class="mute">x'+x.count+' since '+x.t+'</span>':'')+'</td><td class="d">'+esc(x.detail).slice(0,900)+'</td></tr>').join('');
}
refresh();refreshMsgs();setInterval(refresh,3000);setInterval(refreshMsgs,5000);
</script></body></html>
"""


class H(http.server.BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def send(self, body, ctype="application/json", code=200):
        if not isinstance(body, bytes):
            body = json.dumps(body).encode()
        self.send_response(code)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", "no-store")
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        path, _, query = self.path.partition("?")
        if path == "/":
            self.send(PAGE.encode(), "text/html; charset=utf-8")
        elif path == "/api/state":
            self.send(state())
        elif path == "/api/messages":
            m = re.search(r"n=(\d+)", query)
            self.send(messages(min(2000, int(m.group(1))) if m else 300))
        else:
            self.send({"error": "not found"}, code=404)

    def do_POST(self):
        try:
            body = json.loads(self.rfile.read(int(self.headers.get("Content-Length") or 0)) or b"{}")
            if self.path == "/api/plant":
                self.send(apply_plant(body))
            elif self.path == "/api/cmd":
                self.send(run_cmd(body.get("cmd", "")))
            else:
                self.send({"error": "not found"}, code=404)
        except Exception as e:
            self.send({"error": repr(e)}, code=400)


class S(socketserver.ThreadingMixIn, socketserver.TCPServer):
    daemon_threads = True
    allow_reuse_address = True


if __name__ == "__main__":
    threading.Thread(target=flow_loop, daemon=True).start()
    with S(("0.0.0.0", PORT), H) as srv:
        srv.serve_forever()

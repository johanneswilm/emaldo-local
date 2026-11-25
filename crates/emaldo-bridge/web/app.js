// emaldo-local operator UI. No external assets: everything is served by the
// bridge itself, so the page works with the battery off the internet.
"use strict";

const H = { "content-type": "application/json", "x-emaldo-local": "1" };
const $ = (id) => document.getElementById(id);
const esc = (s) => String(s).replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" }[c]));

async function api(path, body) {
  const opt = body === undefined ? {} : { method: "POST", headers: H, body: JSON.stringify(body) };
  const r = await fetch(path, opt);
  if (r.status === 401) { location = "/"; throw new Error("signed out"); }
  let j = null;
  try { j = await r.json(); } catch (e) { j = { ok: false, error: "HTTP " + r.status + ": " + await r.text().catch(() => "") }; }
  if (!r.ok && j && j.ok === undefined) j = { ok: false, error: j.error || ("HTTP " + r.status) };
  return j;
}

function msg(id, text, cls) {
  const e = $(id);
  if (!e) return;
  e.textContent = text;
  e.className = "small " + (cls || "muted");
}

function kv(pairs) {
  return pairs.filter((p) => p[1] !== undefined && p[1] !== null)
    .map(([k, v]) => `<div class="kv"><span>${esc(k)}</span><b>${esc(v)}</b></div>`).join("");
}

function kw(w) {
  if (w === undefined || w === null) return "–";
  const a = Math.abs(w);
  return (a >= 1000 ? (w / 1000).toFixed(2) + " kW" : Math.round(w) + " W");
}
function ago(ts, now) {
  if (!ts) return "never";
  const d = now - ts;
  return d < 60 ? d + " s ago" : Math.round(d / 60) + " min ago";
}

// ---------------------------------------------------------------- overview

let snap = null;

function render(s) {
  snap = s;
  const tele = s.telemetry || {};
  const pf = tele.power_flow && tele.power_flow.value;
  const bat = tele.battery && tele.battery.value;
  const st = tele.settings && tele.settings.value;
  const first = bat && bat.items && bat.items[0];

  $("soc").textContent = first ? first.soc + "%" : "–";
  $("soc-sub").innerHTML = first
    ? kv([["SOH", first.soh + "%"], ["energy", Math.round(first.current_energy_wh) + " / " + first.full_energy_wh + " Wh"],
          ["voltage", (first.voltage_mv / 1000).toFixed(1) + " V"], ["current", (first.current_ma / 1000).toFixed(1) + " A"],
          ["cycles", first.cycle_count], ["module", first.barcode || first.id_info]])
    : '<span class="muted">no battery read yet</span>';

  $("power").innerHTML = pf
    ? kv([["battery", kw(pf.battery_w) + (pf.battery_w > 0 ? " charging" : pf.battery_w < 0 ? " discharging" : " idle")],
          ["solar", kw(pf.solar_w)],
          ["grid", kw(pf.grid_w) + (pf.grid_w > 0 ? " import" : pf.grid_w < 0 ? " export" : "")],
          ["load", kw((pf.addition_load_w || 0) + (pf.other_load_w || 0))],
          ["EV", kw(pf.ev_w)],
          ["solar efficiency", pf.solar_efficiency + "%"],
          ["third-party PV", pf.thirdparty_pv_on ? "on" : "off"],
          ["sampled", ago(tele.power_flow.ts, s.now)]])
    : '<span class="muted">no power read yet</span>';

  const t = s.today || {};
  $("today").innerHTML = kv([["date", t.date], ["charged", t.charged_wh + " Wh"], ["discharged", t.discharged_wh + " Wh"],
    ["imported", t.imported_wh + " Wh"], ["exported", t.exported_wh + " Wh"], ["solar", t.solar_wh + " Wh"], ["load", t.load_wh + " Wh"]]);

  $("conn").innerHTML = kv([
    ["vendor app", s.app_age_s === null || s.app_age_s === undefined ? "never" : s.app_age_s + " s ago"],
    ["Home Assistant", s.ha_age_s === null || s.ha_age_s === undefined ? "never" : s.ha_age_s + " s ago"],
    ["logins", s.logins],
    ["relay frames", s.relay && s.relay.frames],
    ["device end", s.relay && s.relay.device_end ? "seen" : "not seen"],
    ["API hits", Object.entries(s.hits || {}).reduce((a, [, n]) => a + n, 0)],
  ]);

  const alarms = [];
  if (st) {
    (st.errors || []).forEach((e) => alarms.push("settings: " + e));
    const inv = st.accessories && st.accessories.inverter;
    (Array.isArray(inv) ? inv : []).forEach((i) => {
      ["battery_exceptions", "inverter_exceptions", "grid_exceptions", "system_exceptions", "mppt_exceptions", "present_exceptions", "dc_exceptions"]
        .forEach((k) => { if (i[k]) alarms.push(i.id_info + " " + k + "=0x" + i[k].toString(16)); });
    });
  }
  if (first && first.faults) alarms.push("battery faults=0x" + first.faults.toString(16));
  if (first && first.state_flags) alarms.push("battery state=0x" + first.state_flags.toString(16));
  $("alarms").innerHTML = alarms.length ? alarms.map((a) => '<div class="warn">' + esc(a) + "</div>").join("")
    : '<span class="muted">none reported</span>';

  $("temps").innerHTML = first
    ? kv([["BMS", first.bms_temp_c + " °C"], ["electrode A", first.electrode_a_temp_c + " °C"], ["electrode B", first.electrode_b_temp_c + " °C"]])
    : '<span class="muted">no read yet</span>';

  const sess = (s.relay && s.relay.sessions) || [];
  $("sessions").innerHTML = sess.length
    ? "<table><tr><th>end</th><th>addr</th><th>age</th><th></th></tr>" + sess.map((x) =>
      `<tr><td class="mono small">${esc(x.end_id)}</td><td class="mono small">${esc(x.addr)}</td><td>${x.age_s} s</td><td>${x.device ? "module" : "app"}</td></tr>`).join("") + "</table>"
    : '<span class="muted">no E2E sessions</span>';

  const m = s.mode_status || {};
  const badge = $("mode");
  const pinned = m.pinned;
  badge.textContent = pinned ? "local mode" : "vendor mode";
  badge.className = "badge " + (pinned ? "ok" : "warn");
  $("foot").textContent = `${s.name || ""} ${s.model || ""} · ${s.timezone} · uptime ${Math.round((s.uptime_s || 0) / 60)} min · bridge ${s.mode}`;
  $("ha-info").textContent = `${location.hostname}:${(s.relay && s.relay.port) || 1051} (e2e) / 12443 (REST)`;
  if (st) fillSettingsForm(st);
}

// ---------------------------------------------------------------- schedule

const ACTIONS = ["none", "idle", "charge-low", "charge-high", "charge-100", "discharge-low", "discharge-high"];

function slotSelects() { return Array.from($("slots").querySelectorAll("select")); }

async function loadSchedule() {
  const j = await api("/api/schedule");
  if (!j.ok && j.error) { msg("sched-msg", j.error, "warn"); return; }
  $("ov-high").value = j.high;
  $("ov-low").value = j.low;
  $("ov-enable").checked = !!j.enable;
  const slots = j.slots && j.slots.length ? j.slots : new Array(96).fill(0x80);
  const wrap = $("slots");
  wrap.innerHTML = "";
  slots.forEach((code, i) => {
    const sel = document.createElement("select");
    ACTIONS.forEach((a, idx) => { const o = document.createElement("option"); o.value = a; o.textContent = a; sel.appendChild(o); });
    sel.value = j.labels[i];
    sel.title = `slot ${i} (${String(Math.floor(i / 4)).padStart(2, "0")}:${String((i % 4) * 15).padStart(2, "0")}) code 0x${code.toString(16)}`;
    wrap.appendChild(sel);
  });
  msg("sched-msg", `${slots.length} slots · updated ${ago(j.updated, Math.floor(Date.now() / 1000))} · source ${j.source || "bridge"}${j.slots && j.slots.length === 192 ? " (2 days)" : ""}`);
}

async function saveSchedule() {
  const slots = slotSelects().map((s) => s.value);
  const body = { high: +$("ov-high").value, low: +$("ov-low").value, enable: $("ov-enable").checked ? 1 : 0, slots };
  if (!slots.length) { msg("sched-msg", "no slots loaded", "warn"); return; }
  msg("sched-msg", "writing…");
  const j = await api("/api/schedule", body);
  msg("sched-msg", j.ok ? `written (${j.result.slots} slots)` : "failed: " + j.error, j.ok ? "" : "warn");
  if (j.ok) loadSchedule();
}

// ---------------------------------------------------------------- settings

function fillSettingsForm(st) {
  const rc = st.reserve_config || {};
  if (rc.reserve_mode) $("rs-mode").value = String(rc.reserve_mode);
  if (rc.smart_pct !== undefined) $("rs-smart").value = rc.smart_pct;
  if (rc.emergency_pct !== undefined) $("rs-emergency").value = rc.emergency_pct;
  const sp = st.selling_protection || {};
  $("sp-on").checked = !!sp.on;
  if (sp.threshold_kwh !== undefined) $("sp-kwh").value = sp.threshold_kwh;
}

async function loadSchema() {
  const j = await api("/api/schema");
  if (!j || !j.read_only) return;
  $("readonly").innerHTML = j.read_only.map((r) => `<li><b>${esc(r.name)}</b> (${esc(r.key)}) — ${esc(r.reason)}</li>`).join("");
}

async function saveReserve() {
  const body = { mode: +$("rs-mode").value, smart: +$("rs-smart").value, emergency: +$("rs-emergency").value, sync: $("rs-sync").checked ? 1 : 0, enable: 0 };
  msg("rs-msg", "writing…");
  const j = await api("/api/settings/reserve", body);
  msg("rs-msg", j.ok ? "written; re-reading" : "failed: " + j.error, j.ok ? "" : "warn");
  if (j.ok) refresh("settings");
}

async function saveSp() {
  const j = await api("/api/settings/selling_protection", { on: $("sp-on").checked ? 1 : 0, threshold_kwh: +$("sp-kwh").value });
  msg("sp-msg", j.ok ? "written" : "failed: " + j.error, j.ok ? "" : "warn");
  if (j.ok) refresh("settings");
}

async function saveEc() {
  const s = $("ec-start").value ? Math.floor(new Date($("ec-start").value).getTime() / 1000) : 0;
  const e = $("ec-end").value ? Math.floor(new Date($("ec-end").value).getTime() / 1000) : 0;
  const j = await api("/api/settings/emergency_charge", { on: $("ec-on").checked ? 1 : 0, start: s, end: e });
  msg("ec-msg", j.ok ? "written" : "failed: " + j.error, j.ok ? "" : "warn");
}

async function saveMs() {
  const j = await api("/api/settings/manual_selling", { on: $("ms-on").checked ? 1 : 0, target_kwh: +$("ms-kwh").value });
  msg("ms-msg", j.ok ? "written" : "failed: " + j.error, j.ok ? "" : "warn");
}

// ---------------------------------------------------------------- system

async function loadSystem() {
  const m = await api("/api/mode");
  $("mode-state").textContent = JSON.stringify(m, null, 1);
  const l = m.lte || {};
  $("lte-state").textContent = "modem " + (l.modem || "-") + ": " + (l.modem_bound ? "bound" : "unbound") +
    " · pppd " + (l.pppd ? "running" : "stopped") +
    " · " + (l.interfaces || []).map((i) => i.name + (i.present ? " up" : " down")).join(", ");
  const c = await api("/api/config");
  const fields = [["timezone", "str"], ["log_level", "str"], ["poll_power_flow_s", "int"], ["poll_settings_s", "int"],
    ["chat_rotate_s", "int"], ["session_hours", "int"], ["relay_advertise_host", "str"], ["ack_style", "str"],
    ["log_max_bytes", "int"], ["log_keep", "int"]];
  $("config").innerHTML = fields.map(([k]) => `<label>${k} <input id="cfg-${k}" value="${esc(c[k] === undefined ? "" : c[k])}"></label>`).join("")
    + `<p class="muted small">users: ${esc((c.users || []).join(", "))} · external cert: ${c.external_cert ? "yes" : "generated self-signed"} · api :${c.api_port} · e2e :${c.relay_port}/${c.util_port} · ui :${c.ui_port}</p>`;
  const b = await api("/api/backups");
  $("backups").innerHTML = "<tr><th>name</th><th>bytes</th><th>when</th><th></th></tr>" +
    (Array.isArray(b) ? b.map((x) => `<tr><td class="mono small">${esc(x.name)}</td><td>${x.bytes}</td><td>${new Date((x.mtime || 0) * 1000).toLocaleString()}</td><td><button data-restore="${esc(x.name)}">restore</button></td></tr>`).join("") : "");
}

async function refresh(what) {
  msg("refresh-msg", "reading…");
  const j = await api("/api/refresh", { what });
  if (!j.ok) { msg("refresh-msg", j.error, "warn"); return; }
  msg("refresh-msg", "read " + new Date().toLocaleTimeString());
  poll();
}

// ---------------------------------------------------------------- logs

async function loadLogs() {
  const q = encodeURIComponent($("logq").value || "");
  const j = await api("/api/logs?n=300&q=" + q);
  const lines = (j.lines || []).concat((j.events || []).map((e) => `[ev] ${new Date(e.ts * 1000).toLocaleTimeString()} ${e.lvl} ${e.tag}: ${e.msg}`));
  $("log").textContent = lines.join("\n");
}

// ---------------------------------------------------------------- wiring

document.querySelectorAll("nav button").forEach((b) => b.addEventListener("click", () => {
  document.querySelectorAll("nav button").forEach((x) => x.classList.toggle("on", x === b));
  document.querySelectorAll("main section").forEach((s) => { s.hidden = s.id !== b.dataset.tab; });
  if (b.dataset.tab === "schedule") loadSchedule();
  if (b.dataset.tab === "settings") loadSchema();
  if (b.dataset.tab === "system") loadSystem();
  if (b.dataset.tab === "logs") loadLogs();
}));

$("refresh").addEventListener("click", () => refresh("all"));
$("save-schedule").addEventListener("click", saveSchedule);
$("reload-schedule").addEventListener("click", loadSchedule);
$("apply-bulk").addEventListener("click", () => {
  const a = $("bulk").value;
  if (!a) return;
  slotSelects().forEach((s) => { s.value = a; });
});
$("bulk").innerHTML = '<option value="">choose action…</option>' + ACTIONS.map((a) => `<option value="${a}">${a}</option>`).join("");
$("save-reserve").addEventListener("click", saveReserve);
$("save-sp").addEventListener("click", saveSp);
$("save-ms").addEventListener("click", saveMs);
$("save-ec").addEventListener("click", saveEc);
$("go-local").addEventListener("click", async () => { msg("sys-msg", "switching…"); const j = await api("/api/mode", { mode: "local" }); msg("sys-msg", j.ok ? "local mode active" : "failed: " + j.error, j.ok ? "" : "warn"); loadSystem(); poll(); });
$("lte-off").addEventListener("click", async () => { if (!confirm("Close the LTE path? The modem is unbound from USB and pppd stops; the vendor app cannot dial out until it is restored.")) return; msg("lte-msg", "closing…"); const j = await api("/api/lte", { on: false }); msg("lte-msg", j.ok ? "LTE path closed" : "failed: " + j.error, j.ok ? "" : "warn"); loadSystem(); });
$("lte-on").addEventListener("click", async () => { msg("lte-msg", "restoring…"); const j = await api("/api/lte", { on: true }); msg("lte-msg", j.ok ? "modem bound: the app will redial" : "failed: " + j.error, j.ok ? "" : "warn"); loadSystem(); });
$("go-vendor").addEventListener("click", async () => { if (!confirm("Switch to vendor mode? The bridge's cloud leg will pass traffic through and the vendor app restarts against its original cloud path.")) return; msg("sys-msg", "switching…"); const j = await api("/api/mode", { mode: "vendor" }); msg("sys-msg", j.ok ? "vendor mode active" : "failed: " + j.error, j.ok ? "" : "warn"); loadSystem(); poll(); });
$("restart-app").addEventListener("click", async () => { if (!confirm("Restart the vendor app (not the SoC)?")) return; const j = await api("/api/restart", { target: "app" }); msg("sys-msg", j.ok ? "app restarted" : "failed: " + j.error, j.ok ? "" : "warn"); });
$("restart-bridge").addEventListener("click", async () => { if (!confirm("Restart the bridge?")) return; await api("/api/restart", { target: "bridge" }); msg("sys-msg", "bridge restarting…"); setTimeout(() => location.reload(), 4000); });
$("mk-backup").addEventListener("click", async () => { const j = await api("/api/backups", { action: "create" }); msg("sys-msg", j.ok ? "backup " + j.result.file : "failed: " + j.error, j.ok ? "" : "warn"); loadSystem(); });
$("backups").addEventListener("click", async (e) => {
  const n = e.target && e.target.dataset && e.target.dataset.restore;
  if (!n) return;
  if (!confirm("Restore " + n + "? The vendor app restarts and its current state is moved aside.")) return;
  const j = await api("/api/backups", { action: "restore", name: n });
  msg("sys-msg", j.ok ? "restored " + n : "failed: " + j.error, j.ok ? "" : "warn");
  loadSystem();
});
$("save-config").addEventListener("click", async () => {
  const body = {};
  document.querySelectorAll("#config input").forEach((i) => {
    const k = i.id.slice(4);
    body[k] = /^\d+$/.test(i.value) ? +i.value : i.value;
  });
  const j = await api("/api/config", body);
  msg("cfg-msg", j.ok ? "saved: " + (j.changed || []).join(", ") + (j.rejected && j.rejected.length ? " (rejected: " + j.rejected.join(", ") + ")" : "") : "failed", j.ok ? "" : "warn");
});
$("save-pw").addEventListener("click", async () => {
  const j = await api("/api/password", { user: (snap && snap.operator) || "operator", current: $("pw-cur").value, new: $("pw-new").value });
  msg("pw-msg", j.ok ? "password changed" : "failed: " + j.error, j.ok ? "" : "warn");
});
$("logout").addEventListener("click", async () => { await api("/logout"); location = "/"; });
$("log-refresh").addEventListener("click", loadLogs);
$("logq").addEventListener("keydown", (e) => { if (e.key === "Enter") loadLogs(); });

let timer = null;
async function poll() {
  try {
    const s = await api("/api/status");
    if (s && s.now) render(s);
  } catch (e) { /* bridge restarting */ }
}
poll();
timer = setInterval(poll, 10000);

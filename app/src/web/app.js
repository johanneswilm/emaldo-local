// emaldo-local operator UI. One page for both roles: the backend says what it
// is (`api/role`) and the panes marked data-pane=bridge|driver appear only for
// the role that can actually serve them. No external assets.
"use strict";

const H = { "content-type": "application/json", "x-emaldo-local": "1" };
const $ = (id) => document.getElementById(id);
const esc = (s) => String(s).replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" }[c]));

let ROLE = "";
let snap = null;
let es = null;
let timer = null;
let HIST = [];
let CFG = null;

// ---------------------------------------------------------------- plumbing

/// Raw fetch for both APIs: bridge answers `{ok}`, driver answers the object
/// itself, so the shape is decided by the caller.
async function get(path, body) {
  const opt = body === undefined ? {} : { method: "POST", headers: H, body: JSON.stringify(body) };
  return fetch(path, opt);
}

/// JSON call with the status kept; never throws on auth so both roles can
/// send the user to the sign-in card.
async function call(path, body) {
  const opt = body === undefined ? {} : { method: "POST", headers: H, body: JSON.stringify(body) };
  const r = await fetch(path, opt);
  let j = null;
  try { j = await r.json(); } catch (e) { j = null; }
  return { status: r.status, ok: r.ok, j };
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
function fmtkW(w) {
  if (w === undefined || w === null) return "–";
  const k = w / 1000;
  return (k >= 0 ? "" : "−") + Math.abs(k).toFixed(2) + " kW";
}
function fmtW(w) { return (w === undefined || w === null) ? "–" : Math.round(w) + " W"; }
function ago(ts, now) {
  if (!ts) return "never";
  const d = now - ts;
  return d < 60 ? d + " s ago" : Math.round(d / 60) + " min ago";
}

// ---------------------------------------------------------------- hero

function setArc(id, active) {
  const el = document.getElementById(id);
  if (el) el.classList.toggle("active", !!active);
}

/// Drive the flow diagram. Same element ids as the original driver page:
/// v_pv / v_grid / v_bat / v_load / v_soc, bat_fill, soc_bar, s_meta.
function renderHero(pv, grid, bat, load, soc, meta) {
  if (!$("v_pv")) return;
  const f = (w) => (w === undefined || w === null ? "–" : fmtkW(w));
  $("v_pv").textContent = f(pv);
  $("v_grid").textContent = f(grid);
  $("v_bat").textContent = f(bat);
  $("v_load").textContent = f(load);
  $("v_soc").textContent = soc == null ? "–" : soc.toFixed(0) + " % charged";
  $("v_bat").setAttribute("class", "node-val " + (bat > 50 ? "charge" : bat < -50 ? "draw" : "node-val"));

  const top = 152, bot = 208, h = bot - top;
  const frac = Math.max(0, Math.min(1, (soc || 0) / 100));
  $("bat_fill").setAttribute("y", String(bot - h * frac));
  $("bat_fill").setAttribute("height", String(Math.max(0, h * frac)));
  const bar = $("soc_bar");
  if (bar) bar.style.width = Math.max(0, Math.min(100, soc || 0)) + "%";

  setArc("a_pv", pv > 20);
  setArc("a_grid", Math.abs(grid) > 20);
  setArc("a_bat", Math.abs(bat) > 20);
  setArc("a_load", load > 20);
  const m = $("s_meta");
  if (m) m.textContent = meta || "";
}

/// Collapsible device rows (the original page's tree look).
function treeGroup(title, inner, open) {
  return '<details class="dt-grp"' + (open === false ? "" : " open") + '><summary>' + esc(title) + "</summary>" + inner + "</details>";
}
function treeRow(k, v) {
  return '<div class="dt-row"><span class="dt-k">' + esc(k) + '</span><span class="dt-v">' + v + "</span></div>";
}

// Charts: 1:1 geometry so axis text is not stretched.
function chartWidth(svg, fb) { const w = svg && svg.clientWidth ? svg.clientWidth : fb; return Math.max(260, Math.round(w)); }
function chartHeight(svg, fb) { const h = svg && svg.clientHeight ? svg.clientHeight : fb; return Math.max(160, Math.round(h)); }
function setViewBox(svg, W, H) { if (svg) svg.setAttribute("viewBox", "0 0 " + W + " " + H); }
function fsChart(svg) {
  if (document.fullscreenElement) { document.exitFullscreen().catch(() => {}); return; }
  if (svg.requestFullscreen) {
    svg.requestFullscreen().then(() => {
      setTimeout(() => { if (svg.id === "histchart") renderHistory(); else renderPlanner(snap || {}); }, 120);
    }).catch(() => {});
  }
}
document.addEventListener("fullscreenchange", () => {
  if (!document.fullscreenElement) { renderHistory(); if (snap) renderPlanner(snap); }
});
window.addEventListener("resize", () => { renderHistory(); if (snap) renderPlanner(snap); });

function show(which) {
  ["setup", "login", "overview", "history", "plan", "schedule", "settings", "system", "logs"]
    .forEach((id) => { const e = $(id); if (e) e.hidden = id !== which; });
  const rs = $("role-state");
  if (rs) rs.textContent = ROLE ? "running: " + ROLE + " role" : "";
  $("nav").hidden = which === "setup" || which === "login";
  $("mode").hidden = which === "setup" || which === "login";
}

function applyRolePanes() {
  document.querySelectorAll("[data-pane]").forEach((e) => {
    e.hidden = e.dataset.pane !== ROLE;
  });
  document.querySelectorAll("nav button[data-pane]").forEach((b) => {
    // The plan and schedule/status panes exist in both roles; only the
    // bridge-only tabs go away.
    if (b.dataset.tab === "schedule" || b.dataset.tab === "logs") b.hidden = ROLE !== "bridge";
  });
}

// ---------------------------------------------------------------- auth

// The driver role stores one admin password; the bridge keeps a user map.
// The user name is whatever the operator typed - never a remembered value,
// which is how a form default silently overrode the input.

async function loadCreds() {
  // Shows/hides the user field and offers a default name; the submit path
  // reads the input itself.
  const field = $("cred-field");
  if (field) field.hidden = ROLE !== "bridge";
  if (ROLE !== "bridge") return;
  const r = await call("/api/v1/config");
  const names = ((r.j || {}).users) || [];
  const u = $("li_user");
  if (u && !u.value && Array.isArray(names) && names.length === 1) u.value = names[0];
}

async function boot() {
  const r = await call("/api/v1/role");
  const j = r.j || {};
  // The role decides which sign-in fields exist, so it is known before any
  // card is shown - otherwise the login card was rendered without a user
  // field and the submit fell back to a default name.
  ROLE = j.role || "";
  applyRolePanes();
  await loadCreds();
  if (j.setup_required) { show("setup"); return; }
  if (!j.authed) { show("login"); return; }
  startApp();
}

async function doSetup() {
  const body = {
    user: ROLE === "bridge" ? ($("su_user").value.trim() || "operator") : "admin",
    password: $("su_pw").value,
    device_name: $("su_name").value,
    broker_url: $("su_url").value,
  };
  const r = await call("/api/v1/setup", body);
  if (r.ok) { $("su_pw").value = ""; msg("su-msg", "created — log in"); show("login"); $("li_user").value = body.user; }
  else msg("su-msg", "failed: " + ((r.j && r.j.error) || ("HTTP " + r.status)), "warn");
}

async function doLogin() {
  const user = ($("li_user") && $("li_user").value.trim()) || "admin";
  const r = await call("/api/v1/login", { user, password: $("li_pw").value });
  if (r.ok) { $("li_pw").value = ""; boot(); }
  else msg("li-msg", r.status === 401 ? "unknown user or wrong password" : ("log in failed: HTTP " + r.status), "warn");
}

async function doLogout() {
  await get("/api/v1/logout", {});
  if (es) { es.close(); es = null; }
  clearInterval(timer);
  timer = null;
  show("login");
}

async function startApp() {
  show("overview");
  document.querySelectorAll("nav button").forEach((x) => x.classList.toggle("on", x.dataset.tab === "overview"));
  await loadStatus();
  startStream();
  loadHistory();
  loadEvents();
}

// ---------------------------------------------------------------- status

/// Both roles answer `/api/status`; the bridge wraps the vendor front's
/// snapshot, the driver answers its telemetry state directly.
async function loadStatus() {
  const r = await call("/api/v1/status");
  if (r.status === 401) { show("login"); return; }
  if (!r.ok || !r.j) return;
  if (ROLE === "bridge") renderBridge(r.j);
  else { renderDriverState(r.j); }
  if (ROLE === "bridge") renderBridgeExtras(r.j);
}

function startStream() {
  if (es) es.close();
  es = new EventSource("/api/v1/stream");
  es.onmessage = (ev) => {
    let st = null;
    try { st = JSON.parse(ev.data); } catch (e) { return; }
    if (ROLE === "bridge") { snap = st; renderBridge(st); }
    else { renderDriverState(st); }
  };
  clearInterval(timer);
  timer = setInterval(() => { if (!es || es.readyState === 2) loadStatus(); }, 30000);
}

// ---------------------------------------------------------------- bridge

const MODE_NAME = { auto: "Limits & schedule", optimize: "Cost Optimizer", idle: "Idle", charge: "Charge", discharge: "Discharge" };
const MODE_HELP = {
  "auto": "Limits & schedule — applies your charge/discharge limits and reserve, and any 96-slot override schedule. No price or forecast logic.",
  "optimize": "Cost Optimizer — plans quarter-hours using the day-ahead prices and PV forecast: charges when energy is cheap or PV is abundant, discharges when it is expensive, within your reserve and limits.",
  "idle": "Idle — battery held at 0 W (no charge, no discharge).",
  "charge": "Charge — charge at the charge limit (up to the max SOC), regardless of price.",
  "discharge": "Discharge — discharge at the discharge limit (down to the reserve), regardless of price.",
};

function renderBridge(s) {
  snap = s;
  const tele = s.telemetry || {};
  const pf = tele.power_flow && tele.power_flow.value;
  const bat = tele.battery && tele.battery.value;
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
          ["third-party PV", pf.thirdparty_pv_on ? "on" : "off"],
          ["sampled", ago(tele.power_flow.ts, s.now)]])
    : '<span class="muted">no power read yet</span>';

  const t = s.today || {};
  $("today").innerHTML = kv([["date", t.date], ["charged", t.charged_wh + " Wh"], ["discharged", t.discharged_wh + " Wh"],
    ["imported", t.imported_wh + " Wh"], ["exported", t.exported_wh + " Wh"], ["solar", t.solar_wh + " Wh"], ["load", t.load_wh + " Wh"]]);

  $("conn").innerHTML = kv([
    ["vendor app", ago(s.app_last_seen, s.now) + " seen"],
    ["Home Assistant", s.ha_age_s === null || s.ha_age_s === undefined ? "never" : s.ha_age_s + " s ago"],
    ["logins", s.logins],
    ["relay frames", s.relay && s.relay.frames],
    ["device end", s.relay && s.relay.device_end ? "seen" : "not seen"],
    ["API hits", Object.values(s.hits || {}).reduce((a, n) => a + n, 0)],
  ]);

  // Hero: the vendor module's own numbers.
  const ev = pf ? (pf.ev_w || 0) : 0;
  renderHero(
    pf ? pf.solar_w : null,
    pf ? pf.grid_w : null,
    pf ? pf.battery_w : null,
    pf ? (pf.addition_load_w || 0) + (pf.other_load_w || 0) : null,
    first ? first.soc : null,
    pf ? "updated " + ago(tele.power_flow.ts, s.now) + (ev ? " · EV " + kw(ev) : "") : "no power read yet",
  );

  renderBridgeExtras(s);
  renderPolicy(s.optimizer || { policy: { mode: (s.mode_status && s.mode_status.mode) || "" } }, s);
}

function renderBridgeExtras(s) {
  const tele = s.telemetry || {};
  const bat = tele.battery && tele.battery.value;
  const first = bat && bat.items && bat.items[0];

  const alarms = [];
  (s.errors || []).forEach((e) => alarms.push("settings: " + e));
  const inv = s.accessories && s.accessories.inverter;
  (Array.isArray(inv) ? inv : []).forEach((i) => {
    ["battery_exceptions", "inverter_exceptions", "grid_exceptions", "system_exceptions", "mppt_exceptions", "present_exceptions", "dc_exceptions"]
      .forEach((k) => { if (i[k]) alarms.push(i.id_info + " " + k + "=0x" + i[k].toString(16)); });
  });
  if (first && first.faults) alarms.push("battery faults=0x" + first.faults.toString(16));
  $("alarms").innerHTML = alarms.length ? alarms.map((a) => '<div class="warn">' + esc(a) + "</div>").join("")
    : '<span class="muted">none reported</span>';

  $("temps").innerHTML = first
    ? kv([["BMS", first.bms_temp_c + " °C"], ["electrode A", first.electrode_a_temp_c + " °C"], ["electrode B", first.electrode_b_temp_c + " °C"]])
    : '<span class="muted">no read yet</span>';

  const treeEl = $("s_tree");
  if (treeEl) {
    let t = treeGroup("Battery",
      treeRow("State of charge", "<b>" + (first ? first.soc + " %" : "–") + "</b>") +
      treeRow("Power", first ? fmtkW(first.current_ma * first.voltage_mv / 1e6) : "–") +
      treeRow("Voltage / current", first ? (first.voltage_mv / 1000).toFixed(2) + " V · " + (first.current_ma / 1000).toFixed(1) + " A" : "–") +
      treeRow("Energy", first ? Math.round(first.current_energy_wh) + " / " + first.full_energy_wh + " Wh" : "–"));
    const items = (bat && bat.items) || [];
    if (items.length) {
      t += treeGroup("Packs (" + items.length + ")",
        items.map((x) => treeRow("Pack " + (x.slot != null ? x.slot : "?"),
          (x.soc || 0) + "% · " + ((x.voltage_mv || 0) / 1000).toFixed(2) + " V · " + (x.soh || 0) + "% SOH")).join(""), false);
    }
    t += treeGroup("Vendor front",
      treeRow("app seen", ago(s.app_last_seen, s.now)) +
      treeRow("logins", s.logins) +
      treeRow("relay frames", s.relay && s.relay.frames));
    treeEl.innerHTML = t;
  }

  const sess = (s.relay && s.relay.sessions) || [];
  $("sessions").innerHTML = sess.length
    ? "<table><tr><th>end</th><th>addr</th><th>age</th><th>role</th></tr>" + sess.map((x) =>
      `<tr><td class="mono small">${esc(x.end_id)}</td><td class="mono small">${esc(x.addr)}</td><td>${x.age_s} s</td><td>${x.device ? "module" : "app"}</td></tr>`).join("") + "</table>"
    : '<span class="muted">no E2E sessions</span>';

  const m = s.mode_status || {};
  const badge = $("mode");
  badge.textContent = (m.pinned ? "local mode" : "vendor mode") + " · bridge";
  badge.className = "badge " + (m.pinned ? "ok" : "warn");
  $("foot").textContent = `${s.name || ""} ${s.model || ""} · ${s.timezone || ""} · uptime ${Math.round((s.uptime_s || 0) / 60)} min`;
  const hi = $("ha-info");
  if (hi) hi.textContent = `${location.hostname}:${(s.relay && s.relay.port) || 1051} (e2e) / ${s.ui_port || 4443} (UI)`;
}

// ---------------------------------------------------------------- driver

function renderDriverState(st) {
  if (!st || !st.battery) return;
  snap = st;
  window.LASTSTATE = st;
  const pv   = (st.pv && st.pv.power_w) || 0;
  const grid = (st.grid && st.grid.net_w) || 0;
  const bat  = st.battery.power_w || 0;
  const load = (st.load && st.load.net_w) || 0;
  const soc  = st.battery.soc_percent ?? 0;

  $("soc").textContent = soc.toFixed(1) + "%";
  $("soc-sub").innerHTML = kv([
    ["power", fmtkW(bat) + (bat > 50 ? " charging" : bat < -50 ? " discharging" : " idle")],
    ["voltage", (st.battery.voltage_v || 0).toFixed(2) + " V"],
    ["current", (st.battery.current_a || 0).toFixed(1) + " A"],
    ["temperature", (st.battery.temp_c || 0).toFixed(1) + " °C"],
    ["remaining", Math.round(st.battery.remaining_wh || 0) + " / " + Math.round(st.battery.capacity_wh || 0) + " Wh"],
    ["health / cycles", Math.round(st.battery.soh_percent || 0) + " % · " + (st.battery.cycle_count || 0)],
  ]);
  $("power").innerHTML = kv([
    ["solar", fmtkW(pv)], ["grid", fmtkW(grid) + (grid > 0 ? " import" : grid < 0 ? " export" : "")],
    ["battery", fmtkW(bat)], ["load", fmtkW(load)],
    ["updated", new Date((st.updated_at || 0) * 1000).toLocaleTimeString()],
  ]);
  const pol = st.policy || {};
  $("today").innerHTML = kv([
    ["mode", MODE_NAME[pol.mode] || pol.mode || "–"],
    ["price now", st.prices && st.prices.next_slots && st.prices.next_slots[0]
      ? st.prices.next_slots[0].eur_kwh.toFixed(3) + " " + (st.prices.currency || "") + "/kWh" : "–"],
    ["PV forecast today", st.solar && st.solar.kwp ? (st.solar.today_kwh || 0).toFixed(1) + " kWh" : "–"],
  ]);
  $("conn").innerHTML = kv([
    ["MCU", (st.mcu && st.mcu.present) ? "present" : "absent"],
    ["CAN", st.can && st.can.enabled ? (st.can.iface || "can0") + " · " + (st.can.frames || 0) + " frames" : "off"],
    ["grid meter", st.meter && st.meter.available ? "available" : "absent"],
    ["faults", (st.faults && st.faults.length) ? st.faults.join(", ") : "none"],
  ]);
  $("d_sources").innerHTML = kv([
    ["packs", st.battery.modules ? st.battery.modules + " via CAN BMS" : "none"],
    ["inverters", (st.inverters || []).map((i) => i.index + (i.running ? "✓" : "✗")).join(" ") || "–"],
  ]);

  renderHero(pv, grid, bat, load, soc,
    "updated " + new Date((st.updated_at || 0) * 1000).toLocaleTimeString() +
    " · MCU " + (st.mcu && st.mcu.present ? "present" : "absent") +
    (st.faults && st.faults.length ? " · FAULTS: " + st.faults.join(", ") : ""));

  // Device tree, the same grouping the original page had.
  let tree = "";
  tree += treeGroup("Battery",
    treeRow("State of charge", "<b>" + (st.battery.soc_percent || 0).toFixed(1) + " %</b>") +
    treeRow("Power", fmtkW(st.battery.power_w)) +
    treeRow("Voltage / current", (st.battery.voltage_v || 0).toFixed(2) + " V · " + (st.battery.current_a || 0).toFixed(1) + " A") +
    treeRow("Temperature", (st.battery.temp_c || 0).toFixed(1) + " °C") +
    treeRow("Remaining", Math.round(st.battery.remaining_wh || 0) + " / " + Math.round(st.battery.capacity_wh || 0) + " Wh") +
    treeRow("Health / cycles", (st.battery.soh_percent || 0).toFixed(0) + " % · " + (st.battery.cycle_count || 0)));
  const packs = (st.can && st.can.batteries) || [];
  if (packs.length) {
    tree += treeGroup("Packs (" + packs.length + ")",
      packs.map((x) => treeRow("Pack " + x.cabinet + "·" + x.position,
        (x.soc_percent || 0) + "% · " + ((x.voltage_mv || 0) / 1000).toFixed(2) + " V · " +
        ((x.current_ma || 0) / 1000).toFixed(2) + " A")).join(""), false);
  }
  let inv = (st.inverters || []).map((i) => treeRow("Inverter " + i.index, i.running ? "running" : "off")).join("");
  inv += treeRow("MCU", (st.mcu && st.mcu.present) ? "present" : "absent");
  tree += treeGroup("Inverters &amp; MCU", inv);
  let src = treeRow("Grid meter", st.meter && st.meter.available
    ? (st.meter.import_kwh || 0).toFixed(2) + " kWh in · " + (st.meter.export_kwh || 0).toFixed(2) + " kWh out" : "absent");
  if (st.prices && st.prices.area) src += treeRow("Prices", st.prices.area + " · now " +
    (st.prices.next_slots && st.prices.next_slots[0] ? st.prices.next_slots[0].eur_kwh.toFixed(3) + " " + (st.prices.currency || "") : "–"));
  if (st.solar && st.solar.kwp) src += treeRow("PV forecast", st.solar.kwp.toFixed(2) + " kWp · " + (st.solar.today_kwh || 0).toFixed(1) + " kWh today");
  tree += treeGroup("Sources", src);
  const stEl = $("s_tree");
  if (stEl) stEl.innerHTML = tree;

  const badge = $("mode");
  badge.textContent = "direct control · driver";
  badge.className = "badge ok";
  $("foot").textContent = `uptime ${Math.round((st.uptime_s || 0) / 60) || "–"} min`;

  const raw = st.raw || {};
  const keys = Object.keys(raw).sort();
  let html = "<tr><th>block</th><th>value</th></tr>";
  for (const k of keys) html += '<tr><td><code>' + esc(k) + "</code></td><td><code>" + esc(raw[k]) + "</code></td></tr>";
  $("s_raw").innerHTML = keys.length ? html : "";

  renderPolicy({ policy: pol, actuator: st.control || {}, prices: st.prices, solar: st.solar }, null);
  renderPlanner(st);
}

// ---------------------------------------------------------------- plan

function renderPolicy(o, bridgeSnap) {
  const pol = o.policy || {}, ctl = o.actuator || {};
  $("c_mode").textContent = MODE_NAME[pol.mode] || pol.mode || "Limits & schedule";
  $("c_setpoint").textContent = fmtW(pol.setpoint_w);
  $("c_reason").textContent = pol.reason || "";
  $("c_writes").textContent = ctl.writes_enabled ? "WRITES ENABLED" : "dry-run";
  $("c_writes").style.background = ctl.writes_enabled ? "#7a2e2e" : "#30363d";
  $("c_wcount").textContent = ctl.write_count ?? 0;
  $("c_last").textContent = ctl.last_result ? ("last: " + ctl.last_result) : "";
  $("c_modehelp").textContent = MODE_HELP[pol.mode] || "";
  const wrap = $("planner_wrap");
  if (wrap) wrap.hidden = pol.mode !== "optimize";
  renderPlanner(bridgeSnap && bridgeSnap.policy ? bridgeSnap : { policy: pol });
  if (document.activeElement !== $("p_mode")) $("p_mode").value = pol.mode || "auto";
  const links = $("c_links");
  if (links) links.innerHTML = ROLE === "bridge" ? '<a href="#plan">Plan</a> <a href="#schedule">Schedule</a>' : "";
}

function renderPlanner(st) {
  const svg = document.getElementById("planner");
  if (!svg) return;
  const note = document.getElementById("planner_note");
  const plan = (st.policy && st.policy.plan && st.policy.plan.slots) || [];
  if (!plan.length) { svg.innerHTML = ""; if (note) note.textContent = "No plan yet — the Cost Optimizer builds one when prices and PV forecast are available (set the mode to “Cost Optimizer”)."; return; }
  const H = chartHeight(svg, 220), padL = 34, padB = 22, padT = 12;
  const W = chartWidth(svg, 720);
  setViewBox(svg, W, H);
  const n = plan.length;
  const x = (i) => padL + (i / Math.max(1, n - 1)) * (W - 2 * padL);
  const bw = (W - 2 * padL) / n;
  let pmin = Infinity, pmax = -Infinity, pvmax = 1, powmax = 1;
  plan.forEach((s) => { pmin = Math.min(pmin, s.price); pmax = Math.max(pmax, s.price); pvmax = Math.max(pvmax, s.pv_w || 0); powmax = Math.max(powmax, Math.abs(s.power_w || 0)); });
  if (!isFinite(pmin)) { pmin = 0; pmax = 1; }
  if (pmax === pmin) pmax = pmin + 0.01;
  const yP = (v) => padT + (1 - (v - pmin) / (pmax - pmin)) * (H - padB - padT);
  const yPv = (v) => (H - padB) - (v / pvmax) * (H - padB - padT) * 0.55;
  const powAreaMid = H - padB;
  let g = "";
  const pvPts = plan.map((s, i) => x(i).toFixed(1) + "," + yPv(s.pv_w || 0).toFixed(1)).join(" ");
  g += '<polygon points="' + padL + "," + (H - padB) + " " + pvPts + " " + (W - padL) + "," + (H - padB) + '" fill="var(--solar)" opacity="0.28"/>';
  const pricePts = plan.map((s, i) => x(i).toFixed(1) + "," + yP(s.price).toFixed(1)).join(" ");
  g += '<polyline points="' + pricePts + '" fill="none" stroke="var(--grid)" stroke-width="2" opacity="0.9"/>';
  const half = (H - padB - padT) * 0.22;
  plan.forEach((s, i) => {
    const p = s.power_w || 0;
    if (Math.abs(p) < 1) return;
    const h = Math.min(half, (Math.abs(p) / powmax) * half);
    if (p > 0) g += '<rect x="' + (x(i) - bw * 0.4).toFixed(1) + '" y="' + (powAreaMid - h).toFixed(1) + '" width="' + Math.max(1, bw * 0.8).toFixed(1) + '" height="' + h.toFixed(1) + '" fill="var(--battery)" opacity="0.85"/>';
    else g += '<rect x="' + (x(i) - bw * 0.4).toFixed(1) + '" y="' + powAreaMid + '" width="' + Math.max(1, bw * 0.8).toFixed(1) + '" height="' + h.toFixed(1) + '" fill="var(--load)" opacity="0.7"/>';
  });
  let lastH = -1;
  plan.forEach((s, i) => {
    const h = new Date((s.start) * 1000).getHours();
    if (h % 6 === 0 && h !== lastH) {
      lastH = h;
      g += '<line x1="' + x(i) + '" y1="' + padT + '" x2="' + x(i) + '" y2="' + (H - padB) + '" stroke="#ffffff18" stroke-width="1"/>';
      g += '<text x="' + x(i) + '" y="' + (H - 6) + '" fill="var(--mut)" font-size="10" text-anchor="middle">' + String(h).padStart(2, "0") + ":00</text>";
    }
  });
  const pmid = (pmin + pmax) / 2;
  const axis = (v) => '<text x="6" y="' + (yP(v) + 3).toFixed(1) + '" fill="var(--grid)" font-size="9">' + v.toFixed(2) + "</text>";
  g += axis(pmax) + axis(pmid) + axis(pmin);
  g += '<text x="6" y="' + (H - 2) + '" fill="var(--mut)" font-size="8">EUR/kWh</text>';
  g += '<text x="' + (W - 4) + '" y="' + (yPv(pvmax) + 9).toFixed(1) + '" fill="var(--solar)" font-size="9" text-anchor="end">PV ' + (pvmax / 1000).toFixed(1) + " kW</text>";
  svg.innerHTML = g;
  if (note) note.textContent = "Price " + pmin.toFixed(2) + "–" + pmax.toFixed(2) + " EUR/kWh · PV peak " + (pvmax / 1000).toFixed(1) + " kW · bars: green = charge, orange = discharge";
}

// ---------------------------------------------------------------- history

async function loadHistory() {
  const r = await call("/api/v1/history");
  if (!r.ok || !r.j) return;
  HIST = r.j.samples || r.j || [];
  renderHistory();
}

function renderHistory() {
  const svg = document.getElementById("histchart");
  if (!svg) return;
  const note = document.getElementById("hist_note");
  const d = HIST.slice(-576);
  if (!d.length) { svg.innerHTML = ""; if (note) note.textContent = "No samples yet."; return; }
  const H = chartHeight(svg, 200), W = chartWidth(svg, 720), padL = 34, padB = 20, padT = 10;
  setViewBox(svg, W, H);
  const n = d.length;
  const x = (i) => padL + (i / Math.max(1, n - 1)) * (W - padL - 6);
  const ySoc = (v) => padT + (1 - v / 100) * (H - padB - padT);
  let pmax = 1;
  d.forEach((s) => { pmax = Math.max(pmax, Math.abs(s.grid || 0), Math.abs(s.pv || 0), Math.abs(s.load || 0)); });
  const yP = (v) => (H - padB) - (Math.max(0, v) / pmax) * (H - padB - padT) * 0.4;
  const line = (key) => d.map((s, i) => x(i).toFixed(1) + "," + yP(s[key] || 0).toFixed(1)).join(" ");
  let g = "";
  g += '<polyline points="' + line("pv") + '" fill="none" stroke="var(--solar)" stroke-width="1.6"/>';
  g += '<polyline points="' + line("grid") + '" fill="none" stroke="var(--grid)" stroke-width="1.4"/>';
  g += '<polyline points="' + line("load") + '" fill="none" stroke="var(--load)" stroke-width="1.2" opacity=".8"/>';
  g += '<polyline points="' + d.map((s, i) => x(i).toFixed(1) + "," + ySoc(s.soc || 0).toFixed(1)).join(" ") + '" fill="none" stroke="var(--battery)" stroke-width="2"/>';
  [0, 50, 100].forEach((v) => {
    g += '<text x="' + (W - 4) + '" y="' + ySoc(v).toFixed(1) + '" fill="var(--battery)" font-size="9" text-anchor="end">' + v + "%</text>";
  });
  svg.innerHTML = g;
  const last = d[d.length - 1];
  if (note) note.textContent = "SOC " + (last.soc || 0).toFixed(0) + "% · grid now " + fmtkW(last.grid) + ", PV " + fmtkW(last.pv) + ", load " + fmtkW(last.load);
}

// ---------------------------------------------------------------- scheduler / settings (bridge)

const ACTIONS = ["none", "idle", "charge-low", "charge-high", "charge-100", "discharge-low", "discharge-high"];
function slotSelects() { return Array.from($("slots").querySelectorAll("select")); }

async function loadSchedule() {
  const r = await call("/api/v1/schedule");
  const j = r.j || {};
  if (!r.ok) { msg("sched-msg", j.error || ("HTTP " + r.status), "warn"); return; }
  $("ov-high").value = j.high;
  $("ov-low").value = j.low;
  $("ov-enable").checked = !!j.enable;
  const slots = j.slots && j.slots.length ? j.slots : new Array(96).fill(0x80);
  const wrap = $("slots");
  wrap.innerHTML = "";
  slots.forEach((code, i) => {
    const sel = document.createElement("select");
    ACTIONS.forEach((a) => { const o = document.createElement("option"); o.value = a; o.textContent = a; sel.appendChild(o); });
    sel.value = (j.labels || [])[i] || "none";
    sel.title = `slot ${i} (${String(Math.floor(i / 4)).padStart(2, "0")}:${String((i % 4) * 15).padStart(2, "0")}) code 0x${code.toString(16)}`;
    wrap.appendChild(sel);
  });
  msg("sched-msg", `${slots.length} slots · source ${j.source || "bridge"}${slots.length === 192 ? " (2 days)" : ""}`);
}

async function saveSchedule() {
  const slots = slotSelects().map((s) => s.value);
  if (!slots.length) { msg("sched-msg", "no slots loaded", "warn"); return; }
  msg("sched-msg", "writing…");
  const r = await call("/api/v1/schedule", { high: +$("ov-high").value, low: +$("ov-low").value, enable: $("ov-enable").checked ? 1 : 0, slots });
  const j = r.j || {};
  msg("sched-msg", r.ok && j.ok ? `written (${j.result.slots} slots)` : "failed: " + (j.error || r.status), r.ok && j.ok ? "" : "warn");
  if (r.ok && j.ok) loadSchedule();
}

function fillSettingsForm(tele) {
  const st = (tele && tele.settings && tele.settings.value) || {};
  const rc = st.reserve_config || {};
  if (rc.reserve_mode !== undefined) $("rs-mode").value = String(rc.reserve_mode);
  if (rc.smart_pct !== undefined) $("rs-smart").value = rc.smart_pct;
  if (rc.emergency_pct !== undefined) $("rs-emergency").value = rc.emergency_pct;
  const sp = st.selling_protection || {};
  $("sp-on").checked = !!sp.on;
  if (sp.threshold_kwh !== undefined) $("sp-kwh").value = sp.threshold_kwh;
}

async function loadSchema() {
  const r = await call("/api/v1/schema");
  const j = r.j || {};
  if (!j.read_only) return;
  $("readonly").innerHTML = j.read_only.map((x) => `<li><b>${esc(x.name)}</b> (${esc(x.key)}) — ${esc(x.reason)}</li>`).join("");
}

async function postSetting(path, body, id, okText) {
  msg(id, "writing…");
  const r = await call(path, body);
  const j = r.j || {};
  msg(id, r.ok && j.ok !== false ? (okText || "written") : "failed: " + (j.error || r.status), r.ok && j.ok !== false ? "" : "warn");
}

function wireBridgeSettings() {
  $("save-reserve").addEventListener("click", () => postSetting("/api/v1/settings/reserve",
    { mode: +$("rs-mode").value, smart: +$("rs-smart").value, emergency: +$("rs-emergency").value, sync: $("rs-sync").checked ? 1 : 0, enable: 0 }, "rs-msg"));
  $("save-sp").addEventListener("click", () => postSetting("/api/v1/settings/selling_protection",
    { on: $("sp-on").checked ? 1 : 0, threshold_kwh: +$("sp-kwh").value }, "sp-msg"));
  $("save-ms").addEventListener("click", () => postSetting("/api/v1/settings/manual_selling",
    { on: $("ms-on").checked ? 1 : 0, target_kwh: +$("ms-kwh").value }, "ms-msg"));
  $("save-ec").addEventListener("click", () => {
    const s = $("ec-start").value ? Math.floor(new Date($("ec-start").value).getTime() / 1000) : 0;
    const e = $("ec-end").value ? Math.floor(new Date($("ec-end").value).getTime() / 1000) : 0;
    postSetting("/api/v1/settings/emergency_charge", { on: $("ec-on").checked ? 1 : 0, start: s, end: e }, "ec-msg");
  });
  $("save-schedule").addEventListener("click", saveSchedule);
  $("reload-schedule").addEventListener("click", loadSchedule);
  $("apply-bulk").addEventListener("click", () => { const a = $("bulk").value; if (a) slotSelects().forEach((s) => { s.value = a; }); });
  $("bulk").innerHTML = '<option value="">choose action…</option>' + ACTIONS.map((a) => `<option value="${a}">${a}</option>`).join("");
  $("refresh").addEventListener("click", async () => {
    msg("refresh-msg", "reading…");
    const r = await call("/api/v1/refresh", { what: "all" });
    const j = r.j || {};
    msg("refresh-msg", r.ok && j.ok ? "read " + new Date().toLocaleTimeString() : (j.error || "failed"), r.ok && j.ok ? "" : "warn");
    loadStatus();
  });
  $("go-local").addEventListener("click", async () => {
    msg("sys-msg", "switching…");
    const r = await call("/api/v1/mode", { mode: "local" });
    const j = r.j || {};
    msg("sys-msg", r.ok && j.ok ? "local mode active" : "failed: " + (j.error || r.status), r.ok && j.ok ? "" : "warn");
    loadSystem(); loadStatus();
  });
  $("go-vendor").addEventListener("click", async () => {
    if (!confirm("Switch to vendor mode? The bridge's cloud leg will pass traffic through and the vendor app restarts against its original cloud path.")) return;
    msg("sys-msg", "switching…");
    const r = await call("/api/v1/mode", { mode: "vendor" });
    const j = r.j || {};
    msg("sys-msg", r.ok && j.ok ? "vendor mode active" : "failed: " + (j.error || r.status), r.ok && j.ok ? "" : "warn");
    loadSystem(); loadStatus();
  });
  $("lte-off").addEventListener("click", async () => {
    if (!confirm("Close the LTE path? The modem is unbound from USB and pppd stops; the vendor app cannot dial out until it is restored.")) return;
    msg("lte-msg", "closing…");
    const r = await call("/api/v1/lte", { on: false }); const j = r.j || {};
    msg("lte-msg", r.ok && j.ok ? "LTE path closed" : "failed: " + (j.error || r.status), r.ok && j.ok ? "" : "warn");
    loadSystem();
  });
  $("lte-on").addEventListener("click", async () => {
    msg("lte-msg", "restoring…");
    const r = await call("/api/v1/lte", { on: true }); const j = r.j || {};
    msg("lte-msg", r.ok && j.ok ? "modem bound: the app will redial" : "failed: " + (j.error || r.status), r.ok && j.ok ? "" : "warn");
    loadSystem();
  });
  $("restart-app").addEventListener("click", async () => {
    if (!confirm("Restart the vendor app (not the SoC)?")) return;
    const r = await call("/api/v1/restart", { target: "app" }); const j = r.j || {};
    msg("sys-msg", r.ok && j.ok ? "app restarted" : "failed: " + (j.error || r.status), r.ok && j.ok ? "" : "warn");
  });
  $("mk-backup").addEventListener("click", async () => {
    const r = await call("/api/v1/backups", { action: "create" }); const j = r.j || {};
    msg("sys-msg", r.ok && j.ok ? "backup " + j.result.file : "failed: " + (j.error || r.status), r.ok && j.ok ? "" : "warn");
    loadSystem();
  });
  $("backups").addEventListener("click", async (e) => {
    const n = e.target && e.target.dataset && e.target.dataset.restore;
    if (!n) return;
    if (!confirm("Restore " + n + "? The vendor app restarts and its current state is moved aside.")) return;
    const r = await call("/api/v1/backups", { action: "restore", name: n }); const j = r.j || {};
    msg("sys-msg", r.ok && j.ok ? "restored " + n : "failed: " + (j.error || r.status), r.ok && j.ok ? "" : "warn");
    loadSystem();
  });
}

async function loadSystem() {
  const r = await call("/api/v1/mode");
  const m = r.j || {};
  $("mode-state").textContent = JSON.stringify(m, null, 1);
  const l = m.lte || {};
  $("lte-state").textContent = "modem " + (l.modem || "-") + ": " + (l.modem_bound ? "bound" : "unbound") +
    " · pppd " + (l.pppd ? "running" : "stopped") +
    " · " + (l.interfaces || []).map((i) => i.name + (i.present ? " up" : " down")).join(", ");
  const c = (await call("/api/v1/config")).j || {};
  const fields = [["timezone", "str"], ["log_level", "str"], ["poll_power_flow_s", "int"], ["poll_settings_s", "int"],
    ["chat_rotate_s", "int"], ["session_hours", "int"], ["relay_advertise_host", "str"], ["ack_style", "str"],
    ["log_max_bytes", "int"], ["log_keep", "int"]];
  $("config").innerHTML = fields.map(([k]) => `<label>${k} <input id="cfg-${k}" value="${esc(c[k] === undefined ? "" : c[k])}"></label>`).join("")
    + `<p class="muted small">users: ${esc((c.users || []).join(", "))} · external cert: ${c.external_cert ? "yes" : "generated self-signed"} · api :${c.api_port} · e2e :${c.relay_port}/${c.util_port} · ui :${c.ui_port}</p>`;
  const b = (await call("/api/v1/backups")).j;
  $("backups").innerHTML = "<tr><th>name</th><th>bytes</th><th>when</th><th></th></tr>" +
    (Array.isArray(b) ? b.map((x) => `<tr><td class="mono small">${esc(x.name)}</td><td>${x.bytes}</td><td>${new Date((x.mtime || 0) * 1000).toLocaleString()}</td><td><button data-restore="${esc(x.name)}">restore</button></td></tr>`).join("") : "");
}

// ---------------------------------------------------------------- driver settings

async function loadConfig() {
  const r = await call("/api/v1/config");
  if (!r.ok || typeof r.j !== "object") return;
  CFG = r.j;
  $("s_cfg").textContent = JSON.stringify(CFG, null, 2);
  const m = CFG.mqtt || {}, pr = CFG.prices || {}, so = CFG.solar || {},
        hw = CFG.hardware || {}, po = CFG.policy || {}, dv = CFG.device || {};
  $("cfg_mqtt_on").checked = !!m.broker_url;
  $("cfg_mqtt_box").hidden = !$("cfg_mqtt_on").checked;
  $("cfg_mqtt_url").value = m.broker_url || "";
  $("cfg_mqtt_prefix").value = m.topic_prefix || "emaldo/powercore";
  $("cfg_mqtt_disc").value = String(m.ha_discovery !== false);
  $("cfg_mqtt_dprefix").value = m.discovery_prefix || "homeassistant";
  $("cfg_prices_enabled").value = String(!!pr.enabled);
  $("cfg_prices_area").value = pr.area || "";
  $("cfg_prices_currency").value = pr.currency || "EUR";
  $("cfg_prices_tz").value = pr.tz_offset_secs ?? 3600;
  $("cfg_prices_refresh").value = pr.refresh_secs ?? 21600;
  $("cfg_solar_enabled").value = String(!!so.enabled);
  $("cfg_solar_lat").value = so.latitude ?? "";
  $("cfg_solar_lon").value = so.longitude ?? "";
  $("cfg_solar_pr").value = so.performance_ratio ?? 0.8;
  renderSolarStrings(so.strings || []);
  $("cfg_reserve").value = po.reserve_percent ?? 20;
  const op = po.optimizer || {};
  $("cfg_opt_spread").value = op.min_spread_eur_kwh ?? 0.03;
  $("cfg_opt_eta").value = op.round_trip_eff ?? 0.9;
  $("cfg_opt_max").value = op.max_soc_percent ?? 100;
  $("cfg_profile").value = hw.profile || "powercore2";
  $("cfg_can").value = String(!!hw.can_listen);
  $("cfg_can_poll").value = String(!!hw.can_poll);
  $("cfg_meter").value = String(!!hw.meter_enabled);
  $("cfg_poll").value = hw.poll_interval_secs ?? 5;
  $("cfg_device_name").value = dv.name || "Power Core";
  $("cfg_display").value = String(hw.display_enabled !== false);
  if ($("p_reserve")) $("p_reserve").value = po.reserve_percent ?? 20;
}

function renderSolarStrings(list) {
  const t = $("cfg_solar_strings");
  t.querySelectorAll("tr.ss").forEach((e) => e.remove());
  list.forEach((st) => {
    const tr = document.createElement("tr");
    tr.className = "ss";
    tr.innerHTML = '<td><input class="ss_name" value="' + esc(st.name || "") + '"></td>'
      + '<td><input class="ss_panels" type="number" value="' + (st.panels || 0) + '"></td>'
      + '<td><input class="ss_wp" type="number" value="' + (st.panel_wp || 0) + '"></td>'
      + '<td><input class="ss_tilt" type="number" value="' + (st.tilt_deg || 0) + '"></td>'
      + '<td><input class="ss_az" type="number" value="' + (st.azimuth_deg || 0) + '"></td>'
      + '<td><select class="ss_bi"><option value="true">yes</option><option value="false">no</option></select></td>'
      + '<td><button class="ghost" class="rm">×</button></td>';
    t.appendChild(tr);
    tr.querySelector(".ss_bi").value = String(!!st.bifacial);
    tr.querySelector(".rm").addEventListener("click", () => tr.remove());
  });
}

function readSolarStrings() {
  const out = [];
  $("cfg_solar_strings").querySelectorAll("tr.ss").forEach((tr) => {
    out.push({
      name: tr.querySelector(".ss_name").value,
      panels: parseInt(tr.querySelector(".ss_panels").value || "0"),
      panel_wp: parseInt(tr.querySelector(".ss_wp").value || "0"),
      tilt_deg: parseFloat(tr.querySelector(".ss_tilt").value || "0"),
      azimuth_deg: parseFloat(tr.querySelector(".ss_az").value || "0"),
      bifacial: tr.querySelector(".ss_bi").value === "true",
    });
  });
  return out;
}

async function saveSettings() {
  if (!CFG) return;
  const c = JSON.parse(JSON.stringify(CFG));
  const b = (id) => $(id).value === "true";
  const n = (id, d) => { const v = parseFloat($(id).value); return isNaN(v) ? d : v; };
  c.mqtt = Object.assign(c.mqtt || {}, {
    broker_url: $("cfg_mqtt_on").checked ? $("cfg_mqtt_url").value : "",
    topic_prefix: $("cfg_mqtt_prefix").value,
    ha_discovery: b("cfg_mqtt_disc"),
    discovery_prefix: $("cfg_mqtt_dprefix").value,
  });
  c.prices = Object.assign(c.prices || {}, {
    enabled: b("cfg_prices_enabled"), area: $("cfg_prices_area").value,
    currency: $("cfg_prices_currency").value, tz_offset_secs: n("cfg_prices_tz", 3600),
    refresh_secs: n("cfg_prices_refresh", 21600),
  });
  const so = c.solar || {};
  so.enabled = b("cfg_solar_enabled");
  so.latitude = n("cfg_solar_lat", 0);
  so.longitude = n("cfg_solar_lon", 0);
  so.performance_ratio = n("cfg_solar_pr", 0.8);
  so.strings = readSolarStrings();
  c.solar = so;
  const pol = c.policy || {};
  pol.reserve_percent = n("cfg_reserve", 20);
  pol.optimizer = Object.assign(pol.optimizer || {}, {
    min_spread_eur_kwh: n("cfg_opt_spread", 0.03),
    round_trip_eff: n("cfg_opt_eta", 0.9), max_soc_percent: n("cfg_opt_max", 100),
  });
  c.policy = pol;
  c.hardware = Object.assign(c.hardware || {}, {
    profile: $("cfg_profile").value,
    can_listen: b("cfg_can"), can_poll: b("cfg_can_poll"), meter_enabled: b("cfg_meter"),
    poll_interval_secs: n("cfg_poll", 5), display_enabled: b("cfg_display"),
  });
  c.device = Object.assign(c.device || {}, { name: $("cfg_device_name").value });
  const r = await call("/api/v1/config", { config: c });
  const j = r.j || {};
  msg("cfg_msg", r.ok ? "saved" : ("failed: " + (j.error || r.status)), r.ok ? "" : "warn");
  if (r.ok) { CFG = j; $("s_cfg").textContent = JSON.stringify(j, null, 2); }
}

function wireDriverSettings() {
  $("cfg_mqtt_on").addEventListener("change", () => { $("cfg_mqtt_box").hidden = !$("cfg_mqtt_on").checked; });
  $("discover-broker").addEventListener("click", async () => {
    const r = await call("/api/v1/broker");
    const j = r.j || {};
    if (j.found) { $("cfg_mqtt_url").value = j.broker_url; msg("cfg_mqtt_hint", "found broker at " + j.candidate); }
    else { if (j.suggested && !$("cfg_mqtt_url").value) $("cfg_mqtt_url").value = j.suggested; msg("cfg_mqtt_hint", "no broker found locally; using a guess — edit if needed", "warn"); }
  });
  $("add-string").addEventListener("click", () => renderSolarStrings([...readSolarStrings(), { name: "", panels: 0, panel_wp: 0, tilt_deg: 30, azimuth_deg: 180, bifacial: false }]));
  $("save-settings").addEventListener("click", saveSettings);
  $("reload-config").addEventListener("click", loadConfig);
  $("do-walk").addEventListener("click", async () => {
    msg("walk-msg", "running…");
    const r = await call("/api/v1/walk", {});
    msg("walk-msg", r.ok ? "started — result in Events" : "failed: " + r.status, r.ok ? "" : "warn");
  });
}

function applyPolicy() {
  const mode = $("p_mode").value;
  const p = { mode, charge: +$("p_charge").value || 0, discharge: +$("p_discharge").value || 0, reserve: +$("p_reserve").value || 0 };
  if (ROLE === "bridge") {
    // The bridge engine reads its plan mode from config; the limits are the
    // module's own settings, so they travel as the vendor settings command.
    postSetting("/api/v1/config", { "policy.mode": mode, "policy.reserve_percent": p.reserve }, "p-msg");
    return;
  }
  msg("p-msg", "applying…");
  call("/api/v1/cmd", { cmd: "set_mode", args: { mode } })
    .then(() => call("/api/v1/cmd", { cmd: "set_charge_limit", args: { watts: p.charge } }))
    .then(() => call("/api/v1/cmd", { cmd: "set_discharge_limit", args: { watts: p.discharge } }))
    .then((r) => msg("p-msg", r.ok ? "applied" : "failed: " + r.status, r.ok ? "" : "warn"));
}

// ---------------------------------------------------------------- events / logs

async function loadEvents() {
  const r = await call("/api/v1/events");
  if (!r.ok || !Array.isArray(r.j)) return;
  $("s_events").textContent = r.j.map((e) =>
    new Date((e.time || e.ts) * 1000).toLocaleString() + " [" + (e.level || e.lvl) + "] " + (e.source || e.tag) + ": " + (e.message || e.msg)
  ).join("\n") || "(no events yet)";
  setTimeout(loadEvents, 15000);
  setTimeout(loadHistory, 120000);
}

async function loadLogs() {
  const q = encodeURIComponent($("logq").value || "");
  const r = await call("/api/v1/logs?n=300&q=" + q);
  const j = r.j || {};
  const lines = (j.lines || []).concat((j.events || []).map((e) => `[ev] ${new Date(e.ts * 1000).toLocaleTimeString()} ${e.lvl} ${e.tag}: ${e.msg}`));
  $("log").textContent = r.ok ? lines.join("\n") : ((j.error || ("HTTP " + r.status)) + " — the driver role writes events, not a log file");
}

// ---------------------------------------------------------------- wiring

document.querySelectorAll("nav button").forEach((b) => b.addEventListener("click", () => {
  document.querySelectorAll("nav button").forEach((x) => x.classList.toggle("on", x === b));
  show(b.dataset.tab);
  if (b.dataset.tab === "schedule") loadSchedule();
  if (b.dataset.tab === "settings") { if (ROLE === "bridge") { loadSchema(); fillSettingsForm(snap && snap.telemetry); } else loadConfig(); }
  if (b.dataset.tab === "system") { if (ROLE === "bridge") loadSystem(); }
  if (b.dataset.tab === "logs") loadLogs();
  if (b.dataset.tab === "plan") loadStatus();
}));

$("do-setup").addEventListener("click", doSetup);
$("do-login").addEventListener("click", doLogin);
$("li_pw").addEventListener("keydown", (e) => { if (e.key === "Enter") doLogin(); });
$("logout").addEventListener("click", doLogout);
$("apply-policy").addEventListener("click", applyPolicy);
$("switch-role-all").addEventListener("click", async () => {
  const target = ROLE === "bridge" ? "driver" : "bridge";
  if (!confirm("Hand the box to the " + target + " role? The vendor app is parked for the driver role; the page reloads on the same address.")) return;
  msg("role-msg", "switching…");
  const r = await call("/api/v1/role", { role: target });
  const j = r.j || {};
  msg("role-msg", r.ok && j.ok ? "starting " + j.unit : "failed: " + (j.error || r.status), r.ok && j.ok ? "" : "warn");
  if (r.ok && j.ok) setTimeout(() => location.reload(), 6000);
});
$("restart-service").addEventListener("click", async () => {
  if (!confirm("Restart this service (the bridge stops for a few seconds)?")) return;
  await call("/api/v1/restart", { target: "bridge" });
  msg("sys-msg", "restarting…");
  setTimeout(() => location.reload(), 5000);
});
$("save-config").addEventListener("click", async () => {
  const body = {};
  document.querySelectorAll("#config input").forEach((i) => {
    const k = i.id.slice(4);
    body[k] = /^\d+$/.test(i.value) ? +i.value : i.value;
  });
  const r = await call("/api/v1/config", body);
  const j = r.j || {};
  msg("cfg-msg2", r.ok ? "saved" + (j.rejected && j.rejected.length ? " (rejected: " + j.rejected.join(", ") + ")" : "") : "failed: " + (j.error || r.status), r.ok ? "" : "warn");
});
$("save-pw").addEventListener("click", async () => {
  const r = await call("/api/v1/password", { current: $("pw-cur").value, new: $("pw-new").value });
  const j = r.j || {};
  msg("pw-msg", r.ok ? "password changed" : "failed: " + (j.error || r.status), r.ok ? "" : "warn");
  $("pw-cur").value = ""; $("pw-new").value = "";
});
$("log-refresh").addEventListener("click", loadLogs);
$("logq").addEventListener("keydown", (e) => { if (e.key === "Enter") loadLogs(); });

wireBridgeSettings();
wireDriverSettings();
boot();

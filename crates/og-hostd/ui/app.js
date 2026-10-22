// OG-Host web UI. Plain ES modules, no build step, no third-party code.
// One EventSource carries everything live (stats every 2 s, container and brain changes);
// views only repaint what changed.

// ---------------------------------------------------------------- utilities
const $ = (s, el = document) => el.querySelector(s);
const $$ = (s, el = document) => [...el.querySelectorAll(s)];
const esc = (v) => String(v ?? '').replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));
const clamp = (v, a, b) => Math.max(a, Math.min(b, v));

const UNITS = ['B', 'KB', 'MB', 'GB', 'TB', 'PB'];
function bytes(n, d = 1) {
  if (n == null || isNaN(n)) return '–';
  let i = 0; n = Number(n);
  while (Math.abs(n) >= 1024 && i < UNITS.length - 1) { n /= 1024; i++; }
  return `${n.toFixed(i === 0 || n >= 100 ? 0 : d)} ${UNITS[i]}`;
}
const rate = (n) => (n == null ? '–' : `${bytes(n)}/s`);
const pct = (v, d = 0) => (v == null || isNaN(v) ? '–' : `${Number(v).toFixed(d)}%`);
function dur(s) {
  if (s == null || s < 0) return '–';
  s = Math.floor(s);
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m`;
  if (s < 86400) return `${Math.floor(s / 3600)}h ${Math.floor((s % 3600) / 60)}m`;
  return `${Math.floor(s / 86400)}d ${Math.floor((s % 86400) / 3600)}h`;
}
const now = () => Date.now() / 1000;
const ago = (t) => (t ? `${dur(now() - t)} ago` : '–');
const clock = (t) => new Date(t * 1000).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
function parseSize(s) {
  if (s == null || String(s).trim() === '') return null;
  const m = String(s).trim().toLowerCase().match(/^([\d.]+)\s*([kmgt]?)i?b?$/);
  if (!m) return NaN;
  return Math.round(parseFloat(m[1]) * { '': 1, k: 1024, m: 1024 ** 2, g: 1024 ** 3, t: 1024 ** 4 }[m[2]]);
}

// ---------------------------------------------------------------- icons (24px stroke)
const ICONS = {
  grid: '<rect x="3" y="3" width="7" height="7" rx="1.5"/><rect x="14" y="3" width="7" height="7" rx="1.5"/><rect x="3" y="14" width="7" height="7" rx="1.5"/><rect x="14" y="14" width="7" height="7" rx="1.5"/>',
  box: '<path d="M21 8 12 3 3 8v8l9 5 9-5z"/><path d="m3 8 9 5 9-5M12 13v8"/>',
  layers: '<path d="m12 2 10 5-10 5L2 7z"/><path d="m2 12 10 5 10-5M2 17l10 5 10-5"/>',
  disc: '<circle cx="12" cy="12" r="9"/><circle cx="12" cy="12" r="2.5"/>',
  drive: '<path d="M22 12H2M5.5 5.1 2 12v6a2 2 0 0 0 2 2h16a2 2 0 0 0 2-2v-6l-3.5-6.9A2 2 0 0 0 16.8 4H7.2a2 2 0 0 0-1.7 1.1z"/><path d="M6 16h.01M10 16h.01"/>',
  logs: '<path d="M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8z"/><path d="M14 2v6h6M16 13H8M16 17H8M10 9H8"/>',
  monitor: '<rect x="2" y="3" width="20" height="14" rx="2"/><path d="M8 21h8M12 17v4"/>',
  search: '<circle cx="11" cy="11" r="7"/><path d="m20 20-3.5-3.5"/>',
  filter: '<path d="M3 5h18l-7 8v6l-4 2v-8z"/>',
  user: '<circle cx="12" cy="8" r="4"/><path d="M4 21a8 8 0 0 1 16 0"/>',
  plus: '<path d="M12 5v14M5 12h14"/>',
  play: '<path d="M7 4v16l13-8z"/>',
  stop: '<rect x="6" y="6" width="12" height="12" rx="1.5"/>',
  restart: '<path d="M3 12a9 9 0 1 0 3-6.7L3 8"/><path d="M3 3v5h5"/>',
  trash: '<path d="M3 6h18M8 6V4h8v2M19 6l-1 14H6L5 6M10 11v6M14 11v6"/>',
  more: '<circle cx="5" cy="12" r="1"/><circle cx="12" cy="12" r="1"/><circle cx="19" cy="12" r="1"/>',
  x: '<path d="M18 6 6 18M6 6l12 12"/>',
  logout: '<path d="M9 21H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h4M16 17l5-5-5-5M21 12H9"/>',
  edit: '<path d="M17 3a2.8 2.8 0 1 1 4 4L7.5 20.5 2 22l1.5-5.5z"/>',
  download: '<path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4M7 10l5 5 5-5M12 15V3"/>',
  shrink: '<path d="M4 14h6v6M20 10h-6V4M14 10l7-7M3 21l7-7"/>',
  expand: '<path d="M15 3h6v6M9 21H3v-6M21 3l-7 7M3 21l7-7"/>',
  cpu: '<rect x="5" y="5" width="14" height="14" rx="2"/><rect x="9" y="9" width="6" height="6"/><path d="M9 2v3M15 2v3M9 19v3M15 19v3M2 9h3M2 15h3M19 9h3M19 15h3"/>',
  mem: '<rect x="2" y="7" width="20" height="10" rx="2"/><path d="M6 7v10M10 7v10M14 7v10M18 7v10M2 20h20"/>',
  net: '<path d="M12 3v12M7 10l5 5 5-5"/><path d="M5 21h14"/>',
  activity: '<path d="M22 12h-4l-3 9L9 3l-3 9H2"/>',
  gpu: '<rect x="2" y="6" width="20" height="12" rx="2"/><circle cx="9" cy="12" r="3"/><path d="M16 10h3M16 14h3M5 18v3M19 18v3"/>',
  zap: '<path d="M13 2 3 14h9l-1 8 10-12h-9z"/>',
  moon: '<path d="M21 12.8A9 9 0 1 1 11.2 3a7 7 0 0 0 9.8 9.8z"/>',
  alert: '<path d="M10.3 3.9 1.8 18a2 2 0 0 0 1.7 3h17a2 2 0 0 0 1.7-3L13.7 3.9a2 2 0 0 0-3.4 0z"/><path d="M12 9v4M12 17h.01"/>',
  check: '<path d="M20 6 9 17l-5-5"/>',
  terminal: '<path d="m4 17 6-6-6-6M12 19h8"/>',
  copy: '<rect x="9" y="9" width="12" height="12" rx="2"/><path d="M5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1"/>',
  game: '<rect x="2" y="7" width="20" height="11" rx="4"/><path d="M7 11v3M5.5 12.5h3M15 12h.01M18 13h.01"/>',
  shield: '<path d="M12 22s8-4 8-10V5l-8-3-8 3v7c0 6 8 10 8 10z"/>',
  sliders: '<path d="M4 21v-7M4 10V3M12 21v-9M12 8V3M20 21v-5M20 12V3M1 14h6M9 8h6M17 16h6"/>',
  file: '<path d="M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8z"/><path d="M14 2v6h6"/>',
  hdd: '<rect x="3" y="4" width="18" height="16" rx="2"/><path d="M7 15h.01M3 12h18"/>',
  link: '<path d="M10 13a5 5 0 0 0 7.5.5l3-3a5 5 0 0 0-7-7l-1.7 1.7"/><path d="M14 11a5 5 0 0 0-7.5-.5l-3 3a5 5 0 0 0 7 7l1.7-1.7"/>',
  eye: '<path d="M2 12s3.5-7 10-7 10 7 10 7-3.5 7-10 7S2 12 2 12z"/><circle cx="12" cy="12" r="3"/>',
  eyeoff: '<path d="M3 3l18 18M10.6 5.1A10 10 0 0 1 12 5c6.5 0 10 7 10 7a17 17 0 0 1-3 3.9M6.6 6.6A17 17 0 0 0 2 12s3.5 7 10 7a9.7 9.7 0 0 0 5.4-1.6M9.9 9.9a3 3 0 0 0 4.2 4.2"/>',
  sparkle: '<path d="M12 3l1.8 5.2L19 10l-5.2 1.8L12 17l-1.8-5.2L5 10l5.2-1.8z"/><path d="M19 15l.8 2.2L22 18l-2.2.8L19 21l-.8-2.2L16 18l2.2-.8z"/>',
  chevron: '<path d="m6 9 6 6 6-6"/>',
  chevronr: '<path d="m9 6 6 6-6 6"/>',
  globe: '<circle cx="12" cy="12" r="9"/><path d="M3 12h18M12 3a14 14 0 0 1 0 18M12 3a14 14 0 0 0 0 18"/>',
  send: '<path d="M12 19V5M5 12l7-7 7 7"/>',
  pin: '<path d="M12 17v5M9 3h6l-1 6 4 4H6l4-4z"/>',
  server: '<rect x="3" y="4" width="18" height="7" rx="1.5"/><rect x="3" y="13" width="18" height="7" rx="1.5"/><path d="M7 7.5h.01M7 16.5h.01"/>',
  servers: '<rect x="2" y="3" width="20" height="6" rx="1.5"/><rect x="2" y="15" width="20" height="6" rx="1.5"/><path d="M6 6h.01M6 18h.01M12 9v6"/>',
  bell: '<path d="M18 8a6 6 0 0 0-12 0c0 7-3 9-3 9h18s-3-2-3-9M13.7 21a2 2 0 0 1-3.4 0"/>',
  belloff: '<path d="M3 3l18 18M8.7 3.4A6 6 0 0 1 18 8c0 3 .7 5.2 1.5 6.7M17 17H3s3-2 3-9M13.7 21a2 2 0 0 1-3.4 0"/>',
  folder: '<path d="M3 7a2 2 0 0 1 2-2h4l2 3h8a2 2 0 0 1 2 2v7a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z"/>',
  up: '<path d="M12 19V5M5 12l7-7 7 7"/>',
  down: '<path d="M12 5v14M19 12l-7 7-7-7"/>',
  chart: '<path d="M3 3v18h18"/><path d="m7 15 4-5 3 3 5-6"/>',
  thermo: '<path d="M14 14.8V5a2 2 0 0 0-4 0v9.8a4 4 0 1 0 4 0z"/>',
  external: '<path d="M15 3h6v6M10 14 21 3M18 13v6a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2h6"/>',
  pause: '<rect x="6" y="5" width="4" height="14" rx="1"/><rect x="14" y="5" width="4" height="14" rx="1"/>',
  refresh: '<path d="M21 12a9 9 0 0 0-15-6.7L3 8"/><path d="M3 3v5h5M3 12a9 9 0 0 0 15 6.7L21 16"/><path d="M16 16h5v5"/>',
  shieldok: '<path d="M12 22s8-4 8-10V5l-8-3-8 3v7c0 6 8 10 8 10z"/><path d="m9 12 2 2 4-4"/>',
  note: '<path d="M4 4h16v12l-4 4H4z"/><path d="M16 20v-4h4M8 9h8M8 13h5"/>',
  drag: '<circle cx="9" cy="6" r="1"/><circle cx="15" cy="6" r="1"/><circle cx="9" cy="12" r="1"/><circle cx="15" cy="12" r="1"/><circle cx="9" cy="18" r="1"/><circle cx="15" cy="18" r="1"/>',
  mode: '<path d="M12 3v18M3 12h18"/><circle cx="12" cy="12" r="9"/>',
  sleep: '<path d="M21 12.8A9 9 0 1 1 11.2 3a7 7 0 0 0 9.8 9.8z"/>',
};
const ic = (n, cls = '') => `<svg class="i ${cls}" viewBox="0 0 24 24" aria-hidden="true">${ICONS[n] || ''}</svg>`;
function hydrateIcons(root = document) {
  for (const el of $$('[data-icon]', root)) {
    if (el.dataset.iconDone) continue;
    el.dataset.iconDone = 1;
    el.insertAdjacentHTML('afterbegin', ic(el.dataset.icon));
  }
}

// ---------------------------------------------------------------- toast, api
let toastTimer;
function toast(msg, bad = false) {
  const t = $('#toast');
  t.textContent = msg;
  t.classList.toggle('bad', bad);
  t.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => (t.hidden = true), bad ? 6000 : 3000);
}

async function api(path, { method = 'GET', body, raw = false } = {}) {
  const opts = { method, headers: { 'X-OG-UI': '1' } };
  if (body !== undefined) { opts.headers['Content-Type'] = 'application/json'; opts.body = JSON.stringify(body); }
  const r = await fetch(path, opts);
  if (r.status === 401 && !path.startsWith('/v1/ui/')) { showLogin(); throw new Error('signed out'); }
  const text = await r.text();
  let data = text;
  if (!raw) { try { data = text ? JSON.parse(text) : null; } catch { /* plain text */ } }
  if (!r.ok) {
    const msg = (data && (data.error || data.message)) || text || r.statusText;
    throw Object.assign(new Error(msg), { status: r.status, data });
  }
  return data;
}

// ---------------------------------------------------------------- state
const S = {
  info: null,
  stats: null, gpus: [],
  containers: [], byId: new Map(),
  brain: null,
  hist: [], chist: {},
  view: 'overview',
  ct: { q: '', filter: 'all', group: false, sort: 'name', dir: 1 },
  hist_series: 'cpu', hist_range: 900,
  drawer: null, drawerTab: 'info',
  log: { id: null, es: null, lines: [], level: 'all', q: '', follow: true, wrap: false },
};

// ---------------------------------------------------------------- sign in
async function boot() {
  hydrateIcons();
  try {
    await api('/v1/ui/me');
    startApp();
  } catch (e) {
    showLogin(e.data && e.data.setup_needed);
  }
}

let tfaTicket = null;
function loginStep(two) {
  tfaTicket = two ? tfaTicket : null;
  $('#loginStep1').hidden = two; $('#loginStep2').hidden = !two; $('#loginBack').hidden = !two;
  $('#loginUser').required = !two; $('#loginPass').required = !two;
  $('#loginBtn').textContent = two ? 'Verify' : 'Sign in';
  (two ? $('#loginCode') : $('#loginUser')).focus();
}
function showLogin(setup = false) {
  if (S.es) { S.es.close(); S.es = null; }
  $('#app').hidden = true;
  $('#login').hidden = false;
  $('#loginSetup').hidden = !setup;
  try { $('#loginRealm').value = localStorage.getItem('og.realm') || 'local'; } catch { /* private window */ }
  loginStep(false);
}
$('#loginBack').addEventListener('click', () => { $('#loginErr').textContent = ''; loginStep(false); });

$('#loginForm').addEventListener('submit', async (ev) => {
  ev.preventDefault();
  const btn = $('#loginBtn'); btn.disabled = true; $('#loginErr').textContent = '';
  try {
    if (tfaTicket) {
      await api('/v1/ui/tfa', { method: 'POST', body: { ticket: tfaTicket, code: $('#loginCode').value.trim() } });
    } else {
      const realm = $('#loginRealm').value;
      try { localStorage.setItem('og.realm', realm); } catch { /* private window */ }
      const r = await api('/v1/ui/login', { method: 'POST', body: { user: $('#loginUser').value.trim(), password: $('#loginPass').value, realm } });
      $('#loginPass').value = '';
      if (r && r.tfa_required) { tfaTicket = r.ticket; $('#loginCode').value = ''; loginStep(true); return; }
    }
    $('#loginCode').value = ''; tfaTicket = null;
    startApp();
  } catch (e) {
    $('#loginErr').textContent = e.message;
    if (tfaTicket && e.status === 401 && /expired/.test(e.message)) loginStep(false);
  } finally { btn.disabled = false; }
});

let started = false;
async function startApp() {
  $('#login').hidden = true;
  $('#app').hidden = false;
  connect();
  const again = started;
  started = true;
  if (again) { try { S.me = await api('/v1/ui/me'); } catch { S.me = null; } setNav(NAV_ALL, SUB_ALL); route(); return; }
  try { S.me = await api('/v1/ui/me'); } catch { S.me = null; }
  try { (await import('./access.js')).init(H); } catch (e) { console.error(e); }
  try {
    const info = await api('/v1/info');
    S.info = S.info || info;
    S.lab = !!info.lab;
    S.labHost = info.lab_host || info.hostname;
  } catch { S.lab = false; }
  try { (await import('./backups.js')).init(H); } catch (e) { console.error(e); }
  if (S.lab) {
    try { (await import('./lab.js')).init(H); }
    catch (e) { console.error(e); S.lab = false; toast(`Lab UI failed to load: ${e.message}`, true); }
  }
  if (!S.lab) setNav(SERVER_TABS);
  route();
}

// ---------------------------------------------------------------- live connection
function connect() {
  if (S.es) S.es.close();
  const es = new EventSource('/v1/events');
  S.es = es;
  const live = $('#live');
  es.onopen = () => { live.className = 'live on'; live.title = 'Live'; };
  es.onerror = async () => {
    live.className = 'live off'; live.title = 'Reconnecting…';
    // a 401 shows up as an error too: check, and sign in again if so
    try { await api('/v1/ui/me'); } catch (e) { if (e.status === 401) { es.close(); showLogin(); } }
  };
  es.addEventListener('info', (e) => { S.info = JSON.parse(e.data); paintInfo(); });
  es.addEventListener('history', (e) => {
    const h = JSON.parse(e.data);
    S.hist = h.node || [];
    S.chist = h.containers || {};
  });
  es.addEventListener('stats', (e) => {
    const d = JSON.parse(e.data);
    S.stats = d.stats; S.gpus = d.gpus || [];
    const n = d.stats.node;
    S.hist.push({ t: d.stats.t, cpu: n.cpu, mem: n.mem.used, rx: n.net.rx, tx: n.net.tx, dr: n.disk.r, dw: n.disk.w });
    if (S.hist.length > 1800) S.hist.splice(0, S.hist.length - 1800);
    for (const [id, c] of Object.entries(d.stats.containers)) {
      const h = (S.chist[id] ||= []);
      h.push([d.stats.t, c.cpu, c.mem]);
      if (h.length > 150) h.shift();
    }
    onStats();
  });
  es.addEventListener('containers', (e) => {
    S.containers = JSON.parse(e.data);
    S.byId = new Map(S.containers.map((c) => [c.id, c]));
    onContainers();
  });
  es.addEventListener('brain', (e) => { S.brain = JSON.parse(e.data); if (S.view === 'overview') paintBrain(); });
}

function paintInfo() {
  const i = S.info;
  $('#hostChip').textContent = i.hostname;
  document.title = `${i.hostname} · OG-Host`;
  if (S.view === 'overview') paintOverviewHead();
}

function onStats() {
  if (S.view === 'overview') paintOverview();
  else if (S.view === 'containers') paintContainerStats();
  else if (S.view === 'storage') paintDisks();
  if (S.drawer && S.drawerTab === 'info') paintDrawerLive();
  if (H.onStats) H.onStats();
}

function onContainers() {
  const bad = S.containers.filter((c) => c.state === 'failed' || (c.oom_killed && c.state !== 'running')).length;
  const b = $('#ctBadge'); if (b) { b.hidden = !bad; b.textContent = bad; }
  if (S.view === 'containers') paintContainers();
  if (S.view === 'stacks') loadStacks();
  if (S.view === 'logs' || S.view === 'ctlogs') paintLogSources();
  if (S.view === 'overview') paintOverview();
  if (H.onContainers) H.onContainers();
  if (S.drawer) {
    const c = S.byId.get(S.drawer);
    if (!c) closeDrawer(); else paintDrawerHead(c);
  }
}

// ---------------------------------------------------------------- routing
// hash -> { el: section id, tab: highlighted top tab, run(arg) }. lab.js adds its own.
const ROUTES = {
  overview: { el: 'overview', tab: 'overview', run: () => { paintOverviewHead(); paintOverview(); paintBrain(); } },
  containers: { el: 'containers', tab: 'containers', run: paintContainers },
  stacks: { el: 'stacks', tab: 'stacks', run: () => loadStacks() },
  images: { el: 'images', tab: 'images', run: () => loadImages() },
  storage: { el: 'storage', tab: 'storage', run: () => { paintDisks(); loadStorage(); } },
  logs: { el: 'logs', tab: 'logs', run: (arg) => { paintLogSources(); if (arg) openLog(arg); else if (S.log.id) openLog(S.log.id); } },
  vms: { el: 'vms', tab: 'vms', run: () => {} },
};
const SERVER_TABS = [['overview', 'grid', 'Overview'], ['containers', 'box', 'Containers'], ['stacks', 'layers', 'Stacks'], ['images', 'disc', 'Images'], ['storage', 'drive', 'Storage'], ['backups', 'shield', 'Backups'], ['logs', 'logs', 'Logs'], ['vms', 'monitor', 'VMs']];
let NAV = SERVER_TABS;
function defaultView() { const v = S.lab ? 'home' : 'overview'; return allowedTab([v]) ? v : (NAV[0] ? NAV[0][0] : 'settings'); }
// what a page needs (held anywhere): pages the caller can't use are left out
const NEEDS = { backups: 'Datastore.Audit', overview: 'Sys.Audit', server: 'Sys.Audit', images: 'Sys.Audit', storage: 'Sys.Audit', containers: 'Container.Audit', ctlogs: 'Container.Audit', stacks: 'Stack.Audit', vms: 'VM.Audit', home: 'Lab.Audit', monitor: 'Lab.Audit', explore: 'Lab.Audit', watch: 'Lab.Audit', apps: 'Lab.Audit' };
let NAV_ALL = SERVER_TABS, SUB_ALL = [];
const allowedTab = ([id]) => !S.me || !NEEDS[id] || can(NEEDS[id], '*') || (id === 'logs' && (can('Lab.Audit', '*') || can('Container.Audit', '*')));
function setNav(tabs, sub) {
  NAV_ALL = tabs; SUB_ALL = sub || [];
  tabs = tabs.filter(allowedTab); sub = (sub || []).filter(allowedTab);
  NAV = tabs;
  $('#tabs').innerHTML = tabs.map(([id, icon, label]) => `<a href="#${id}" data-tab="${id}" class="tab">${ic(icon)}<span>${esc(label)}</span>${id === 'containers' || id === 'server' ? '<b class="tab-badge" id="ctBadge" hidden></b>' : ''}${id === 'monitor' ? '<b class="tab-badge" id="alertBadge" hidden></b>' : ''}${id === 'apps' ? '<b class="tab-badge info" id="queueBadge" hidden></b>' : ''}</a>`).join('');
  $('#subnav').innerHTML = (sub || []).map(([id, icon, label]) => `<a href="#${id}" data-sub="${id}">${ic(icon)}<span>${esc(label)}</span></a>`).join('');
}
let curView = null;
function route() {
  const h = location.hash.replace(/^#/, '');
  const [view, ...rest] = h.split('/');
  const name = ROUTES[view] ? view : defaultView();
  const r = ROUTES[name];
  const leaving = curView && curView !== name ? ROUTES[curView] : null;
  if (leaving && leaving.leave) leaving.leave();
  curView = name;
  S.view = name;
  for (const v of $$('main > .view')) v.hidden = v.id !== `view-${r.el}`;
  for (const t of $$('#tabs .tab')) t.classList.toggle('active', t.dataset.tab === r.tab);
  const sub = $('#subnav');
  sub.hidden = !(S.lab && r.tab === 'server');
  for (const a of $$('#subnav a')) a.classList.toggle('on', a.dataset.sub === name);
  if (S.view !== 'logs' && S.log.es) { S.log.es.close(); S.log.es = null; }
  r.run(rest.length ? rest.map(decodeURIComponent).join('/') : undefined, rest.map(decodeURIComponent));
}
window.addEventListener('hashchange', route);

// ---------------------------------------------------------------- charts
function sparkPath(vals, w = 100, h = 30, max) {
  if (!vals.length) return { line: '', area: '' };
  const m = max ?? Math.max(...vals, 1e-9);
  const step = vals.length > 1 ? w / (vals.length - 1) : w;
  const pts = vals.map((v, i) => [i * step, h - clamp(v / m, 0, 1) * (h - 2) - 1]);
  const line = pts.map((p, i) => `${i ? 'L' : 'M'}${p[0].toFixed(1)},${p[1].toFixed(1)}`).join('');
  return { line, area: `${line}L${w},${h}L0,${h}Z` };
}
function spark(vals, { max, cls = '', b } = {}) {
  const a = sparkPath(vals, 100, 30, max);
  const bb = b ? sparkPath(b, 100, 30, max) : null;
  return `<svg class="spark ${cls}" viewBox="0 0 100 30" preserveAspectRatio="none"><path class="area" d="${a.area}"/><path class="line" d="${a.line}"/>${bb ? `<path class="area b" d="${bb.area}"/><path class="line b" d="${bb.line}"/>` : ''}</svg>`;
}

const SERIES = {
  cpu: { label: 'CPU', keys: ['cpu'], fmt: (v) => pct(v, 1), max: 100 },
  mem: { label: 'Memory', keys: ['mem'], fmt: bytes, max: () => S.info?.mem_total },
  net: { label: 'Network', keys: ['rx', 'tx'], names: ['in', 'out'], fmt: rate },
  disk: { label: 'Disk', keys: ['dr', 'dw'], names: ['read', 'write'], fmt: rate },
};
let chartData = null;
async function drawHistory() {
  const range = S.hist_range;
  let pts;
  if (range <= 3600) {
    const cut = now() - range;
    pts = S.hist.filter((p) => p.t >= cut);
  } else {
    const m = await api(`/v1/metrics?range=${range}`);
    pts = m.t.map((t, i) => ({ t, cpu: m.cpu[i], mem: m.mem[i], rx: m.rx[i], tx: m.tx[i], dr: m.dr[i], dw: m.dw[i] }));
  }
  chartData = pts;
  renderChart();
}
function renderChart(hoverX) {
  const cv = $('#histChart');
  if (!cv || cv.offsetParent === null) return;
  const dpr = window.devicePixelRatio || 1;
  const W = cv.clientWidth, H = cv.clientHeight;
  if (cv.width !== W * dpr || cv.height !== H * dpr) { cv.width = W * dpr; cv.height = H * dpr; }
  const g = cv.getContext('2d');
  g.setTransform(dpr, 0, 0, dpr, 0, 0);
  g.clearRect(0, 0, W, H);
  const css = getComputedStyle(document.documentElement);
  const col = { line: css.getPropertyValue('--line').trim(), text: css.getPropertyValue('--text-3').trim(), a: css.getPropertyValue('--accent').trim(), b: css.getPropertyValue('--accent-2').trim() };
  const pts = chartData || [];
  const ser = SERIES[S.hist_series];
  const padL = 64, padR = 10, padT = 10, padB = 22;
  const w = W - padL - padR, h = H - padT - padB;
  let max = typeof ser.max === 'function' ? ser.max() : ser.max;
  if (!max) max = Math.max(1, ...pts.flatMap((p) => ser.keys.map((k) => p[k] || 0))) * 1.15;
  g.font = '11px ui-monospace, monospace'; g.fillStyle = col.text; g.strokeStyle = col.line; g.lineWidth = 1;
  for (let i = 0; i <= 4; i++) {
    const y = padT + (h * i) / 4;
    g.beginPath(); g.moveTo(padL, y + .5); g.lineTo(W - padR, y + .5); g.stroke();
    g.textAlign = 'right'; g.fillText(ser.fmt(max * (1 - i / 4)), padL - 8, y + 4);
  }
  if (pts.length < 2) { g.textAlign = 'center'; g.fillText('collecting…', padL + w / 2, padT + h / 2); return; }
  const t0 = pts[0].t, t1 = pts[pts.length - 1].t, span = Math.max(1, t1 - t0);
  const X = (t) => padL + ((t - t0) / span) * w;
  const Y = (v) => padT + h - clamp(v / max, 0, 1) * h;
  g.textAlign = 'center';
  for (let i = 0; i <= 4; i++) { const t = t0 + (span * i) / 4; g.fillText(clock(t), clamp(X(t), padL + 20, W - padR - 20), H - 5); }
  ser.keys.forEach((k, si) => {
    const c = si ? col.b : col.a;
    g.beginPath();
    pts.forEach((p, i) => (i ? g.lineTo(X(p.t), Y(p[k] || 0)) : g.moveTo(X(p.t), Y(p[k] || 0))));
    g.strokeStyle = c; g.lineWidth = 1.6; g.stroke();
    g.lineTo(X(t1), padT + h); g.lineTo(X(t0), padT + h); g.closePath();
    const grad = g.createLinearGradient(0, padT, 0, padT + h);
    grad.addColorStop(0, c + '40'); grad.addColorStop(1, c + '00');
    g.fillStyle = grad; g.fill();
  });
  const tip = $('#histTip');
  if (hoverX == null || hoverX < padL) { tip.hidden = true; return; }
  const tt = t0 + ((hoverX - padL) / w) * span;
  let best = pts[0];
  for (const p of pts) if (Math.abs(p.t - tt) < Math.abs(best.t - tt)) best = p;
  const x = X(best.t);
  g.strokeStyle = col.text; g.setLineDash([3, 3]); g.beginPath(); g.moveTo(x, padT); g.lineTo(x, padT + h); g.stroke(); g.setLineDash([]);
  tip.hidden = false;
  tip.innerHTML = `${clock(best.t)}<br>${ser.keys.map((k, i) => `${ser.names ? ser.names[i] + ' ' : ''}<b>${ser.fmt(best[k])}</b>`).join('<br>')}`;
  tip.style.left = `${clamp(x + 14, 0, W - 140)}px`; tip.style.top = '18px';
}
$('#histChart').addEventListener('mousemove', (e) => renderChart(e.offsetX));
$('#histChart').addEventListener('mouseleave', () => renderChart());
window.addEventListener('resize', () => renderChart());
$('#histSeries').addEventListener('click', (e) => {
  const b = e.target.closest('button'); if (!b) return;
  S.hist_series = b.dataset.s; segOn('#histSeries', b); renderChart();
});
$('#histRange').addEventListener('click', (e) => {
  const b = e.target.closest('button'); if (!b) return;
  S.hist_range = Number(b.dataset.r); segOn('#histRange', b); drawHistory().catch((er) => toast(er.message, true));
});
function segOn(sel, btn) { for (const b of $$(`${sel} button`)) b.classList.toggle('on', b === btn); }

// ---------------------------------------------------------------- overview
function paintOverviewHead() {
  const i = S.info; if (!i) return;
  $('#ovTitle').textContent = i.hostname;
  $('#ovSub').innerHTML = `<span>${esc(i.os || 'Linux')}</span><span class="chip">kernel ${esc(i.kernel)}</span><span class="chip">${i.cores} cores</span><span class="chip">${bytes(i.mem_total)} RAM</span>${i.virtual ? '<span class="chip warn" title="This server is itself a virtual machine">VM host</span>' : ''}<span class="muted">OG-Host ${esc(i.version)}</span>`;
}

function stateOf(c) {
  if (c.state === 'running') {
    if (c.health && c.health.status === 'unhealthy') return { cls: 'warn', text: 'unhealthy' };
    if (c.health && c.health.status === 'starting') return { cls: 'busy', text: 'starting' };
    return { cls: 'ok', text: 'running' };
  }
  if (c.desired === 'sleeping') return { cls: '', text: 'sleeping' };
  if (c.desired === 'evicted') return { cls: 'warn', text: 'evicted' };
  if (c.desired === 'preempted') return { cls: '', text: 'paused for gaming' };
  if (c.state === 'failed') return { cls: 'crit', text: 'failed' };
  if (c.oom_killed) return { cls: 'crit', text: 'out of memory' };
  if (c.state === 'created') return { cls: '', text: 'created' };
  if (c.exit_code && c.exit_code !== 0) return { cls: 'crit', text: `exited (${c.exit_code})` };
  return { cls: '', text: 'stopped' };
}

function paintOverview() {
  const st = S.stats; if (!st) return;
  const n = st.node, i = S.info || {};
  const last = (k, len = 150) => S.hist.slice(-len).map((p) => p[k] || 0);
  const memPct = n.mem.total ? (n.mem.used / n.mem.total) * 100 : 0;
  const psi = S.brain?.node?.psi_memory?.some10 ?? 0;
  const running = S.containers.filter((c) => c.state === 'running').length;
  const counts = {};
  for (const c of S.containers) { const s = stateOf(c).text; counts[s] = (counts[s] || 0) + 1; }
  const level = memPct > 92 || psi > 25 ? 'crit' : memPct > 80 || psi > 10 || n.cpu > 90 ? 'warn' : 'ok';
  const status = $('#ovStatus');
  status.className = `status-pill ${level}`;
  status.innerHTML = `<span class="dot dot-${level}"></span>${level === 'ok' ? 'Healthy' : psi > 10 ? 'Memory pressure' : n.cpu > 90 ? 'CPU busy' : 'Running hot'}<span class="muted">· up ${dur(n.uptime)}</span>`;
  const bar = (v, extra = '') => `<div class="bar ${v > 90 ? 'crit' : v > 75 ? 'warn' : ''} ${extra}"><i style="width:${clamp(v, 0, 100).toFixed(1)}%"></i></div>`;
  const fsMain = (n.fs || []).find((f) => f.mount === '/') || (n.fs || [])[0];
  $('#ovWidgets').innerHTML = `
    <div class="widget"><div class="widget-head">${ic('cpu')}CPU<span class="sp"></span><span>${i.cores || ''} cores</span></div>
      <div class="widget-big">${pct(n.cpu, 0)}<small>load ${n.load.map((l) => l.toFixed(2)).join(' · ')}</small></div>${spark(last('cpu'), { max: 100 })}</div>
    <div class="widget"><div class="widget-head">${ic('mem')}Memory<span class="sp"></span><span>${psi > 0.5 ? `pressure ${psi.toFixed(0)}%` : 'no pressure'}</span></div>
      <div class="widget-big">${bytes(n.mem.used)}<small>of ${bytes(n.mem.total)}</small></div>
      <div class="bar split"><i style="width:${memPct.toFixed(1)}%"></i><i class="cache" style="width:${clamp((n.mem.cached / n.mem.total) * 100, 0, 100 - memPct).toFixed(1)}%"></i></div>
      <div class="kv"><span>available</span><b>${bytes(n.mem.available)}</b></div>
      <div class="kv"><span>swap</span><b>${n.mem.swap_total ? `${bytes(n.mem.swap_used)} / ${bytes(n.mem.swap_total)}` : 'none'}</b></div></div>
    <div class="widget"><div class="widget-head">${ic('net')}Network<span class="sp"></span><span>physical</span></div>
      <div class="widget-big">${rate(n.net.rx)}<small>in · ${rate(n.net.tx)} out</small></div>${spark(last('rx'), { b: last('tx') })}</div>
    <div class="widget"><div class="widget-head">${ic('hdd')}Disk<span class="sp"></span><span>${fsMain ? esc(fsMain.mount) : ''}</span></div>
      <div class="widget-big">${fsMain ? pct((fsMain.used / fsMain.total) * 100) : '–'}<small>${fsMain ? `${bytes(fsMain.used)} of ${bytes(fsMain.total)}` : ''}</small></div>
      ${fsMain ? bar((fsMain.used / fsMain.total) * 100) : ''}
      <div class="kv"><span>read · write</span><b>${rate(n.disk.r)} · ${rate(n.disk.w)}</b></div></div>
    <div class="widget"><div class="widget-head">${ic('box')}Containers<span class="sp"></span><a href="#containers" class="link-btn">open</a></div>
      <div class="widget-big">${running}<small>of ${S.containers.length} running</small></div>
      <div class="chips">${Object.entries(counts).map(([k, v]) => `<span class="chip ${k === 'running' ? 'good' : /failed|memory|exited \(|unhealthy/.test(k) ? 'crit' : k === 'evicted' ? 'warn' : ''}">${v} ${esc(k)}</span>`).join('')}</div></div>`;
  paintGpus();
  paintTop();
  if (S.hist_range <= 3600) { const cut = now() - S.hist_range; chartData = S.hist.filter((p) => p.t >= cut); renderChart(); }
}

function paintGpus() {
  const g = S.gpus || [];
  $('#gpuWrap').hidden = !g.length;
  if (!g.length) return;
  const sessions = g.reduce((a, x) => a + (x.encoder_sessions || 0), 0);
  $('#gpuMeta').textContent = `NVENC ${sessions} / 8 sessions`;
  $('#gpuGrid').innerHTML = g.map((x) => {
    const used = (x.mem_used_mib / x.mem_total_mib) * 100;
    const users = S.containers.filter((c) => c.state === 'running' && (c.gpus || []).length);
    return `<div class="gpu"><div class="gpu-head"><span class="gpu-icon">${ic('gpu')}</span><div><b>${esc(x.name)}</b><small>GPU ${x.index} · ${x.temp_c ?? '–'}°C · ${x.power_w != null ? x.power_w.toFixed(0) + ' W' : '–'}</small></div></div>
      <div class="metric"><span>VRAM</span><div class="bar ${used > 90 ? 'crit' : used > 75 ? 'warn' : ''}"><i style="width:${used.toFixed(1)}%"></i></div><span class="val">${bytes(x.mem_used_mib * 1048576)} / ${bytes(x.mem_total_mib * 1048576)}</span></div>
      <div class="metric"><span>Load</span><div class="bar"><i style="width:${x.util_pct ?? 0}%"></i></div><span class="val">${pct(x.util_pct)}</span></div>
      <div class="metric"><span>NVENC</span><div class="bar ${x.encoder_sessions >= 8 ? 'crit' : ''}"><i style="width:${((x.encoder_sessions || 0) / 8) * 100}%"></i></div><span class="val">${x.encoder_sessions || 0} / 8</span></div>
      ${users.length ? `<div class="chips">${users.map((c) => `<span class="chip ${c.labels['og.gpu-class'] === 'gaming' ? 'good' : ''}">${esc(c.name)}</span>`).join('')}</div>` : ''}</div>`;
  }).join('');
}

function paintTop() {
  const cs = S.stats?.containers || {};
  const rows = Object.entries(cs).map(([id, s]) => ({ c: S.byId.get(id), s })).filter((r) => r.c);
  rows.sort((a, b) => b.s.cpu - a.s.cpu || b.s.mem - a.s.mem);
  $('#ovTop').innerHTML = rows.length
    ? rows.slice(0, 8).map((r) => `<div class="mini-row click" data-open="${r.c.id}"><b>${esc(r.c.name)}</b><span>${pct(r.s.cpu, 1)}</span><span>${bytes(r.s.mem)}</span></div>`).join('')
    : '<p class="muted">Nothing running.</p>';
}

function paintBrain() {
  const b = S.brain; if (!b) return;
  const pressured = b.containers.filter((c) => c.memory.squeezed_high).length;
  const protectedN = b.containers.filter((c) => c.class === 'critical' && c.running).length;
  $('#brainMeta').textContent = `${protectedN} protected · ${pressured} squeezed`;
  const log = b.log || [];
  $('#ovBrain').innerHTML = log.length
    ? log.slice(0, 10).map((l) => `<div class="log-row ${/ALERT/.test(l.message) ? 'alert' : ''}"><time>${clock(l.at)}</time><span>${esc(l.message)}</span></div>`).join('')
    : '<p class="muted">All quiet — no memory pressure, nothing put to sleep.</p>';
}

document.addEventListener('click', (e) => {
  const o = e.target.closest('[data-open]');
  if (o) openDrawer(o.dataset.open);
});

// ---------------------------------------------------------------- containers
function stats(id) { return S.stats?.containers?.[id]; }
function sortKey(c) {
  const s = stats(c.id);
  switch (S.ct.sort) {
    case 'image': return c.image;
    case 'cpu': return s ? s.cpu : -1;
    case 'mem': return s ? s.mem : -1;
    case 'up': return c.state === 'running' && c.started_at ? -c.started_at : -Infinity;
    default: return c.name;
  }
}
function filtered() {
  const q = S.ct.q.toLowerCase();
  let list = S.containers.filter((c) => {
    if (S.ct.filter === 'running' && c.state !== 'running') return false;
    if (S.ct.filter === 'stopped' && c.state === 'running') return false;
    return !q || c.name.toLowerCase().includes(q) || c.image.toLowerCase().includes(q) || (c.project || '').toLowerCase().includes(q);
  });
  list.sort((a, b) => { const x = sortKey(a), y = sortKey(b); return (x < y ? -1 : x > y ? 1 : 0) * S.ct.dir; });
  if (S.ct.group) list.sort((a, b) => (a.project || '~').localeCompare(b.project || '~'));
  return list;
}
function chipsFor(c) {
  const out = [];
  if (c.class && c.class !== 'normal') out.push(`<span class="chip ${c.class === 'critical' ? 'accent' : c.class === 'batch' ? '' : ''}" title="Priority class">${esc(c.class)}</span>`);
  if (c.desktop) out.push(`<span class="chip good" title="Looks like a real PC">${ic('game')}desktop</span>`);
  if ((c.gpus || []).length) out.push(`<span class="chip good">${ic('gpu')}GPU</span>`);
  if (c.minimized) out.push(`<span class="chip warn" title="Minimal preset active">${ic('shrink')}minimized</span>`);
  if (c.health) out.push(`<span class="chip ${c.health.status === 'healthy' ? 'good' : c.health.status === 'unhealthy' ? 'crit' : ''}">${esc(c.health.status)}</span>`);
  return out.join('');
}
function addr(c) {
  if (c.network_mode === 'host') return 'host network';
  const ports = (c.ports || []).map((p) => `${p.host_port}→${p.container_port}${p.proto === 'Udp' ? '/udp' : ''}`).join(' ');
  return [c.ip, ports].filter(Boolean).join(' · ') || (c.network_mode === 'none' ? 'no network' : '–');
}
function rowCells(c) {
  const s = stats(c.id), st = stateOf(c);
  const limit = s && s.mem_max ? s.mem_max : null;
  const memPct = s && limit ? (s.mem / limit) * 100 : null;
  const running = c.state === 'running';
  return [
    `<span class="dot dot-${st.cls}" title="${esc(st.text)}"></span>`,
    `<b>${esc(c.name)}</b><small>${c.project ? `${esc(c.project)} · ` : ''}${esc(st.text)}</small>${chipsFor(c) ? `<div class="chips">${chipsFor(c)}</div>` : ''}`,
    esc(c.image),
    running && s ? `<div class="cell-bar">${pct(s.cpu, 1)}<div class="bar"><i style="width:${clamp(s.cpu / ((S.info?.cores || 1)), 0, 100)}%"></i></div></div>` : '–',
    running && s ? `<div class="cell-bar">${bytes(s.mem)}${limit ? ` / ${bytes(limit)}` : ''}<div class="bar ${memPct > 90 ? 'crit' : memPct > 75 ? 'warn' : ''}"><i style="width:${memPct != null ? clamp(memPct, 0, 100) : clamp((s.mem / (S.info?.mem_total || 1)) * 100, 0, 100)}%"></i></div></div>` : '–',
    running && s ? `${rate(s.rx)}<br>${rate(s.tx)}` : '–',
    esc(addr(c)),
    running && c.started_at ? dur(now() - c.started_at) : c.finished_at ? ago(c.finished_at) : '–',
    `<div class="row-actions">${running
      ? `<button class="row-btn stop" data-act="stop" data-id="${c.id}" title="Stop">${ic('stop')}</button>`
      : `<button class="row-btn go" data-act="start" data-id="${c.id}" title="${c.desired === 'sleeping' ? 'Wake' : 'Start'}">${ic('play')}</button>`}
      <button class="row-btn" data-act="menu" data-id="${c.id}" title="More">${ic('more')}</button></div>`,
  ];
}
const CELL_CLASS = ['', 'name', 'img hide-xs', 'num', 'num', 'num hide-sm', 'addr hide-sm', 'hide-sm', 'hide-xs'];
function paintContainers() {
  const tb = $('#ctTable tbody');
  const list = filtered();
  $('#ctEmpty').hidden = S.containers.length > 0;
  const rows = [];
  let lastGroup = null;
  for (const c of list) {
    if (S.ct.group && (c.project || '') !== lastGroup) {
      lastGroup = c.project || '';
      rows.push(`<tr class="group"><td colspan="9">${esc(lastGroup || 'standalone')}</td></tr>`);
    }
    const cells = rowCells(c);
    rows.push(`<tr class="click" data-row="${c.id}">${cells.map((h, i) => `<td class="${CELL_CLASS[i]}">${h}</td>`).join('')}</tr>`);
  }
  tb.innerHTML = rows.join('');
  for (const th of $$('#ctTable th[data-sort]')) th.classList.toggle('sorted', th.dataset.sort === S.ct.sort);
}
// stats tick: patch only the live cells of visible rows
function paintContainerStats() {
  if (S.ct.sort === 'cpu' || S.ct.sort === 'mem') return paintContainers();
  for (const tr of $$('#ctTable tbody tr[data-row]')) {
    const c = S.byId.get(tr.dataset.row); if (!c) continue;
    const cells = rowCells(c);
    [3, 4, 5, 7].forEach((i) => { if (tr.children[i].innerHTML !== cells[i]) tr.children[i].innerHTML = cells[i]; });
  }
}
$('#ctQ').addEventListener('input', (e) => { S.ct.q = e.target.value; paintContainers(); });
$('#ctFilter').addEventListener('click', (e) => { const b = e.target.closest('button'); if (!b) return; S.ct.filter = b.dataset.f; segOn('#ctFilter', b); paintContainers(); });
$('#ctGroup').addEventListener('change', (e) => { S.ct.group = e.target.checked; paintContainers(); });
$('#ctTable thead').addEventListener('click', (e) => {
  const th = e.target.closest('th[data-sort]'); if (!th) return;
  if (S.ct.sort === th.dataset.sort) S.ct.dir *= -1; else { S.ct.sort = th.dataset.sort; S.ct.dir = ['cpu', 'mem'].includes(th.dataset.sort) ? -1 : 1; }
  paintContainers();
});
$('#ctTable tbody').addEventListener('click', (e) => {
  const act = e.target.closest('[data-act]');
  if (act) { e.stopPropagation(); return containerAction(act.dataset.act, act.dataset.id, act); }
  const tr = e.target.closest('tr[data-row]');
  if (tr) openDrawer(tr.dataset.row);
});

async function containerAction(act, id, el) {
  const c = S.byId.get(id); if (!c) return;
  if (act === 'menu') return containerMenu(c, el);
  const labels = { start: 'Starting', stop: 'Stopping', restart: 'Restarting', kill: 'Killing' };
  if (act === 'remove') {
    if (!(await confirmBox(`Remove ${c.name}?`, `The container and its writable layer are deleted. Named volumes are kept.`, 'Remove'))) return;
    if (c.state === 'running') await api(`/v1/containers/${id}/stop`, { method: 'POST' }).catch(() => {});
    await api(`/v1/containers/${id}`, { method: 'DELETE' }).then(() => toast(`Removed ${c.name}`)).catch((er) => toast(er.message, true));
    return;
  }
  if (act === 'minimal' || act === 'restore') {
    return api(`/v1/containers/${id}/resources`, { method: 'POST', body: { preset: act } })
      .then(() => toast(act === 'minimal' ? `${c.name} minimized: a quarter core, memory held` : `${c.name} restored`))
      .catch((er) => toast(er.message, true));
  }
  if (act === 'logs') { location.hash = H.containerLogsHash(c); return; }
  el && el.classList.add('busy');
  toast(`${labels[act] || act} ${c.name}…`);
  try {
    await api(`/v1/containers/${id}/${act}`, { method: 'POST' });
    toast(`${c.name}: ${act === 'start' ? 'started' : act === 'stop' ? 'stopped' : act === 'kill' ? 'killed' : 'restarted'}`);
  } catch (er) { toast(`${c.name}: ${er.message}`, true); } finally { el && el.classList.remove('busy'); }
}

function containerMenu(c, el) {
  const running = c.state === 'running';
  const items = [
    ['info', 'Details', () => openDrawer(c.id)],
    ['logs', 'Logs', () => containerAction('logs', c.id)],
    ['sliders', 'Resources…', () => openDrawer(c.id, 'res')],
    null,
    running ? ['restart', 'Restart', () => containerAction('restart', c.id)] : ['play', 'Start', () => containerAction('start', c.id)],
    running && (c.minimized ? ['expand', 'Restore resources', () => containerAction('restore', c.id)] : ['shrink', 'Minimize', () => containerAction('minimal', c.id)]),
    running && ['zap', 'Kill', () => containerAction('kill', c.id)],
    null,
    ['trash', 'Remove…', () => containerAction('remove', c.id), 'danger'],
  ].filter((x) => x !== false);
  showMenu(el, items);
}

// ---------------------------------------------------------------- menu, modal, confirm
function showMenu(anchor, items) {
  const m = $('#menu');
  m.innerHTML = items.map((it, i) => (it ? `<button data-i="${i}" class="${it[3] || ''}">${ic(it[0] === 'info' ? 'file' : it[0])}${esc(it[1])}</button>` : '<hr>')).join('');
  m.hidden = false;
  const r = anchor.getBoundingClientRect();
  const mw = m.offsetWidth, mh = m.offsetHeight;
  m.style.left = `${clamp(r.right - mw, 8, innerWidth - mw - 8)}px`;
  m.style.top = `${r.bottom + mh + 8 > innerHeight ? Math.max(8, r.top - mh - 4) : r.bottom + 4}px`;
  m.onclick = (e) => { const b = e.target.closest('button'); if (!b) return; m.hidden = true; items[b.dataset.i][2](); };
}
document.addEventListener('mousedown', (e) => { if (!e.target.closest('#menu')) $('#menu').hidden = true; });

function modal(html, { wide = false } = {}) {
  const bd = $('#modal'), box = $('#modalBox');
  box.className = `modal ${wide ? 'wide' : ''}`;
  box.innerHTML = html;
  hydrateIcons(box);
  bd.hidden = false;
  const close = () => { bd.hidden = true; box.innerHTML = ''; };
  bd.onmousedown = (e) => { if (e.target === bd) close(); };
  return { box, close };
}
function confirmBox(title, text, ok = 'OK') {
  return new Promise((res) => {
    const { box, close } = modal(`<h3>${ic('alert')}${esc(title)}</h3><p class="muted">${esc(text)}</p><div class="modal-actions"><button class="ghost-btn" data-c="0">Cancel</button><button class="danger-btn" data-c="1">${esc(ok)}</button></div>`);
    box.onclick = (e) => { const b = e.target.closest('[data-c]'); if (!b) return; close(); res(b.dataset.c === '1'); };
  });
}

// ---------------------------------------------------------------- new container
$('#ctNew').addEventListener('click', () => {
  const { box, close } = modal(`
    <h3>${ic('box')}New container</h3>
    <form class="form" id="ncForm">
      <div class="row2"><label>Image<input name="image" placeholder="nginx:1.29-alpine" required></label><label>Name<input name="name" placeholder="web"></label></div>
      <label><span>Ports <span class="hint">host:container, one per line or comma-separated</span></span><input name="ports" placeholder="8080:80"></label>
      <label><span>Environment <span class="hint">KEY=value per line</span></span><textarea name="env" rows="2"></textarea></label>
      <label><span>Volumes <span class="hint">/host/path:/path[:ro] or volume-name:/path</span></span><textarea name="vols" rows="2"></textarea></label>
      <div class="row2">
        <label>Restart<select name="restart"><option value="unless-stopped">unless stopped</option><option value="always">always</option><option value="on-failure">on failure</option><option value="no">never</option></select></label>
        <label>Priority<select name="prio"><option value="">normal</option><option value="critical">critical — protected</option><option value="batch">batch — squeezed first</option><option value="idle-stop">idle-stop — sleeps when idle</option></select></label>
      </div>
      <div class="row2"><label>Memory limit<input name="mem" placeholder="e.g. 512M"></label><label><span>CPU limit <span class="hint">cores</span></span><input name="cpus" placeholder="e.g. 1.5"></label></div>
      <div class="chips"><label class="chk"><input type="checkbox" name="gpu"> NVIDIA GPU</label><label class="chk"><input type="checkbox" name="desktop"> Looks like a real PC (games)</label></div>
      <p class="err" id="ncErr"></p>
      <div class="modal-actions"><span class="note">The image is pulled if needed.</span><button type="button" class="ghost-btn" data-close>Cancel</button><button class="primary-btn">Create &amp; start</button></div>
    </form>`);
  box.querySelector('[data-close]').onclick = close;
  box.querySelector('input[name=image]').focus();
  box.querySelector('#ncForm').onsubmit = async (e) => {
    e.preventDefault();
    const f = Object.fromEntries(new FormData(e.target));
    const btn = e.target.querySelector('.primary-btn'); btn.disabled = true;
    const err = box.querySelector('#ncErr'); err.textContent = '';
    try {
      const list = (s) => s.split(/[\n,]/).map((x) => x.trim()).filter(Boolean);
      const bindings = {};
      for (const p of list(f.ports)) {
        const m = p.match(/^(?:(\d+\.\d+\.\d+\.\d+):)?(\d+):(\d+)(?:\/(tcp|udp))?$/);
        if (!m) throw new Error(`bad port ${p}`);
        (bindings[`${m[3]}/${m[4] || 'tcp'}`] ||= []).push({ HostIp: m[1] || '', HostPort: m[2] });
      }
      const mem = parseSize(f.mem);
      if (Number.isNaN(mem)) throw new Error('memory limit looks like 512M or 2G');
      const cpus = f.cpus ? Number(f.cpus) : 0;
      if (Number.isNaN(cpus) || cpus < 0) throw new Error('CPU limit is a number of cores');
      const labels = {};
      if (f.prio) labels['og.priority'] = f.prio;
      if (f.desktop) { labels['og.profile'] = 'desktop'; labels['og.gpu-class'] = 'gaming'; }
      toast(`Pulling ${f.image}…`);
      const pull = await api(`/v1/docker/images/create?fromImage=${encodeURIComponent(f.image.trim())}`, { method: 'POST', raw: true });
      const perr = String(pull).split('\n').filter(Boolean).map((l) => { try { return JSON.parse(l); } catch { return {}; } }).find((l) => l.error);
      if (perr) throw new Error(perr.error);
      const body = {
        Image: f.image.trim(), Env: list(f.env).length ? f.env.split('\n').map((x) => x.trim()).filter(Boolean) : [], Labels: labels,
        ExposedPorts: Object.fromEntries(Object.keys(bindings).map((k) => [k, {}])),
        HostConfig: {
          PortBindings: bindings, Binds: f.vols.split('\n').map((x) => x.trim()).filter(Boolean),
          RestartPolicy: { Name: f.restart }, Memory: mem || 0, NanoCpus: Math.round(cpus * 1e9),
          DeviceRequests: f.gpu ? [{ Driver: 'nvidia', Count: -1, Capabilities: [['gpu']] }] : [],
        },
      };
      const r = await api(`/v1/docker/containers/create${f.name ? `?name=${encodeURIComponent(f.name.trim())}` : ''}`, { method: 'POST', body });
      await api(`/v1/docker/containers/${r.Id}/start`, { method: 'POST' });
      close();
      toast(`Started ${f.name || f.image}`);
      location.hash = '#containers';
    } catch (er) { err.textContent = er.message; } finally { btn.disabled = false; }
  };
});

// ---------------------------------------------------------------- drawer
function openDrawer(id, tab = 'info') {
  const c = S.byId.get(id); if (!c) return;
  S.drawer = id; S.drawerTab = tab;
  $('#drawer').classList.add('open');
  $('#drawer').setAttribute('aria-hidden', 'false');
  segOn('#drTabs', $(`#drTabs [data-dt=${tab}]`));
  paintDrawerHead(c);
  paintDrawerBody();
}
function closeDrawer() {
  S.drawer = null;
  if (S.drawerLog) { S.drawerLog.close(); S.drawerLog = null; }
  $('#drawer').classList.remove('open');
  $('#drawer').setAttribute('aria-hidden', 'true');
}
function paintDrawerHead(c) {
  const st = stateOf(c);
  $('#drDot').className = `dot dot-${st.cls}`;
  $('#drName').textContent = c.name;
  $('#drSub').textContent = `${c.image} · ${st.text}`;
  const running = c.state === 'running';
  $('#drActions').innerHTML = `${running
    ? `<button class="icon-btn" data-act="restart" data-id="${c.id}" title="Restart">${ic('restart')}</button><button class="icon-btn" data-act="stop" data-id="${c.id}" title="Stop">${ic('stop')}</button>`
    : `<button class="icon-btn" data-act="start" data-id="${c.id}" title="Start">${ic('play')}</button>`}
    <button class="icon-btn" data-act="menu" data-id="${c.id}" title="More">${ic('more')}</button>
    <button class="icon-btn" id="drClose" title="Close">${ic('x')}</button>`;
}
$('#drActions').addEventListener('click', (e) => {
  if (e.target.closest('#drClose')) return closeDrawer();
  const a = e.target.closest('[data-act]'); if (a) containerAction(a.dataset.act, a.dataset.id, a);
});
$('#drTabs').addEventListener('click', (e) => {
  const b = e.target.closest('button'); if (!b || !S.drawer) return;
  S.drawerTab = b.dataset.dt; segOn('#drTabs', b); paintDrawerBody();
});
document.addEventListener('keydown', (e) => { if (e.key === 'Escape') { if (!$('#modal').hidden) $('#modal').hidden = true; else if (!$('#palette').hidden) closePalette(); else if (S.drawer) closeDrawer(); } });

function paintDrawerBody() {
  if (S.drawerLog) { S.drawerLog.close(); S.drawerLog = null; }
  const c = S.byId.get(S.drawer); if (!c) return;
  const body = $('#drBody');
  if (S.drawerTab === 'info') {
    const kv = (k, v) => `<div class="kv"><span>${k}</span><b>${v}</b></div>`;
    const labels = Object.entries(c.labels || {}).filter(([k]) => !k.startsWith('com.docker.compose.project.'));
    body.innerHTML = `
      <div class="card"><h3>${ic('activity')}Live</h3><div id="drLive"></div></div>
      <div class="card"><h3>${ic('box')}Container</h3>
        ${kv('ID', `<code>${c.id.slice(0, 12)}</code>`)}${kv('Image', esc(c.image))}${kv('State', esc(stateOf(c).text))}
        ${kv('Restart policy', esc(c.restart || 'no'))}${kv('Network', esc(c.network_mode || '–'))}${kv('Address', esc(addr(c)))}
        ${c.project ? kv('Stack', `${esc(c.project)} / ${esc(c.service)}`) : ''}
        ${kv('Created', ago(c.created_at))}${c.started_at ? kv(c.state === 'running' ? 'Up' : 'Last started', c.state === 'running' ? dur(now() - c.started_at) : ago(c.started_at)) : ''}
        ${c.error ? `<p class="err">${esc(c.error)}</p>` : ''}${c.oom_killed ? '<p class="err">Killed by the kernel for running out of memory.</p>' : ''}</div>
      <div class="card"><h3>${ic('shield')}Policy</h3>
        ${kv('Priority', esc(c.class || 'normal'))}${kv('Profile', c.desktop ? 'desktop (looks like a real PC)' : 'standard')}${kv('GPUs', (c.gpus || []).length ? esc(c.gpus.join(', ')) : 'none')}
        ${c.health ? kv('Health', `${esc(c.health.status)}${c.health.failing_streak ? ` · ${c.health.failing_streak} failing` : ''}`) : ''}
        ${c.health && c.health.last_output ? `<pre class="json">${esc(c.health.last_output.slice(-600))}</pre>` : ''}</div>
      ${labels.length ? `<div class="card"><h3>${ic('file')}Labels</h3>${labels.map(([k, v]) => kv(esc(k), esc(v))).join('')}</div>` : ''}`;
    paintDrawerLive();
  } else if (S.drawerTab === 'logs') {
    body.innerHTML = `<div class="log-lines wrap" id="drLog"><div class="log-empty">Loading…</div></div>`;
    const el = $('#drLog'); let first = true; let lines = [];
    S.drawerLog = new EventSource(`/v1/containers/${c.id}/logs/stream?tail=400`);
    S.drawerLog.addEventListener('log', (ev) => {
      const chunk = JSON.parse(ev.data);
      lines = lines.concat(chunk.split('\n').filter((l, i, a) => l || i < a.length - 1));
      if (lines.length > 4000) lines = lines.slice(-4000);
      const stick = first || el.scrollTop + el.clientHeight >= el.scrollHeight - 30;
      el.innerHTML = lines.length ? lines.map((l) => `<div class="ll ${lvl(l)}">${esc(l)}</div>`).join('') : '<div class="log-empty">No output yet.</div>';
      if (stick) el.scrollTop = el.scrollHeight;
      first = false;
    });
  } else if (S.drawerTab === 'res') {
    body.innerHTML = '<p class="muted">Loading…</p>';
    api(`/v1/containers/${c.id}/resources`).then((r) => paintResources(c, r)).catch((er) => (body.innerHTML = `<p class="err">${esc(er.message)}</p>`));
  } else {
    body.innerHTML = '<p class="muted">Loading…</p>';
    api(`/v1/docker/containers/${c.id}/json`).then((j) => { body.innerHTML = `<pre class="json">${esc(JSON.stringify(j, null, 2))}</pre>`; }).catch((er) => (body.innerHTML = `<p class="err">${esc(er.message)}</p>`));
  }
}

function paintDrawerLive() {
  const el = $('#drLive'); if (!el || !S.drawer) return;
  const c = S.byId.get(S.drawer); const s = stats(S.drawer);
  if (!c || c.state !== 'running' || !s) { el.innerHTML = '<p class="muted">Not running.</p>'; return; }
  const h = S.chist[S.drawer] || [];
  el.innerHTML = `
    <div class="row2" style="display:grid;grid-template-columns:1fr 1fr;gap:14px">
      <div><div class="kv"><span>CPU</span><b>${pct(s.cpu, 1)}</b></div>${spark(h.map((p) => p[1]), { max: Math.max(10, ...h.map((p) => p[1])) })}</div>
      <div><div class="kv"><span>Memory</span><b>${bytes(s.mem)}${s.mem_max ? ` / ${bytes(s.mem_max)}` : ''}</b></div>${spark(h.map((p) => p[2]), { max: s.mem_max || undefined, cls: 'good' })}</div>
    </div>
    <div class="kv"><span>Network in · out</span><b>${rate(s.rx)} · ${rate(s.tx)}</b></div>
    <div class="kv"><span>Disk read · write</span><b>${rate(s.io_r)} · ${rate(s.io_w)}</b></div>
    <div class="kv"><span>Processes</span><b>${s.pids}</b></div>`;
}

function paintResources(c, r) {
  const body = $('#drBody');
  const cores = S.info?.cores || 4;
  const memTotal = S.info?.mem_total || 8 * 1024 ** 3;
  const mb = (v) => (v ? bytes(v) : '');
  body.innerHTML = `
    <div class="card"><h3>${ic('sliders')}Presets</h3>
      <p class="muted small">Minimal keeps it running on a quarter core with the lowest CPU and disk priority, and stops its memory from growing. Restore puts back exactly what it had.</p>
      <div class="presets"><button class="ghost-btn" data-preset="minimal" ${c.minimized ? 'disabled' : ''}>${ic('shrink')}Minimize</button><button class="ghost-btn" data-preset="restore" ${c.minimized ? '' : 'disabled'}>${ic('expand')}Restore</button></div></div>
    <form class="card form" id="resForm"><h3>${ic('cpu')}Limits <span class="muted small">applied live, kept across restarts</span></h3>
      <label><span>CPU limit <span class="hint">cores, 0 = no limit</span></span><div class="slider"><input type="range" min="0" max="${cores}" step="0.05" value="${r.cpus || 0}" data-mirror="cpus"><input type="text" name="cpus" value="${r.cpus || 0}"></div></label>
      <label><span>CPU weight <span class="hint">share under contention, default 100</span></span><div class="slider"><input type="range" min="1" max="1000" step="1" value="${Math.min(r.cpu_weight || 100, 1000)}" data-mirror="cpu_weight"><input type="text" name="cpu_weight" value="${r.cpu_weight || 100}"></div></label>
      <label><span>Pin to CPUs <span class="hint">e.g. 0-3 or 0,2; empty = all ${cores}</span></span><input name="cpuset_cpus" value="${esc(r.cpuset_cpus || '')}"></label>
      <div class="row2">
        <label><span>Memory limit <span class="hint">hard cap, empty = none</span></span><input name="memory_max" value="${mb(r.memory_max)}" placeholder="none"></label>
        <label><span>Memory high <span class="hint">throttle above</span></span><input name="memory_high" value="${mb(r.memory_high)}" placeholder="none"></label>
      </div>
      <div class="row2">
        <label><span>Memory reserved <span class="hint">protected from reclaim</span></span><input name="memory_low" value="${mb(r.memory_low)}" placeholder="none"></label>
        <label><span>Max processes <span class="hint">0 = no limit</span></span><input name="pids_max" value="${r.pids_max || 0}"></label>
      </div>
      <label><span>Disk priority <span class="hint">io weight, default 100</span></span><div class="slider"><input type="range" min="1" max="1000" step="1" value="${Math.min(r.io_weight || 100, 1000)}" data-mirror="io_weight"><input type="text" name="io_weight" value="${r.io_weight || 100}"></div></label>
      ${(r.io || []).length ? `<p class="muted small">Disk caps: ${r.io.map((l) => `${esc(l.device)} ${l.read_bps ? `read ${rate(l.read_bps)}` : ''} ${l.write_bps ? `write ${rate(l.write_bps)}` : ''}`).join('; ')}</p>` : ''}
      <p class="muted small">Host: ${cores} cores, ${bytes(memTotal)}.</p>
      <p class="err" id="resErr"></p>
      <div class="modal-actions"><button class="primary-btn">${ic('check')}Apply</button></div>
    </form>`;
  body.querySelector('.presets').onclick = (e) => { const b = e.target.closest('[data-preset]'); if (!b) return; api(`/v1/containers/${c.id}/resources`, { method: 'POST', body: { preset: b.dataset.preset } }).then((nr) => { toast(b.dataset.preset === 'minimal' ? 'Minimized' : 'Restored'); paintResources({ ...c, minimized: b.dataset.preset === 'minimal' }, nr); }).catch((er) => toast(er.message, true)); };
  for (const rg of $$('[data-mirror]', body)) {
    const t = body.querySelector(`input[name=${rg.dataset.mirror}]`);
    rg.oninput = () => (t.value = rg.value);
    t.oninput = () => (rg.value = t.value);
  }
  body.querySelector('#resForm').onsubmit = async (e) => {
    e.preventDefault();
    const f = Object.fromEntries(new FormData(e.target));
    const err = body.querySelector('#resErr'); err.textContent = '';
    const size = (v, name) => { const n = v.trim() === '' || v.trim() === 'none' ? 0 : parseSize(v); if (Number.isNaN(n)) throw new Error(`${name} looks like 512M or 2G`); return n; };
    try {
      const change = {
        cpus: Number(f.cpus) || 0, cpu_weight: clamp(Math.round(Number(f.cpu_weight) || 100), 1, 10000), cpuset_cpus: f.cpuset_cpus.trim(),
        memory_max: size(f.memory_max, 'Memory limit'), memory_high: size(f.memory_high, 'Memory high'), memory_low: size(f.memory_low, 'Memory reserved'),
        pids_max: Math.max(0, Math.round(Number(f.pids_max) || 0)), io_weight: clamp(Math.round(Number(f.io_weight) || 100), 1, 10000),
      };
      const nr = await api(`/v1/containers/${c.id}/resources`, { method: 'POST', body: change });
      toast(`Limits applied to ${c.name}`);
      paintResources(c, nr);
    } catch (er) { err.textContent = er.message; }
  };
}

// ---------------------------------------------------------------- stacks
let stackCache = [];
async function loadStacks() {
  try { stackCache = await api('/v1/stacks'); } catch (e) { toast(e.message, true); return; }
  paintStacks();
}
function paintStacks() {
  const el = $('#stacks');
  if (!stackCache.length) { el.innerHTML = '<p class="empty">No stacks yet. Create one from a compose file.</p>'; return; }
  el.innerHTML = stackCache.map((s) => {
    const total = s.services.length;
    const level = s.running === total && total ? 'ok' : s.running ? 'warn' : '';
    return `<div class="stack ${s.running ? '' : 'stopped'}" data-stack="${esc(s.name)}">
      <div class="stack-top"><span class="dot dot-${level}"></span><b>${esc(s.name)}</b><span class="chip">${s.running}/${total} running</span>${s.managed ? '<span class="chip accent">managed</span>' : ''}<span class="sp"></span></div>
      ${s.file ? `<div class="stack-path" title="${esc(s.file)}">${esc(s.file)}</div>` : '<div class="stack-path">compose file not known</div>'}
      <div class="svc">${s.services.map((v) => `<span ${v.container ? `data-open="${v.container}"` : ''} title="${esc(v.image || '')}"><i class="dot dot-${v.state === 'running' ? 'ok' : v.state === 'failed' ? 'crit' : ''}"></i>${esc(v.name)}</span>`).join('')}</div>
      <div class="stack-actions">
        <button data-sa="up" ${s.file ? '' : 'disabled'}>${ic('play')}Up</button>
        <button data-sa="down">${ic('stop')}Down</button>
        <button data-sa="restart" ${s.file ? '' : 'disabled'}>${ic('restart')}Restart</button>
        <button data-sa="pull" ${s.file ? '' : 'disabled'}>${ic('download')}Pull &amp; up</button>
        <button data-sa="edit" ${s.file ? '' : 'disabled'}>${ic('edit')}Edit</button>
        ${s.managed ? `<button data-sa="delete">${ic('trash')}Delete</button>` : ''}
      </div>
      <pre class="stack-out" hidden></pre></div>`;
  }).join('');
}
$('#stacks').addEventListener('click', async (e) => {
  const b = e.target.closest('[data-sa]'); if (!b) return;
  const card = b.closest('[data-stack]'); const name = card.dataset.stack;
  const out = card.querySelector('.stack-out');
  const act = b.dataset.sa;
  if (act === 'edit') return openEditor(name);
  if (act === 'delete') {
    if (!(await confirmBox(`Delete stack ${name}?`, 'Its containers are removed and its compose file deleted. Named volumes are kept.', 'Delete'))) return;
    return api(`/v1/stacks/${encodeURIComponent(name)}`, { method: 'DELETE' }).then(() => { toast(`Deleted ${name}`); loadStacks(); }).catch((er) => toast(er.message, true));
  }
  if (act === 'down' && !(await confirmBox(`Stop ${name}?`, 'All of its containers are stopped and removed (volumes are kept).', 'Down'))) return;
  for (const x of $$('button', card)) x.disabled = true;
  b.classList.add('busy');
  out.hidden = false; out.className = 'stack-out'; out.textContent = `${act}…`;
  try {
    const r = await api(`/v1/stacks/${encodeURIComponent(name)}/${act}`, { method: 'POST' });
    out.textContent = `${act} done in ${r.seconds.toFixed(1)}s${r.warnings?.length ? `\n${r.warnings.map((w) => `warning: ${w}`).join('\n')}` : ''}`;
  } catch (er) { out.className = 'stack-out fail'; out.textContent = er.message; }
  loadStacks();
});

function lineDiff(a, b) {
  // LCS on lines; shows changes with 2 lines of context
  const A = a.split('\n'), B = b.split('\n');
  const n = A.length, m = B.length;
  if (n * m > 4e6) return [{ t: 'ctx', s: '(file too large to diff)' }];
  const dp = Array.from({ length: n + 1 }, () => new Uint32Array(m + 1));
  for (let i = n - 1; i >= 0; i--) for (let j = m - 1; j >= 0; j--) dp[i][j] = A[i] === B[j] ? dp[i + 1][j + 1] + 1 : Math.max(dp[i + 1][j], dp[i][j + 1]);
  const ops = []; let i = 0, j = 0;
  while (i < n || j < m) {
    if (i < n && j < m && A[i] === B[j]) { ops.push({ t: 'ctx', s: A[i] }); i++; j++; }
    else if (j < m && (i >= n || dp[i][j + 1] >= dp[i + 1][j])) { ops.push({ t: 'add', s: B[j++] }); }
    else ops.push({ t: 'del', s: A[i++] });
  }
  const keep = ops.map((o, k) => o.t !== 'ctx' || ops.slice(Math.max(0, k - 2), k + 3).some((x) => x.t !== 'ctx'));
  const res = []; let gap = false;
  ops.forEach((o, k) => { if (keep[k]) { res.push(o); gap = false; } else if (!gap) { res.push({ t: 'ctx', s: '⋯' }); gap = true; } });
  return res;
}

async function openEditor(name) {
  const isNew = !name;
  let orig = '', file = '';
  if (!isNew) {
    try { const r = await api(`/v1/stacks/${encodeURIComponent(name)}`); orig = r.content; file = r.file; } catch (er) { return toast(er.message, true); }
  } else {
    orig = 'services:\n  web:\n    image: nginx:1.29-alpine\n    ports:\n      - "8080:80"\n    restart: unless-stopped\n';
  }
  const { box, close } = modal(`
    <h3>${ic('file')}${isNew ? 'New stack' : esc(name)}</h3>
    ${isNew ? '<label class="form"><span class="muted small">Name (lowercase letters, digits, - and _)</span><input id="edName" placeholder="my-stack"></label>' : `<small class="muted">${esc(file)}</small>`}
    <div class="seg seg-sm" id="edTabs" style="justify-self:start"><button data-ed="edit" class="on">Edit</button><button data-ed="diff">Changes</button></div>
    <textarea class="code" id="edText" spellcheck="false" wrap="off"></textarea>
    <div class="diff" id="edDiff" hidden></div>
    <p class="err" id="edErr"></p>
    <div class="modal-actions"><span class="note">A backup is kept; a file that doesn't parse is rolled back.</span>
      <button class="ghost-btn" data-ed-close>Cancel</button><button class="ghost-btn" data-ed-save="0">Save</button><button class="primary-btn" data-ed-save="1">Save &amp; up</button></div>`, { wide: true });
  const ta = box.querySelector('#edText'); ta.value = orig;
  ta.addEventListener('keydown', (e) => { if (e.key === 'Tab') { e.preventDefault(); const s = ta.selectionStart; ta.setRangeText('  ', s, ta.selectionEnd, 'end'); } });
  box.querySelector('[data-ed-close]').onclick = close;
  box.querySelector('#edTabs').onclick = (e) => {
    const b = e.target.closest('button'); if (!b) return;
    for (const x of $$('#edTabs button', box)) x.classList.toggle('on', x === b);
    const diff = b.dataset.ed === 'diff';
    ta.hidden = diff; box.querySelector('#edDiff').hidden = !diff;
    if (diff) box.querySelector('#edDiff').innerHTML = lineDiff(orig, ta.value).map((o) => `<div class="${o.t}">${o.t === 'add' ? '+ ' : o.t === 'del' ? '- ' : '  '}${esc(o.s)}</div>`).join('') || '<div class="ctx">no changes</div>';
  };
  for (const sb of $$('[data-ed-save]', box)) sb.onclick = async () => {
    const nm = isNew ? box.querySelector('#edName').value.trim() : name;
    const err = box.querySelector('#edErr'); err.textContent = '';
    if (!/^[a-z0-9][a-z0-9_-]{0,62}$/.test(nm)) { err.textContent = 'Name: lowercase letters, digits, - and _'; return; }
    for (const x of $$('button', box)) x.disabled = true;
    try {
      await api(`/v1/stacks/${encodeURIComponent(nm)}`, { method: 'PUT', body: { content: ta.value } });
      if (sb.dataset.edSave === '1') { toast(`Starting ${nm}…`); await api(`/v1/stacks/${encodeURIComponent(nm)}/up`, { method: 'POST' }); toast(`${nm} is up`); }
      else toast(`Saved ${nm}`);
      close(); loadStacks();
    } catch (er) { err.textContent = er.message; for (const x of $$('button', box)) x.disabled = false; }
  };
  (isNew ? box.querySelector('#edName') : ta).focus();
}
$('#stNew').addEventListener('click', () => openEditor(null));

// ---------------------------------------------------------------- images
async function loadImages() {
  let imgs;
  try { imgs = await api('/v1/docker/images/json'); } catch (e) { return toast(e.message, true); }
  const used = (tag) => S.containers.filter((c) => c.image === tag || `docker.io/library/${c.image}` === tag || c.image === tag.replace(/^docker\.io\/(library\/)?/, ''));
  const total = imgs.reduce((a, i) => a + (i.Size || 0), 0);
  $('#imgMeta').textContent = `${imgs.length} images · ${bytes(total)}`;
  const rows = imgs.flatMap((i) => (i.RepoTags.length ? i.RepoTags : ['<none>']).map((t) => ({ t, i })));
  rows.sort((a, b) => a.t.localeCompare(b.t));
  $('#imgTable tbody').innerHTML = rows.map(({ t, i }) => {
    const u = used(t);
    return `<tr><td><b>${esc(t)}</b></td><td class="addr">${esc(i.Id.replace('sha256:', '').slice(0, 12))}</td><td class="num">${bytes(i.Size)}</td>
      <td>${u.length ? u.map((c) => `<span class="chip" data-open="${c.id}">${esc(c.name)}</span>`).join(' ') : '<span class="muted">unused</span>'}</td>
      <td><div class="row-actions"><button class="row-btn" data-img-pull="${esc(t)}" title="Pull again">${ic('download')}</button><button class="row-btn stop" data-img-rm="${esc(t)}" title="Remove" ${u.length ? 'disabled' : ''}>${ic('trash')}</button></div></td></tr>`;
  }).join('') || '<tr><td colspan="5" class="muted">No images yet.</td></tr>';
}
async function pull(ref) {
  toast(`Pulling ${ref}…`);
  const out = await api(`/v1/docker/images/create?fromImage=${encodeURIComponent(ref)}`, { method: 'POST', raw: true });
  const err = String(out).split('\n').filter(Boolean).map((l) => { try { return JSON.parse(l); } catch { return {}; } }).find((l) => l.error);
  if (err) throw new Error(err.error);
  toast(`Pulled ${ref}`);
  loadImages();
}
$('#pullForm').addEventListener('submit', (e) => { e.preventDefault(); const r = $('#pullRef').value.trim(); if (r) pull(r).then(() => ($('#pullRef').value = '')).catch((er) => toast(er.message, true)); });
$('#imgTable').addEventListener('click', async (e) => {
  const p = e.target.closest('[data-img-pull]'); if (p) return pull(p.dataset.imgPull).catch((er) => toast(er.message, true));
  const r = e.target.closest('[data-img-rm]');
  if (r && (await confirmBox(`Remove ${r.dataset.imgRm}?`, 'The tag is removed; layers shared with other images stay.', 'Remove'))) {
    api(`/v1/docker/images/${encodeURIComponent(r.dataset.imgRm)}`, { method: 'DELETE' }).then(() => { toast('Removed'); loadImages(); }).catch((er) => toast(er.message, true));
  }
});

// ---------------------------------------------------------------- storage & networks
function paintDisks() {
  const fs = S.stats?.node?.fs || [];
  $('#fsList').innerHTML = fs.map((f) => { const p = (f.used / f.total) * 100; return `<div class="disk"><b>${esc(f.mount)}</b><span class="right">${bytes(f.used)} of ${bytes(f.total)} · ${bytes(f.total - f.used)} free</span><div class="bar ${p > 90 ? 'crit' : p > 80 ? 'warn' : ''}"><i style="width:${p.toFixed(1)}%"></i></div></div>`; }).join('') || '<p class="muted">No filesystems found.</p>';
}
async function loadStorage() {
  try {
    const [vols, nets, cjson] = await Promise.all([api('/v1/docker/volumes'), api('/v1/docker/networks'), api('/v1/docker/containers/json?all=1')]);
    const vUse = {}, nUse = {};
    for (const c of cjson) {
      for (const m of c.Mounts || []) if (m.Name) (vUse[m.Name] ||= []).push(c.Names[0].replace(/^\//, ''));
      for (const n of Object.keys(c.NetworkSettings?.Networks || {})) (nUse[n] ||= []).push(c.Names[0].replace(/^\//, ''));
    }
    $('#volTable tbody').innerHTML = (vols.Volumes || []).sort((a, b) => a.Name.localeCompare(b.Name)).map((v) => `<tr><td><b>${esc(v.Name)}</b><br><small class="muted">${esc(v.Mountpoint)}</small></td><td>${(vUse[v.Name] || []).map((n) => `<span class="chip">${esc(n)}</span>`).join(' ') || '<span class="muted">unused</span>'}</td><td><div class="row-actions"><button class="row-btn stop" data-vol-rm="${esc(v.Name)}" title="Delete" ${vUse[v.Name] ? 'disabled' : ''}>${ic('trash')}</button></div></td></tr>`).join('') || '<tr><td colspan="3" class="muted">No volumes.</td></tr>';
    $('#netTable tbody').innerHTML = nets.sort((a, b) => a.Name.localeCompare(b.Name)).map((n) => `<tr><td><b>${esc(n.Name)}</b><br><small class="muted">${esc(n.Driver)}${n.Options?.['og-host.bridge'] ? ` · ${esc(n.Options['og-host.bridge'])}` : ''}</small></td><td class="addr">${esc(n.IPAM?.Config?.[0]?.Subnet || '–')}</td><td>${(nUse[n.Name] || []).length}</td><td><div class="row-actions">${['bridge', 'host', 'none'].includes(n.Name) ? '' : `<button class="row-btn stop" data-net-rm="${esc(n.Name)}" title="Delete" ${(nUse[n.Name] || []).length ? 'disabled' : ''}>${ic('trash')}</button>`}</div></td></tr>`).join('');
  } catch (e) { toast(e.message, true); }
}
$('#volTable').addEventListener('click', async (e) => {
  const b = e.target.closest('[data-vol-rm]');
  if (b && (await confirmBox(`Delete volume ${b.dataset.volRm}?`, 'Everything stored in it is deleted for good.', 'Delete'))) api(`/v1/docker/volumes/${encodeURIComponent(b.dataset.volRm)}`, { method: 'DELETE' }).then(() => { toast('Deleted'); loadStorage(); }).catch((er) => toast(er.message, true));
});
$('#netTable').addEventListener('click', async (e) => {
  const b = e.target.closest('[data-net-rm]');
  if (b && (await confirmBox(`Delete network ${b.dataset.netRm}?`, 'Its bridge and firewall rules are removed.', 'Delete'))) api(`/v1/docker/networks/${encodeURIComponent(b.dataset.netRm)}`, { method: 'DELETE' }).then(() => { toast('Deleted'); loadStorage(); }).catch((er) => toast(er.message, true));
});
function nameModal(title, label, placeholder, extra = '') {
  return new Promise((res) => {
    const { box, close } = modal(`<h3>${ic('plus')}${esc(title)}</h3><form class="form"><label>${esc(label)}<input name="n" placeholder="${esc(placeholder)}" required pattern="[a-zA-Z0-9][a-zA-Z0-9_.-]*"></label>${extra}<div class="modal-actions"><button type="button" class="ghost-btn" data-x>Cancel</button><button class="primary-btn">Create</button></div></form>`);
    box.querySelector('[data-x]').onclick = () => { close(); res(null); };
    box.querySelector('form').onsubmit = (e) => { e.preventDefault(); const f = Object.fromEntries(new FormData(e.target)); close(); res(f); };
    box.querySelector('input').focus();
  });
}
$('#volNew').addEventListener('click', async () => { const f = await nameModal('New volume', 'Name', 'app-data'); if (f) api('/v1/docker/volumes/create', { method: 'POST', body: { Name: f.n } }).then(() => { toast('Created'); loadStorage(); }).catch((er) => toast(er.message, true)); });
$('#netNew').addEventListener('click', async () => {
  const f = await nameModal('New network', 'Name', 'backend', '<label><span>Subnet <span class="hint">optional, e.g. 10.89.20.0/24</span></span><input name="subnet" placeholder="automatic"></label>');
  if (f) api('/v1/docker/networks/create', { method: 'POST', body: { Name: f.n, ...(f.subnet ? { IPAM: { Config: [{ Subnet: f.subnet }] } } : {}) } }).then(() => { toast('Created'); loadStorage(); }).catch((er) => toast(er.message, true));
});

// ---------------------------------------------------------------- logs
const lvl = (l) => (/\b(error|fatal|panic|exception|failed|crit(ical)?)\b/i.test(l) ? 'error' : /\b(warn(ing)?)\b/i.test(l) ? 'warn' : '');
function paintLogSources() {
  const q = $('#logSrcQ').value.toLowerCase();
  const groups = {};
  for (const c of S.containers) if (!q || c.name.toLowerCase().includes(q)) (groups[c.project || 'standalone'] ||= []).push(c);
  $('#logSources').innerHTML = Object.keys(groups).sort((a, b) => (a === 'standalone') - (b === 'standalone') || a.localeCompare(b)).map((g) =>
    `<div class="log-group">${esc(g)}</div>${groups[g].sort((a, b) => a.name.localeCompare(b.name)).map((c) => `<button class="log-src ${S.log.id === c.id ? 'sel' : ''}" data-log="${c.id}"><span class="dot dot-${stateOf(c).cls}"></span><span>${esc(c.name)}</span></button>`).join('')}`).join('') || '<p class="muted" style="padding:8px">No containers.</p>';
}
$('#logSrcQ').addEventListener('input', paintLogSources);
$('#logSources').addEventListener('click', (e) => { const b = e.target.closest('[data-log]'); if (b) location.hash = `#${S.lab ? 'ctlogs' : 'logs'}/${b.dataset.log}`; });
function openLog(id) {
  const c = S.byId.get(id);
  if (S.log.es) S.log.es.close();
  S.log.id = id; S.log.lines = [];
  paintLogSources();
  $('#logTitle').innerHTML = c ? `<span class="dot dot-${stateOf(c).cls}"></span>${esc(c.name)} <small class="muted">${esc(c.image)}</small>` : 'Container';
  $('#logLines').innerHTML = '<div class="log-empty">Loading…</div>';
  S.log.es = new EventSource(`/v1/containers/${id}/logs/stream?tail=1000`);
  S.log.es.addEventListener('log', (e) => {
    const chunk = JSON.parse(e.data);
    if (!chunk) { if (!S.log.lines.length) paintLog(); return; }
    S.log.lines.push(...chunk.split('\n').filter((l, i, a) => l || i < a.length - 1));
    if (S.log.lines.length > 20000) S.log.lines.splice(0, S.log.lines.length - 20000);
    paintLog();
  });
}
let logPaint = 0;
function paintLog() {
  cancelAnimationFrame(logPaint);
  logPaint = requestAnimationFrame(() => {
    const el = $('#logLines');
    const q = S.log.q.toLowerCase();
    const stick = S.log.follow;
    let lines = S.log.lines;
    if (S.log.level !== 'all') lines = lines.filter((l) => { const v = lvl(l); return S.log.level === 'error' ? v === 'error' : !!v; });
    if (q) lines = lines.filter((l) => l.toLowerCase().includes(q));
    const shown = lines.slice(-5000);
    const hl = (s) => (q ? esc(s).replace(new RegExp(q.replace(/[.*+?^${}()|[\]\\]/g, '\\$&').replace(/[&<>"']/g, (c) => esc(c)), 'gi'), (m) => `<mark>${m}</mark>`) : esc(s));
    el.innerHTML = shown.length ? shown.map((l) => `<div class="ll ${lvl(l)}">${hl(l)}</div>`).join('') : `<div class="log-empty">${S.log.lines.length ? 'No lines match.' : 'No output yet.'}</div>`;
    el.classList.toggle('wrap', S.log.wrap);
    if (stick) el.scrollTop = el.scrollHeight;
  });
}
$('#logLevel').addEventListener('click', (e) => { const b = e.target.closest('button'); if (!b) return; S.log.level = b.dataset.lv; segOn('#logLevel', b); paintLog(); });
$('#logQ').addEventListener('input', (e) => { S.log.q = e.target.value; paintLog(); });
$('#logFollow').addEventListener('click', (e) => { S.log.follow = !S.log.follow; e.currentTarget.classList.toggle('on', S.log.follow); paintLog(); });
$('#logWrap').addEventListener('click', (e) => { S.log.wrap = !S.log.wrap; e.currentTarget.classList.toggle('on', S.log.wrap); paintLog(); });
$('#logLines').addEventListener('wheel', () => { const el = $('#logLines'); const atEnd = el.scrollTop + el.clientHeight >= el.scrollHeight - 30; if (S.log.follow !== atEnd) { S.log.follow = atEnd; $('#logFollow').classList.toggle('on', atEnd); } }, { passive: true });

// ---------------------------------------------------------------- palette
let palItems = [], palSel = 0;
function openPalette() {
  $('#palette').hidden = false;
  $('#paletteQ').value = '';
  paintPalette();
  $('#paletteQ').focus();
}
function closePalette() { $('#palette').hidden = true; }
function paintPalette() {
  const q = $('#paletteQ').value.toLowerCase().trim();
  const pages = [...NAV, ...(S.lab ? SERVER_TABS.map(([id, i, l]) => [id === 'logs' ? 'ctlogs' : id, i, `${l} (this server)`]) : [])].filter(([id]) => id !== 'server').map(([id, icon, label]) => ({ icon, label, hint: 'page', go: () => (location.hash = `#${id}`) }));
  const extra = H.paletteItems ? H.paletteItems(q) : [];
  const cts = S.containers.map((c) => ({ icon: 'box', label: c.name, hint: stateOf(c).text, go: () => openDrawer(c.id) }));
  const sts = stackCache.map((s) => ({ icon: 'layers', label: s.name, hint: 'stack', go: () => (location.hash = '#stacks') }));
  const acts = [
    { icon: 'plus', label: 'New container', hint: 'action', go: () => { location.hash = '#containers'; $('#ctNew').click(); } },
    { icon: 'plus', label: 'New stack', hint: 'action', go: () => { location.hash = '#stacks'; openEditor(null); } },
    { icon: 'logout', label: 'Sign out', hint: 'action', go: signOut },
  ];
  palItems = [...extra, ...cts, ...sts, ...pages, ...acts].filter((x) => x.always || !q || x.label.toLowerCase().includes(q)).slice(0, 40);
  palSel = 0;
  $('#paletteList').innerHTML = palItems.map((x, i) => `<li data-i="${i}" class="${i === palSel ? 'sel' : ''}">${ic(x.icon)}<span>${esc(x.label)}</span><small>${esc(x.hint)}</small></li>`).join('') || '<li class="muted">Nothing found</li>';
}
$('#paletteQ').addEventListener('input', paintPalette);
$('#paletteQ').addEventListener('keydown', (e) => {
  if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
    e.preventDefault();
    palSel = clamp(palSel + (e.key === 'ArrowDown' ? 1 : -1), 0, palItems.length - 1);
    $$('#paletteList li').forEach((li, i) => li.classList.toggle('sel', i === palSel));
    $$('#paletteList li')[palSel]?.scrollIntoView({ block: 'nearest' });
  } else if (e.key === 'Enter' && palItems[palSel]) { closePalette(); palItems[palSel].go(); }
});
$('#paletteList').addEventListener('click', (e) => { const li = e.target.closest('li[data-i]'); if (li) { closePalette(); palItems[li.dataset.i].go(); } });
$('#palette').addEventListener('mousedown', (e) => { if (e.target.id === 'palette') closePalette(); });
$('#searchBtn').addEventListener('click', openPalette);
document.addEventListener('keydown', (e) => {
  if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === 'k') { e.preventDefault(); $('#palette').hidden ? openPalette() : closePalette(); }
  else if (e.key === '/' && !/input|textarea|select/i.test(document.activeElement.tagName)) { e.preventDefault(); openPalette(); }
});

// ---------------------------------------------------------------- account
async function signOut() {
  await api('/v1/ui/logout', { method: 'POST' }).catch(() => {});
  showLogin();
}
$('#userBtn').addEventListener('click', (e) => {
  const a = e.currentTarget;
  api('/v1/ui/me').then((m) => {
    S.me = m;
    const items = [['user', `Signed in as ${m.user}`, () => (location.hash = '#settings/account')], ['shield', 'Account & security', () => (location.hash = '#settings/account')]];
    if (can('Access.Audit', '/access')) items.push(['user', 'Users & permissions', () => (location.hash = '#settings/users')]);
    items.push(null, ['logout', 'Sign out', signOut]);
    showMenu(a, items);
  }).catch(() => {});
});

/** Does the signed-in caller hold `priv` on `area` (one of the areas /v1/ui/me reports)? */
function can(priv, area = '/') {
  const m = S.me; if (!m) return false;
  if (area === '*') return (m.anywhere || []).includes(priv);
  return ((m.privileges || {})[area] || []).includes(priv);
}

// ---------------------------------------------------------------- shared with lab.js
const H = {
  $, $$, esc, clamp, bytes, rate, pct, dur, now, ago, clock, parseSize, ic, ICONS, hydrateIcons, toast, api, S, ROUTES, SERVER_TABS,
  setNav, route, modal, confirmBox, can, showMenu, spark, sparkPath, segOn, openDrawer, stateOf, lineDiff,
  containerLogsHash: (c) => (S.lab ? `#logs/${encodeURIComponent(S.labHost)}/container/${encodeURIComponent(c.name)}` : `#logs/${c.id}`),
};

boot();

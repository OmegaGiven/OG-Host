// OG-Host lab UI: OmegaEnv's Home, Monitor, Explore, Watch, Apps, Logs and OpenClaw,
// on top of app.js's shell (helpers come in through init()). No third-party code.

let H, $, $$, esc, ic, api, toast, bytes, rate, S;
const L = {
  apps: [], hosts: [], overview: null, queue: [], groups: [], station: null, showHidden: false,
  sort: { key: 'state', dir: 1 }, openHosts: new Set(), editLayout: false, hostHist: {}, demoOn: false, demoCfg: { groups: [], keywords: [] },
};
const store = {
  get(k, d) { try { return JSON.parse(localStorage.getItem('og:' + k)) ?? d; } catch { return d; } },
  set(k, v) { try { localStorage.setItem('og:' + k, JSON.stringify(v)); } catch { /* private mode */ } },
};
const pct0 = (n) => (n == null || isNaN(n) ? '—' : `${Math.round(n)}%`);
const dfree = (d) => d.avail ?? d.total - d.used;
const dpct = (d) => (100 * d.used) / Math.max(1, d.used + dfree(d));
const level = (p) => (p >= 95 ? 'crit' : p >= 85 ? 'warn' : '');
const bar = (p, cls) => `<div class="bar ${cls ?? level(p)}"><i style="width:${Math.max(0, Math.min(100, p || 0)).toFixed(1)}%"></i></div>`;
const msAgo = (t) => { const s = Math.round((Date.now() - t) / 1000); return s < 60 ? `${s}s ago` : s < 3600 ? `${Math.round(s / 60)}m ago` : s < 86400 ? `${Math.round(s / 3600)}h ago` : `${Math.round(s / 86400)}d ago`; };
const uptime = (s) => { if (!s) return '—'; const d = Math.floor(s / 86400), h = Math.floor((s % 86400) / 3600), m = Math.floor((s % 3600) / 60); return d ? `${d}d ${h}h` : h ? `${h}h ${m}m` : `${m}m`; };
const hue = (s) => { let h = 0; for (const c of String(s)) h = (h * 31 + c.charCodeAt(0)) % 360; return h; };

export function init(h) {
  H = h; ({ $, $$, esc, ic, api, toast, bytes, rate, S } = h);
  L.demoOn = store.get('demo', false);
  H.setNav(
    [['home', 'grid', 'Home'], ['monitor', 'activity', 'Monitor'], ['explore', 'chart', 'Explore'], ['watch', 'eye', 'Watch'], ['apps', 'layers', 'Apps'], ['logs', 'logs', 'Logs'], ['server', 'server', 'Server']],
    [['overview', 'grid', 'Overview'], ['containers', 'box', 'Containers'], ['stacks', 'layers', 'Stacks'], ['images', 'disc', 'Images'], ['storage', 'drive', 'Storage'], ['ctlogs', 'logs', 'Logs'], ['vms', 'monitor', 'VMs']],
  );
  const R = H.ROUTES;
  // this server's pages live under "Server"
  for (const k of ['overview', 'containers', 'stacks', 'images', 'storage', 'vms']) R[k].tab = 'server';
  R.ctlogs = { ...R.logs, tab: 'server' };
  R.server = { el: 'overview', tab: 'server', run: () => { location.replace('#overview'); } };
  R.home = { el: 'home', tab: 'home', run: () => { renderHome(); refresh(); } };
  R.monitor = { el: 'monitor', tab: 'monitor', run: () => { renderMonitor(); refresh(true); } };
  R.explore = { el: 'explore', tab: 'explore', run: (_a, parts) => openExplore(parts || []).catch((e) => toast(e.message, true)), leave: () => clearInterval(ex.timer) };
  R.watch = { el: 'watch', tab: 'watch', run: () => loadWatches(), leave: () => clearInterval(wTimer) };
  R.apps = { el: 'apps', tab: 'apps', run: (_a, parts) => { L.appsSub = parts && parts[0] === 'add' ? 'add' : 'installed'; renderApps(); if (L.appsSub === 'add') loadCatalog(); } };
  R.logs = { el: 'labl', tab: 'logs', run: (_a, parts) => openLogs(parts || []), leave: () => clearInterval(logs.timer) };
  H.hydrateIcons();
  H.onContainers = () => {};
  H.onStats = () => {};
  H.paletteItems = paletteItems;
  $('#clawBtn').hidden = false;
  $('#demoBtn').hidden = false;
  bindHome(); bindMonitor(); bindExplore(); bindWatch(); bindApps(); bindLogs(); bindClaw(); bindDemo();
  loadDemoCfg().then(() => { applyDemoChrome(); refresh(true); refreshStation(); });
  setInterval(() => refresh(!$('#view-monitor').hidden), 10_000);
  setInterval(refreshStation, 30_000);
}

// ---------------------------------------------------------------- data
let refreshing = false;
async function refresh(withHistory) {
  if (refreshing) return;
  refreshing = true;
  try {
    const hist = withHistory ? (Object.keys(L.hostHist).length ? 1 : 360) : 0;
    const [apps, hosts, overview, queue, layout] = await Promise.all([api('/api/apps'), api(`/api/hosts?history=${hist}`), api('/api/overview'), api('/api/stacks'), api('/api/layout/groups').catch(() => null)]);
    for (const h of hosts) {
      const prev = L.hostHist[h.name] || [];
      const add = (h.history || []).filter((p) => !prev.length || p.t > prev[prev.length - 1].t);
      L.hostHist[h.name] = hist === 360 ? h.history || [] : [...prev, ...add].slice(-360);
    }
    Object.assign(L, { apps, hosts, overview, queue });
    if (layout) L.groups = layout.groups;
    applyDemoFilters();
    renderBadges();
    if (!$('#view-home').hidden) renderHome();
    if (!$('#view-monitor').hidden) renderMonitor();
    if (!$('#view-apps').hidden) renderApps();
  } catch (e) { if (e.message !== 'signed out') console.warn(e); } finally { refreshing = false; }
}
async function refreshStation() {
  try { L.station = await api('/api/widgets/station'); } catch { L.station = { error: 'unreachable' }; }
  if (!$('#view-home').hidden) renderWidgets();
  renderClawState();
}

function renderBadges() {
  const o = L.overview; if (!o) return;
  const crit = o.alerts.filter((a) => a.sev === 'crit').length, warn = o.alerts.filter((a) => a.sev === 'warn').length;
  const b = $('#alertBadge'); if (b) { b.hidden = !(crit || warn); b.textContent = crit || warn; b.className = 'tab-badge' + (crit ? '' : ' warn'); }
  const pending = L.queue.filter((s) => s.status === 'pending').length;
  const q = $('#queueBadge'); if (q) { q.hidden = !pending; q.textContent = pending; }
}

function summary() {
  const o = L.overview; if (!o) return ['', 'Checking your lab…'];
  const crit = o.alerts.filter((a) => a.sev === 'crit'), warn = o.alerts.filter((a) => a.sev === 'warn');
  const base = `${o.apps.up} of ${o.apps.total} apps healthy · ${o.hosts.online} of ${o.hosts.total} machines online`;
  if (crit.length) return ['crit', `${crit.length} ${crit.length === 1 ? 'thing needs' : 'things need'} attention — ${crit[0].title}`];
  if (warn.length) return ['warn', `Mostly fine · ${warn[0].title}${warn.length > 1 ? ` (+${warn.length - 1} more)` : ''}`];
  return ['ok', `Everything looks good · ${base}`];
}

function orderedHosts() {
  const hosts = [...L.hosts];
  const roots = hosts.filter((h) => !h.parent || !hosts.some((p) => p.name === h.parent));
  const out = [];
  const add = (h) => { out.push(h); hosts.filter((c) => c.parent === h.name).forEach(add); };
  roots.sort((a, b) => (b.guests?.length || 0) - (a.guests?.length || 0) || a.name.localeCompare(b.name)).forEach(add);
  return out;
}

// ---------------------------------------------------------------- icons for tiles
function iconHtml(icon, name) {
  const letter = `<span class="app-icon letter" style="background:linear-gradient(135deg,hsl(${hue(name)} 70% 55%),hsl(${(hue(name) + 40) % 360} 70% 45%))">${esc((name || '?').trim()[0]?.toUpperCase() || '?')}</span>`;
  if (!icon) return letter;
  const src = `/api/icon?src=${encodeURIComponent(icon)}`;
  if (icon.startsWith('mdi-')) return `<span class="app-icon"><i class="mask" style="--m:url('${src}')"></i></span>`;
  return `<span class="app-icon"><img src="${esc(src)}" alt="" loading="lazy" data-fallback="${esc(name)}"></span>`;
}
document.addEventListener('error', (e) => {
  const img = e.target;
  if (img.tagName === 'IMG' && img.dataset.fallback !== undefined) img.closest('.app-icon').outerHTML = iconHtml('', img.dataset.fallback);
}, true);

const healthState = (a) => (a.known ? 'known' : a.health?.state || 'pending');
const HEALTH = { up: 'Healthy', degraded: 'Slow', down: 'Not responding', unknown: 'No check', pending: 'Checking', known: 'Known issue', sleeping: 'Sleeping (starts on demand)' };
const DOT = { up: 'ok', degraded: 'warn', down: 'crit', known: 'known', sleeping: '', unknown: '', pending: '' };

// ---------------------------------------------------------------- HOME
function renderHome() {
  const h = new Date().getHours();
  $('#greeting').textContent = h < 5 ? 'Good night' : h < 12 ? 'Good morning' : h < 18 ? 'Good afternoon' : 'Good evening';
  const [lvl, text] = summary();
  $('#heroSub').innerHTML = `<span class="dot dot-${lvl}"></span> <a href="#monitor">${esc(text)}</a>`;
  renderWidgets();
  renderTiles();
}

function renderWidgets() {
  const w = [];
  const s = L.station;
  if (s && s.error !== 'not configured') {
    w.push(`<div class="widget"><div class="widget-head">${ic('gpu')}station GPU<span class="sp"></span>${s.error ? '<span class="dot dot-crit"></span>' : s.wolf_session ? '<span class="chip good">in game</span>' : ''}</div>
      <div class="seg mode-seg"><button class="${s.mode === 'ai' ? 'on ai' : ''}" data-mode="ai">${ic('sparkle')}AI mode</button><button class="${s.mode === 'gaming' ? 'on gaming' : ''}" data-mode="gaming">${ic('game')}Gaming</button></div>
      ${s.error ? '<div class="kv"><span>Mode control unreachable</span></div>' : `<div class="kv"><span>VRAM free</span><b>${s.vram_free_mib != null ? (s.vram_free_mib / 1024).toFixed(1) + ' GB' : '—'}</b></div>
      <div class="kv"><span>OpenClaw brain</span><b>${s.llama_server ? 'loaded' : 'off'}</b></div>`}</div>`);
  }
  const online = L.hosts.filter((x) => x.online);
  w.push(`<div class="widget"><div class="widget-head">${ic('servers')}Machines<span class="sp"></span><a class="muted" href="#monitor">${online.length}/${L.hosts.length} online</a></div>
    <div class="mini-hosts">${orderedHosts().map((x) => `<div class="mini-host"><b>${esc(x.name)}</b>${x.online ? bar(x.cpu?.pct, '') : '<span class="muted">offline</span>'}<span>${x.online ? pct0(x.cpu?.pct) : ''}</span></div>`).join('') || '<span class="muted">Waiting for agents…</span>'}</div></div>`);
  const disks = L.hosts.flatMap((x) => (x.disks || []).map((d) => ({ ...d, host: x.name }))).sort((a, b) => b.total - a.total);
  if (disks[0]) {
    const big = disks[0];
    w.push(`<div class="widget"><div class="widget-head">${ic('hdd')}Storage<span class="sp"></span><span class="muted">${esc(big.host)}:${esc(big.mount)}</span></div>
      <div class="widget-big">${bytes(dfree(big))}<small>free of ${bytes(big.total)}</small></div>${bar(dpct(big))}
      <div class="kv"><span>Across ${disks.length} volumes</span><b>${bytes(disks.reduce((a, d) => a + dfree(d), 0))} free</b></div></div>`);
  }
  const al = L.overview?.alerts || [];
  w.push(`<div class="widget"><div class="widget-head">${ic('bell')}Needs attention<span class="sp"></span><a class="muted" href="#monitor">${al.length || 'none'}</a></div>
    ${al.length ? al.slice(0, 3).map((a) => `<div class="kv"><span><span class="dot dot-${a.sev === 'info' ? 'busy' : a.sev}"></span>&nbsp; ${esc(a.title)}</span></div>`).join('') : `<div class="widget-big" style="color:var(--good)">${ic('check')}<small>All clear</small></div>`}</div>`);
  $('#homeWidgets').innerHTML = w.join('');
}

const homeCollapsed = () => new Set(store.get('homeCollapsed', []));
const setCollapsed = (set) => store.set('homeCollapsed', [...set]);

function tile(a) {
  const st = healthState(a), h = a.health || {};
  return `<a class="tile ${a.hidden ? 'is-hidden' : ''}" href="${esc(a.href)}" target="_blank" rel="noopener" data-id="${esc(a.id)}" title="${esc(a.known ? `Known issue${a.known.note ? ': ' + a.known.note : ''}` : a.description || a.name)}">
    ${iconHtml(a.icon, a.name)}
    <div class="tile-body"><div class="tile-name">${esc(a.name)}</div><div class="tile-desc">${esc(a.description || a.href.replace(/^https?:\/\//, ''))}</div>
      <div class="tile-meta">${a.host ? `<span class="chip">${esc(a.host)}</span>` : ''}<span>${esc(HEALTH[st] || st)}${h.ms != null && st !== 'down' ? ` · ${h.ms} ms` : ''}</span></div></div>
    <span class="dot dot-${DOT[st] ?? ''}" title="${esc(HEALTH[st] || st)}"></span>
    <button class="tile-menu" data-menu="${esc(a.id)}" title="Options">${ic('more')}</button></a>`;
}

function renderTiles() {
  const editing = L.editLayout;
  const visible = L.apps.filter((a) => L.showHidden || !a.hidden).sort((x, y) => (x.order ?? 0) - (y.order ?? 0));
  const byGroup = new Map((L.groups || []).map((g) => [g, []]));
  for (const a of visible) { if (!byGroup.has(a.group)) byGroup.set(a.group, []); byGroup.get(a.group).push(a); }
  const sections = [];
  const pinned = visible.filter((a) => a.pinned);
  if (pinned.length && !editing) sections.push(['Pinned', pinned, true]);
  for (const [g, list] of byGroup) {
    if (!list.length && !editing) continue;
    if (g === 'Ungrouped' && !list.length) continue;
    sections.push([g, list, false]);
  }
  const closed = homeCollapsed();
  const names = sections.filter((x) => !x[2]).map((x) => x[0]);
  $('#appGroups').classList.toggle('editing', editing);
  $('#appGroups').innerHTML = (editing ? `<div class="edit-banner">${ic('drag')}<span>Drag apps to move them between groups. Use the arrows to reorder groups.</span><button class="ghost-btn" data-addgroup>${ic('plus')} Add group</button><button class="primary-btn" data-doneedit>Done</button></div>` : '') +
    sections.map(([g, list, virtual]) => {
      const down = list.filter((a) => !a.hidden && healthState(a) === 'down').length;
      const isClosed = !editing && closed.has(g);
      const i = names.indexOf(g);
      const hiddenInDemo = L.demoCfg.groups.includes(g);
      const controls = editing && !virtual ? `<span class="sp"></span><span class="gctl">
        <button data-gup="${esc(g)}" ${i <= 0 ? 'disabled' : ''} title="Move group up">${ic('up')}</button>
        <button data-gdown="${esc(g)}" ${i >= names.length - 1 ? 'disabled' : ''} title="Move group down">${ic('down')}</button>
        ${g !== 'Ungrouped' ? `<button data-grename="${esc(g)}" title="Rename group">${ic('edit')}</button>
        <button data-gdemo="${esc(g)}" class="${hiddenInDemo ? 'demo-hidden' : ''}" title="${hiddenInDemo ? 'Hidden in demo mode — click to show it' : 'Shown in demo mode — click to hide it'}">${ic(hiddenInDemo ? 'eyeoff' : 'eye')}</button>
        <button data-gdelete="${esc(g)}" title="Delete group (its apps move to Ungrouped)">${ic('trash')}</button>` : ''}</span>` : '';
      return `<section class="${isClosed ? 'closed' : ''}" data-group="${esc(g)}">
        <div class="group-head ${editing ? '' : 'collapsible'} ${isClosed ? 'closed' : ''}" ${editing ? '' : `data-hc="${esc(g)}"`}>${ic(editing ? 'folder' : 'chevron', 'chev')}<h2>${esc(g)}</h2><span class="count">${list.length}</span>${isClosed && down ? `<span class="down-count">${down} down</span>` : ''}${controls}</div>
        <div class="app-grid" ${editing && !virtual ? `data-drop="${esc(g)}"` : ''}>${isClosed ? '' : list.map(tile).join('')}${editing && !virtual && !list.length ? '<div class="drop-hint">Drop apps here</div>' : ''}</div></section>`;
    }).join('') || '<p class="empty">No app tiles yet. Ask OpenClaw to add one, or import from Homepage.</p>';
  if (editing) $$('#appGroups [data-drop] .tile').forEach((t) => { t.draggable = true; });
  const wc = closed.has('__widgets');
  $('#homeWidgets').hidden = wc;
  $('[data-hc="__widgets"]').classList.toggle('closed', wc);
  const all = [...$$('#appGroups [data-hc]').map((e) => e.dataset.hc), '__widgets'].every((n) => closed.has(n));
  $('#collapseAll').textContent = all ? 'Expand all' : 'Collapse all';
  const hiddenCount = L.apps.filter((a) => a.hidden).length;
  const tb = $('#toggleHidden');
  tb.hidden = !hiddenCount;
  tb.textContent = L.showHidden ? 'Hide hidden apps' : `Show ${hiddenCount} hidden app${hiddenCount > 1 ? 's' : ''}`;
  $('#editLayout').classList.toggle('on', editing);
}

function ask(title, label, value = '', { textarea = false, placeholder = '' } = {}) {
  return new Promise((res) => {
    const { box, close } = H.modal(`<h3>${esc(title)}</h3><form class="form"><label>${esc(label)}${textarea ? `<textarea name="v" rows="3" placeholder="${esc(placeholder)}">${esc(value)}</textarea>` : `<input name="v" value="${esc(value)}" placeholder="${esc(placeholder)}">`}</label><div class="modal-actions"><button type="button" class="ghost-btn" data-x>Cancel</button><button class="primary-btn">OK</button></div></form>`);
    box.querySelector('[data-x]').onclick = () => { close(); res(null); };
    box.querySelector('form').onsubmit = (e) => { e.preventDefault(); const v = e.target.v.value; close(); res(v); };
    const inp = box.querySelector('[name=v]'); inp.focus(); inp.select?.();
  });
}

async function layoutCall(path, body, method = 'POST') {
  const r = await api(path, { method, body });
  if (r.groups) L.groups = r.groups;
  return r;
}
async function moveApps(items) {
  for (const x of items) { const a = L.apps.find((y) => y.id === x.id); if (a) Object.assign(a, x); }
  renderTiles();
  await layoutCall('/api/layout/apps', { items }).catch((e) => toast(e.message, true));
}
function placeApp(id, group, index) {
  const a = L.apps.find((x) => x.id === id); if (!a) return Promise.resolve();
  const from = a.group;
  const target = L.apps.filter((x) => x.group === group && x.id !== id).sort((p, q) => (p.order ?? 0) - (q.order ?? 0));
  target.splice(Math.max(0, Math.min(index, target.length)), 0, a);
  const items = target.map((x, i) => ({ id: x.id, group, order: (i + 1) * 10 }));
  if (from !== group) L.apps.filter((x) => x.group === from && x.id !== id).sort((p, q) => (p.order ?? 0) - (q.order ?? 0)).forEach((x, i) => items.push({ id: x.id, group: from, order: (i + 1) * 10 }));
  return moveApps(items);
}
async function patchApp(a, patch) {
  try { Object.assign(a, await api(`/api/apps/${encodeURIComponent(a.id)}`, { method: 'PATCH', body: patch })); renderTiles(); } catch (e) { toast(e.message, true); }
}
async function markKnown(target, label) {
  const note = await ask(`Mark “${label}” as a known issue`, 'It turns grey and stops alerting until you clear it. What’s wrong / why:', '', { textarea: true });
  if (note === null) return;
  try { await api('/api/known', { method: 'POST', body: { ...target, note } }); toast('Marked as known issue — alerts muted'); refresh(); loadProjects(true); } catch (e) { toast(e.message, true); }
}
async function clearKnown(key) {
  try { await api('/api/known', { method: 'DELETE', body: { key } }); toast('Known issue cleared — alerts back on'); refresh(); loadProjects(true); } catch (e) { toast(e.message, true); }
}

function bindHome() {
  $('#homeWidgets').addEventListener('click', async (e) => {
    const b = e.target.closest('[data-mode]'); if (!b) return;
    const mode = b.dataset.mode;
    if (L.station?.mode === mode) return;
    $$('#homeWidgets .mode-seg button').forEach((x) => (x.disabled = true));
    toast(mode === 'gaming' ? 'Freeing the GPU for gaming…' : 'Switching station to AI mode…');
    try {
      const r = await api(`/api/widgets/station/${mode}`, { method: 'POST' });
      L.station = r.status;
      toast(L.station?.mode === mode ? `station is in ${mode === 'ai' ? 'AI' : 'Gaming'} mode` : r.result?.text || 'Mode change refused');
    } catch (err) { toast(err.message, true); }
    renderWidgets();
  });
  $('#editLayout').addEventListener('click', async (e) => { e.stopPropagation(); L.editLayout = !L.editLayout; renderTiles(); });
  $('#toggleHidden').addEventListener('click', () => { L.showHidden = !L.showHidden; renderTiles(); });
  $('#collapseAll').addEventListener('click', (e) => {
    e.stopPropagation();
    const names = [...$$('#appGroups [data-hc]').map((x) => x.dataset.hc), '__widgets'];
    const set = homeCollapsed(); const allClosed = names.every((n) => set.has(n));
    names.forEach((n) => (allClosed ? set.delete(n) : set.add(n)));
    setCollapsed(set); renderTiles();
  });
  $('#view-home').addEventListener('click', (e) => {
    if (L.editLayout && e.target.closest('#appGroups')) return;
    if (e.target.closest('button,a')) return;
    const h = e.target.closest('[data-hc]'); if (!h) return;
    const set = homeCollapsed(), n = h.dataset.hc;
    set.has(n) ? set.delete(n) : set.add(n);
    setCollapsed(set); renderTiles();
  });
  let dragId = null;
  const grid = $('#appGroups');
  grid.addEventListener('dragstart', (e) => { const t = e.target.closest('.tile'); if (!t || !L.editLayout) return; dragId = t.dataset.id; t.classList.add('dragging'); e.dataTransfer.effectAllowed = 'move'; e.dataTransfer.setData('text/plain', dragId); });
  grid.addEventListener('dragend', (e) => { e.target.closest('.tile')?.classList.remove('dragging'); $$('.drop-over').forEach((x) => x.classList.remove('drop-over')); $('.drop-mark')?.remove(); dragId = null; });
  const dropIndex = (g, x, y) => {
    const tiles = [...g.querySelectorAll('.tile:not(.dragging)')];
    for (let i = 0; i < tiles.length; i++) {
      const r = tiles[i].getBoundingClientRect();
      if (y < r.top + r.height / 2 && (y < r.top || x < r.left + r.width / 2)) return i;
      if (y >= r.top && y <= r.bottom && x < r.left + r.width / 2) return i;
    }
    return tiles.length;
  };
  grid.addEventListener('dragover', (e) => {
    const g = e.target.closest('[data-drop]'); if (!g || !dragId) return;
    e.preventDefault(); e.dataTransfer.dropEffect = 'move';
    $$('.drop-over').forEach((x) => x !== g && x.classList.remove('drop-over')); g.classList.add('drop-over');
    const idx = dropIndex(g, e.clientX, e.clientY);
    let mark = $('.drop-mark'); if (!mark) { mark = document.createElement('div'); mark.className = 'drop-mark'; }
    const tiles = [...g.querySelectorAll('.tile:not(.dragging)')];
    if (tiles[idx]) g.insertBefore(mark, tiles[idx]); else g.appendChild(mark);
  });
  grid.addEventListener('drop', (e) => {
    const g = e.target.closest('[data-drop]'); if (!g || !dragId) return;
    e.preventDefault();
    placeApp(dragId, g.dataset.drop, dropIndex(g, e.clientX, e.clientY));
  });
  grid.addEventListener('click', async (e) => {
    const m = e.target.closest('[data-menu]');
    if (m) {
      e.preventDefault(); e.stopPropagation();
      const a = L.apps.find((x) => x.id === m.dataset.menu); if (!a) return;
      H.showMenu(m, [
        ['pin', a.pinned ? 'Unpin' : 'Pin to top', () => patchApp(a, { pinned: !a.pinned })],
        [a.hidden ? 'eye' : 'eyeoff', a.hidden ? 'Show' : 'Hide', () => patchApp(a, { hidden: !a.hidden })],
        a.known ? ['check', 'Clear known issue', () => clearKnown(a.known.key)] : ['note', 'Mark as known issue…', () => markKnown({ appId: a.id }, a.name)],
        ['sleep', a.onDemand ? 'Always-on app (alert when down)' : 'Starts on demand (no alert when asleep)', () => patchApp(a, { onDemand: !a.onDemand })],
        [a.sensitive ? 'eye' : 'eyeoff', a.sensitive ? 'Show in demo mode' : 'Hide in demo mode', () => patchApp(a, { sensitive: !a.sensitive })],
        ['folder', 'Move to group…', () => setTimeout(() => H.showMenu(m, [...(L.groups || []).filter((g) => g !== a.group).map((g) => ['folder', g, () => placeApp(a.id, g, 9999)]), ['plus', 'New group…', async () => { const n = (await ask('New group', 'Name'))?.trim(); if (n) { if (!(L.groups || []).includes(n)) await layoutCall('/api/layout/groups', { name: n }); placeApp(a.id, n, 9999); } }]]))],
        ['sparkle', 'Ask OpenClaw about it', () => openClaw(`Tell me about ${a.name} (${a.href}) in my lab: what it does, its health, and anything I should tune.`)],
      ]);
      return;
    }
    if (!L.editLayout) return;
    const t = e.target.closest('a.tile'); if (t) { e.preventDefault(); return; }
    const b = e.target.closest('button'); if (!b) return;
    const d = b.dataset;
    try {
      if (d.doneedit != null) { L.editLayout = false; renderTiles(); toast('Layout saved'); }
      if (d.addgroup != null) { const n = (await ask('New group', 'Name'))?.trim(); if (n) { await layoutCall('/api/layout/groups', { name: n }); renderTiles(); } }
      if (d.grename) { const n = (await ask(`Rename “${d.grename}”`, 'New name', d.grename))?.trim(); if (n && n !== d.grename) { await layoutCall('/api/layout/groups/rename', { from: d.grename, to: n }); await refresh(); } }
      if (d.gdelete) {
        const count = L.apps.filter((a) => a.group === d.gdelete).length;
        if (!(await H.confirmBox(`Delete “${d.gdelete}”?`, count ? `Its ${count} app${count > 1 ? 's' : ''} move to Ungrouped.` : 'The group is empty.', 'Delete'))) return;
        await layoutCall('/api/layout/groups/delete', { name: d.gdelete }); await refresh();
      }
      if (d.gdemo) {
        const groups = L.demoCfg.groups.includes(d.gdemo) ? L.demoCfg.groups.filter((x) => x !== d.gdemo) : [...L.demoCfg.groups, d.gdemo];
        L.demoCfg = await api('/api/demo', { method: 'PUT', body: { ...L.demoCfg, groups } }); renderTiles();
        toast(groups.includes(d.gdemo) ? `“${d.gdemo}” is hidden in demo mode` : `“${d.gdemo}” shows in demo mode`);
      }
      if (d.gup || d.gdown) {
        const g = d.gup || d.gdown, list = [...L.groups], i = list.indexOf(g), j = d.gup ? i - 1 : i + 1;
        if (i < 0 || j < 0 || j >= list.length) return;
        [list[i], list[j]] = [list[j], list[i]];
        await layoutCall('/api/layout/groups', { groups: list }, 'PUT'); renderTiles();
      }
    } catch (err) { toast(err.message, true); }
  });
}

// ---------------------------------------------------------------- MONITOR
function sparkHost(points, key, max = 100, cls = '') {
  const vals = points.map((p) => p[key]).filter((v) => v != null);
  if (vals.length < 2) return `<svg class="spark sm ${cls}"></svg>`;
  const W = 200, Hh = 22, n = vals.length, top = Math.max(max, ...vals);
  const xy = vals.map((v, i) => [(i / (n - 1)) * W, Hh - 1 - (v / top) * (Hh - 2)]);
  const line = xy.map((p, i) => `${i ? 'L' : 'M'}${p[0].toFixed(1)} ${p[1].toFixed(1)}`).join('');
  return `<svg class="spark sm ${cls}" viewBox="0 0 ${W} ${Hh}" preserveAspectRatio="none"><path class="area" d="${line}L${W} ${Hh}L0 ${Hh}Z"/><path class="line" d="${line}"/></svg>`;
}

function hostCard(h) {
  const memP = h.mem ? (100 * (h.mem.total - h.mem.available)) / h.mem.total : null;
  const ctrs = h.containers || [];
  const running = ctrs.filter((c) => c.state === 'running').length;
  const isPve = (h.guests || []).length > 0;
  const open = L.openHosts.has(h.name);
  const hist = L.hostHist[h.name] || [];
  const iconName = isPve ? 'servers' : h.ogHost ? 'server' : (h.gpus || []).length ? 'gpu' : h.parent ? 'monitor' : 'server';
  return `<div class="host ${h.online ? '' : 'offline'}">
    <div class="host-head"><span class="host-icon">${ic(iconName)}</span>
      <div><h3>${esc(h.label || h.name)} ${h.parent ? `<span class="child-tag">on ${esc(h.parent)}</span>` : ''}${h.ogHost ? '<span class="chip accent">OG-Host</span>' : ''}</h3>
      <small>${esc(h.os || '')} · up ${uptime(h.uptime)}${h.online ? '' : ` · last seen ${msAgo(h.seenAt)}`}</small></div>
      <span class="dot dot-${h.online ? 'ok' : 'crit'}"></span></div>
    <div class="metric clickable" data-ex="${esc(h.name)}/cpu" title="CPU history"><span>CPU</span><div class="metric-stack">${bar(h.cpu?.pct, '')}${sparkHost(hist, 'cpu')}</div><span class="val">${pct0(h.cpu?.pct)} · ${h.cpu?.cores ?? '?'}c</span></div>
    <div class="metric clickable" data-ex="${esc(h.name)}/mem_used_pct" title="Memory history"><span>Memory</span><div class="metric-stack">${bar(memP)}${sparkHost(hist, 'mem')}</div><span class="val">${h.mem ? `${bytes(h.mem.total - h.mem.available, 0)}/${bytes(h.mem.total, 0)}` : '—'}${h.mem?.swapTotal ? `<br>swap ${pct0((100 * (h.mem.swapTotal - h.mem.swapFree)) / h.mem.swapTotal)}` : ''}</span></div>
    ${(h.gpus || []).map((g) => `<div class="metric"><span>GPU</span><div class="metric-stack">${bar((100 * g.memUsed) / Math.max(1, g.memTotal), '')}${sparkHost(hist, 'gpu', 100, 'good')}</div><span class="val">${pct0(g.util)} · ${bytes(g.memUsed, 1)}<br>${g.temp ?? '?'}°C${g.power ? ` · ${Math.round(g.power)}W` : ''}</span></div>`).join('')}
    <div class="metric clickable" data-ex="${esc(h.name)}/net_rx" title="Network history"><span>Network</span><div class="metric-stack">${sparkHost(hist, 'rx', 1)}</div><span class="val">↓${rate(h.net?.rxBps)}<br>↑${rate(h.net?.txBps)}</span></div>
    ${(h.disks || []).map((d) => `<div class="metric"><span title="${esc(d.mount)} · ${esc(d.device || '')}">${esc(d.mount.length > 9 ? '…' + d.mount.slice(-8) : d.mount)}</span>${bar(dpct(d))}<span class="val">${bytes(dfree(d), 0)} free</span></div>`).join('')}
    <div class="host-foot">
      ${h.temp != null ? `<span class="chip">${ic('thermo')} ${h.temp}°C</span>` : ''}
      ${h.docker ? `<span class="chip">${running}/${ctrs.length} containers</span>` : ''}
      ${isPve ? `<span class="chip">${h.guests.filter((g) => g.status === 'running').length}/${h.guests.length} guests</span>` : ''}
      ${h.cpu?.load?.length ? `<span class="chip">load ${Number(h.cpu.load[0]).toFixed(2)}</span>` : ''}
      ${h.console ? `<a class="link-btn" href="${esc(h.console)}" target="_blank" rel="noopener">${ic('external')} Console</a>` : ''}
      ${h.ogHost && h.name === S.labHost ? '<a class="link-btn" href="#overview">Server view</a>' : ''}
      ${ctrs.length || isPve ? `<button class="link-btn" data-host="${esc(h.name)}">${open ? 'Hide' : 'Details'}</button>` : ''}
    </div>
    ${open ? `<div class="host-detail">
      ${isPve ? h.guests.map((g) => `<div class="row-mini"><span class="dot dot-${g.status === 'running' ? 'ok' : ''}"></span><b>${esc(g.vmid)} · ${esc(g.host || g.name)}${g.host && g.host !== g.name ? ` <span class="muted">${esc(g.name)}</span>` : ''}</b><span>${g.status === 'running' ? pct0(g.cpu) : esc(g.status)}</span><span>${g.maxmem ? bytes(g.mem, 0) + '/' + bytes(g.maxmem, 0) : ''}</span></div>`).join('') : ''}
      ${ctrs.slice().sort((a, b) => (b.mem || 0) - (a.mem || 0)).map((c) => `<div class="row-mini clickable" data-logs="${esc(h.name)}/container/${esc(c.name)}" title="${esc(c.image)} — ${esc(c.status)} · click for logs"><span class="dot dot-${c.state === 'running' ? (/unhealthy/.test(c.status) ? 'warn' : 'ok') : c.state === 'restarting' ? 'crit' : ''}"></span><b>${esc(c.name)}</b><span>${c.cpu != null ? Number(c.cpu).toFixed(1) + '%' : ''}</span><span>${c.mem ? bytes(c.mem, 0) : esc(c.state)}</span></div>`).join('')}
    </div>` : ''}
  </div>`;
}

function pveTile(h) {
  const memP = h.mem ? (100 * (h.mem.total - h.mem.available)) / h.mem.total : null;
  const g = h.guests || [], up = g.filter((x) => x.status === 'running').length;
  const body = `<span class="host-icon">${ic('servers')}</span><div class="pve-main"><b>${esc(h.label || h.name)}</b><small>${g.length ? `${up}/${g.length} guests running` : 'no guests'} · up ${uptime(h.uptime)}</small>
    <div class="pve-bars"><span>CPU ${pct0(h.cpu?.pct)}</span><span>Mem ${pct0(memP)}</span>${bar(h.cpu?.pct, '')}${bar(memP)}</div></div><span class="dot dot-${h.online ? 'ok' : 'crit'}"></span>${h.console ? ic('external', 'pve-go') : ''}`;
  return h.console ? `<a class="pve-tile ${h.online ? '' : 'offline'}" href="${esc(h.console)}" target="_blank" rel="noopener" title="Open the web console">${body}</a>` : `<div class="pve-tile ${h.online ? '' : 'offline'}">${body}</div>`;
}

function renderMonitor() {
  const o = L.overview;
  const [lvl, text] = summary();
  $('#banner').className = `status-banner ${lvl}`;
  $('#banner').innerHTML = `<span class="dot big-dot dot-${lvl}"></span><div><h1>${lvl === 'ok' ? 'All systems healthy' : lvl === 'warn' ? 'Mostly healthy' : lvl === 'crit' ? 'Something needs you' : 'Checking…'}</h1><p>${esc(text)}</p></div>`;
  $('#alerts').innerHTML = (o?.alerts || []).map((a) => `<div class="alert ${a.sev}">${ic(a.sev === 'info' ? 'bell' : 'alert')}<b>${esc(a.title)}</b><span>${esc(a.detail || '')}</span>${a.log ? `<a class="link-btn" href="#logs/${[a.log.host, a.log.kind, a.log.source].map(encodeURIComponent).join('/')}">View logs</a>` : ''}${a.stackId ? '<a class="link-btn" href="#apps">Review</a>' : ''}${a.appId || a.log ? `<button class="link-btn" data-ask="${esc(a.title)}: ${esc(a.detail)}">${ic('sparkle')} Ask</button>` : ''}</div>`).join('');
  const pves = L.hosts.filter((h) => h.console || (h.guests || []).length).sort((a, b) => a.name.localeCompare(b.name));
  $('#pveWrap').hidden = !pves.length;
  $('#pveMeta').textContent = `${pves.reduce((n, h) => n + (h.guests || []).filter((g) => g.status === 'running').length, 0)} guests running`;
  $('#pveGrid').innerHTML = pves.map(pveTile).join('');
  const hosts = orderedHosts();
  $('#hostsMeta').textContent = `${hosts.filter((h) => h.online).length} of ${hosts.length} online · updates every 10 s`;
  $('#hostGrid').innerHTML = hosts.map(hostCard).join('') || '<p class="empty">No machines reporting yet. Run <code>og-hostd lab agent &lt;name&gt;</code> for an agent’s settings.</p>';
  const disks = L.hosts.flatMap((h) => (h.disks || []).map((d) => ({ ...d, host: h.name }))).sort((a, b) => dpct(b) - dpct(a));
  $('#storageMeta').textContent = `${bytes(disks.reduce((a, d) => a + dfree(d), 0))} free of ${bytes(disks.reduce((a, d) => a + d.total, 0))}`;
  $('#labStorage').innerHTML = disks.map((d) => { const p = dpct(d); return `<div class="disk"><div><b>${esc(d.mount)}</b> <small class="muted">on ${esc(d.host)}${d.fs ? ` · ${esc(d.fs)}` : ''} · ${bytes(d.total)}</small></div><div class="right">${pct0(p)} · ${bytes(dfree(d))} free</div>${bar(p)}</div>`; }).join('') || '<p class="muted">No disks reported.</p>';
  const ctrs = L.hosts.flatMap((h) => (h.containers || []).filter((c) => c.mem).map((c) => ({ ...c, host: h.name }))).sort((a, b) => b.mem - a.mem).slice(0, 10);
  $('#heavy').innerHTML = ctrs.map((c) => `<div class="mini-row click" data-logs="${esc(c.host)}/container/${esc(c.name)}"><b>${esc(c.name)} <small class="muted">${esc(c.host)}</small></b><span>${c.cpu != null ? Number(c.cpu).toFixed(1) + '%' : ''}</span><span>${bytes(c.mem)}</span></div>`).join('') || '<p class="muted">No container stats yet.</p>';
  renderAppTable();
}

function renderAppTable() {
  const rank = { down: 0, degraded: 1, known: 2, pending: 3, sleeping: 4, unknown: 5, up: 6 };
  const { key, dir } = L.sort;
  const rows = L.apps.filter((a) => !a.hidden).map((a) => ({ a, st: healthState(a), ms: a.health?.ms ?? Infinity }));
  rows.sort((x, y) => dir * (key === 'state' ? rank[x.st] - rank[y.st] || x.a.name.localeCompare(y.a.name) : key === 'ms' ? x.ms - y.ms : String(x.a[key] || '').localeCompare(String(y.a[key] || ''))));
  $('#appsMeta').textContent = `${rows.filter((r) => r.st === 'up').length} healthy · ${rows.filter((r) => r.st === 'down').length} down · checked every 30 s`;
  const th = (k, l) => `<th data-sort="${k}" class="${key === k ? 'sorted' : ''}">${l}${key === k ? (dir > 0 ? ' ↑' : ' ↓') : ''}</th>`;
  $('#appTable').innerHTML = `<thead><tr>${th('state', 'Status')}${th('name', 'App')}${th('host', 'Host')}${th('group', 'Group')}${th('ms', 'Response')}</tr></thead><tbody>${rows.map(({ a, st }) => `
    <tr><td><span class="state"><span class="dot dot-${DOT[st] ?? ''}"></span> ${esc(HEALTH[st])}</span></td><td><a href="${esc(a.href)}" target="_blank" rel="noopener">${esc(a.name)}</a></td><td>${esc(a.host)}</td><td class="muted">${esc(a.group)}</td>
    <td class="num">${a.health?.ms != null ? a.health.ms + ' ms' : a.health?.code ? 'HTTP ' + a.health.code : esc(a.health?.error || '—')}</td></tr>`).join('')}</tbody>`;
}

function bindMonitor() {
  $('#hostGrid').addEventListener('click', (e) => {
    const x = e.target.closest('[data-ex]'); if (x) { location.hash = 'explore/q/' + x.dataset.ex.split('/').map(encodeURIComponent).join('/'); return; }
    const l = e.target.closest('[data-logs]'); if (l) { location.hash = 'logs/' + l.dataset.logs.split('/').map(encodeURIComponent).join('/'); return; }
    const b = e.target.closest('[data-host]'); if (!b) return;
    const n = b.dataset.host; L.openHosts.has(n) ? L.openHosts.delete(n) : L.openHosts.add(n);
    renderMonitor();
  });
  $('#heavy').addEventListener('click', (e) => { const l = e.target.closest('[data-logs]'); if (l) location.hash = 'logs/' + l.dataset.logs.split('/').map(encodeURIComponent).join('/'); });
  $('#alerts').addEventListener('click', (e) => { const b = e.target.closest('[data-ask]'); if (b) openClaw(`In my homelab: ${b.dataset.ask}. What's the likely cause and how do I fix it?`); });
  $('#appTable').addEventListener('click', (e) => { const th = e.target.closest('[data-sort]'); if (!th) return; const k = th.dataset.sort; L.sort = { key: k, dir: L.sort.key === k ? -L.sort.dir : 1 }; renderAppTable(); });
}

// ---------------------------------------------------------------- EXPLORE
const EX_RANGES = { '1h': 3600e3, '6h': 6 * 3600e3, '24h': 86400e3, '7d': 7 * 86400e3, '30d': 30 * 86400e3, '1y': 365 * 86400e3 };
const EX_COLORS = ['#8b7bff', '#3ddc97', '#5ec8ff', '#ffb547', '#ff5d6c', '#f472b6', '#a3e635', '#fb923c', '#22d3ee', '#c084fc'];
const ex = { catalog: null, catalogAt: 0, views: [], viewId: store.get('exView', ''), panels: [], range: '24h', custom: null, auto: true, charts: [], timer: null, dirty: false, hoverT: null };

function fmtUnit(unit, v) {
  if (v == null || isNaN(v)) return '—';
  switch (unit) {
    case 'pct': return v.toFixed(v < 10 ? 1 : 0) + '%';
    case 'bytes': return bytes(v);
    case 'Bps': return rate(v);
    case 'C': return Math.round(v) + '°C';
    case 'W': return Math.round(v) + ' W';
    case 'ms': return Math.round(v) + ' ms';
    case 'usd': return '$' + (+v).toFixed(2);
    case 'bool': return v >= 0.999 ? 'up' : v <= 0.001 ? 'down' : Math.round(v * 100) + '% up';
    default: return Math.abs(v) >= 100 ? String(Math.round(v)) : String(+v.toFixed(2));
  }
}
const seriesName = (s) => {
  const m = ex.catalog?.metrics[s.metric]?.title || s.metric;
  if (s.host === 'apps') return `${L.apps.find((a) => a.id === s.label)?.name || s.label} · ${m}`;
  if (s.host === 'watches') return `${watchList.find((w) => w.id === s.label)?.name || s.label} · ${m}`;
  return `${s.host}${s.label ? ' ' + s.label : ''} · ${m}`;
};

/** Time-series chart on canvas: several lines, shared hover time, drag to zoom. */
class TimeChart {
  constructor(el, opts) {
    this.el = el; this.opts = opts;
    el.innerHTML = '<canvas></canvas><div class="tc-legend"></div>';
    this.cv = el.querySelector('canvas'); this.legend = el.querySelector('.tc-legend');
    this.data = { t: [], series: [] }; this.drag = null;
    this.cv.addEventListener('mousemove', (e) => { const t = this.xToT(e.offsetX); if (this.drag) { this.drag.x1 = e.offsetX; this.draw(); } opts.onHover?.(t); });
    this.cv.addEventListener('mouseleave', () => opts.onHover?.(null));
    this.cv.addEventListener('mousedown', (e) => { this.drag = { x0: e.offsetX, x1: e.offsetX }; });
    window.addEventListener('mouseup', this.up = (e) => {
      if (!this.drag) return;
      const { x0, x1 } = this.drag; this.drag = null;
      if (Math.abs(x1 - x0) > 8) opts.onZoom?.(this.xToT(Math.min(x0, x1)), this.xToT(Math.max(x0, x1)));
      this.draw();
    });
    this.cv.addEventListener('dblclick', () => opts.onReset?.());
  }
  destroy() { window.removeEventListener('mouseup', this.up); }
  set(data, from, to, unit) { this.data = data; this.from = from; this.to = to; this.unit = unit; this.draw(); }
  geom() { const W = this.cv.clientWidth, Hh = this.cv.clientHeight; return { W, Hh, l: 62, r: 10, t: 8, b: 22 }; }
  xToT(x) { const g = this.geom(); return this.from + ((x - g.l) / Math.max(1, g.W - g.l - g.r)) * (this.to - this.from); }
  draw() {
    const cv = this.cv, dpr = window.devicePixelRatio || 1, g = this.geom();
    if (!g.W) return;
    if (cv.width !== g.W * dpr || cv.height !== g.Hh * dpr) { cv.width = g.W * dpr; cv.height = g.Hh * dpr; }
    const c = cv.getContext('2d'); c.setTransform(dpr, 0, 0, dpr, 0, 0); c.clearRect(0, 0, g.W, g.Hh);
    const css = getComputedStyle(document.documentElement);
    const lineC = css.getPropertyValue('--line').trim(), textC = css.getPropertyValue('--text-3').trim();
    const { t, series } = this.data, unit = this.unit;
    const w = g.W - g.l - g.r, h = g.Hh - g.t - g.b;
    let mn = Infinity, mx = -Infinity;
    for (const s of series) for (const v of s.vals) if (v != null) { mn = Math.min(mn, v); mx = Math.max(mx, v); }
    if (!isFinite(mn)) { mn = 0; mx = 1; }
    let lo = Math.min(0, mn), hi = unit === 'pct' ? Math.max(100, mx) : unit === 'bool' ? 1.05 : mx * 1.08 || 1;
    if (hi <= lo) hi = lo + 1;
    const X = (tt) => g.l + ((tt - this.from) / Math.max(1, this.to - this.from)) * w;
    const Y = (v) => g.t + h - ((v - lo) / (hi - lo)) * h;
    c.font = '11px ui-monospace, monospace'; c.fillStyle = textC; c.strokeStyle = lineC; c.lineWidth = 1;
    for (let i = 0; i <= 4; i++) { const y = g.t + (h * i) / 4; c.beginPath(); c.moveTo(g.l, y + 0.5); c.lineTo(g.W - g.r, y + 0.5); c.stroke(); c.textAlign = 'right'; c.fillText(fmtUnit(unit, hi - ((hi - lo) * i) / 4), g.l - 6, y + 4); }
    const span = this.to - this.from;
    const tfmt = (tt) => { const d = new Date(tt); return span > 2 * 86400e3 ? d.toLocaleDateString([], { month: 'short', day: 'numeric' }) : d.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' }); };
    c.textAlign = 'center';
    for (let i = 0; i <= 4; i++) { const tt = this.from + (span * i) / 4; c.fillText(tfmt(tt), Math.max(g.l + 24, Math.min(g.W - g.r - 24, X(tt))), g.Hh - 6); }
    series.forEach((s, k) => {
      c.strokeStyle = s.color; c.lineWidth = 1.6; c.beginPath();
      let pen = false;
      s.vals.forEach((v, i) => { if (v == null) { pen = false; return; } const x = X(t[i]), y = Y(v); if (pen) c.lineTo(x, y); else { c.moveTo(x, y); pen = true; } });
      c.stroke();
      if (series.length === 1) {
        c.lineTo(X(t[t.length - 1] ?? this.to), g.t + h); c.lineTo(X(t[0] ?? this.from), g.t + h); c.closePath();
        const gr = c.createLinearGradient(0, g.t, 0, g.t + h); gr.addColorStop(0, s.color + '33'); gr.addColorStop(1, s.color + '00');
        c.fillStyle = gr; c.fill();
      }
    });
    if (this.drag) { c.fillStyle = 'rgba(139,123,255,.18)'; const a = Math.min(this.drag.x0, this.drag.x1); c.fillRect(a, g.t, Math.abs(this.drag.x1 - this.drag.x0), h); }
    // hover: crosshair + legend values at the nearest time
    const ht = ex.hoverT;
    let idx = -1;
    if (ht != null && t.length && ht >= this.from && ht <= this.to) {
      let best = Infinity;
      t.forEach((tt, i) => { const d = Math.abs(tt - ht); if (d < best) { best = d; idx = i; } });
      const x = X(t[idx]); c.strokeStyle = textC; c.setLineDash([3, 3]); c.beginPath(); c.moveTo(x, g.t); c.lineTo(x, g.t + h); c.stroke(); c.setLineDash([]);
    }
    this.legend.innerHTML = (idx >= 0 ? `<span class="tc-time">${new Date(t[idx]).toLocaleString([], { month: 'short', day: 'numeric', hour: '2-digit', minute: '2-digit' })}</span>` : '') +
      series.map((s) => { const v = idx >= 0 ? s.vals[idx] : [...s.vals].reverse().find((x) => x != null); return `<span><i style="background:${s.color}"></i>${esc(s.label)} <b>${fmtUnit(s.unit, v)}</b></span>`; }).join('');
  }
}

function defaultPanels() {
  const series = ex.catalog?.series || [];
  const hostsWith = (metric) => [...new Set(series.filter((s) => s.metric === metric).map((s) => s.host))];
  const all = (metric) => series.filter((s) => s.metric === metric).map((s) => ({ host: s.host, metric, label: s.label }));
  return [
    { title: 'CPU by machine', series: hostsWith('cpu').map((h) => ({ host: h, metric: 'cpu', label: '' })) },
    { title: 'Memory used by machine', series: hostsWith('mem_used_pct').map((h) => ({ host: h, metric: 'mem_used_pct', label: '' })) },
    { title: 'Disks filling up', series: all('disk_used_pct') },
    { title: 'GPU utilisation', series: all('gpu_util') },
    { title: 'Network in', series: hostsWith('net_rx').map((h) => ({ host: h, metric: 'net_rx', label: '' })) },
    { title: 'Swap used', series: hostsWith('swap_used_pct').map((h) => ({ host: h, metric: 'swap_used_pct', label: '' })) },
  ].filter((x) => x.series.length);
}

async function openExplore(parts) {
  if (!ex.catalog || Date.now() - ex.catalogAt > 60_000) {
    ex.catalog = await api('/api/metrics/series'); ex.catalogAt = Date.now();
    if (L.demoOn) { const ids = new Set(L.apps.map((a) => a.id)); ex.catalog.series = ex.catalog.series.filter((x) => !sensitiveName(x.label) && !(x.host === 'apps' && !ids.has(x.label))); }
  }
  ex.views = await api('/api/views').catch(() => []);
  if (parts[0] === 'q' && parts[1] && parts[2]) {
    ex.viewId = ''; ex.panels = [{ title: seriesName({ host: parts[1], metric: parts[2], label: parts[3] || '' }), series: [{ host: parts[1], metric: parts[2], label: parts[3] || '' }] }];
    ex.dirty = true;
  } else if (parts[0] && ex.views.some((v) => v.id === parts[0])) loadView(parts[0]);
  else if (!ex.panels.length) { const v = ex.views.find((x) => x.id === ex.viewId); if (v) loadView(v.id); else { ex.panels = defaultPanels(); ex.viewId = ''; } }
  renderExBuilder(); renderExViews(); renderExPanels();
  clearInterval(ex.timer);
  ex.timer = setInterval(() => { if (!$('#view-explore').hidden && ex.auto && !ex.custom) refreshExData(); }, 30_000);
}
function loadView(id) {
  const v = ex.views.find((x) => x.id === id); if (!v) return;
  ex.viewId = id; store.set('exView', id);
  ex.panels = JSON.parse(JSON.stringify(v.panels));
  ex.range = EX_RANGES[v.range] ? v.range : '24h'; ex.custom = null; ex.dirty = false;
}
function exWindow() { if (ex.custom) return ex.custom; const to = Date.now(); return { from: to - EX_RANGES[ex.range], to }; }
function renderExViews() {
  $('#exView').innerHTML = `<option value="">${!ex.viewId && ex.dirty ? 'Unsaved charts' : 'Default overview'}</option>` + ex.views.map((v) => `<option value="${esc(v.id)}" ${v.id === ex.viewId ? 'selected' : ''}>${esc(v.name)}${v.id === ex.viewId && ex.dirty ? ' •' : ''}</option>`).join('');
  $('#exDelete').hidden = !ex.viewId;
  $$('#exRanges button').forEach((b) => b.classList.toggle('on', !ex.custom && b.dataset.r === ex.range));
  $('.ex-custom').classList.toggle('on', !!ex.custom);
  const { from, to } = exWindow();
  const loc = (t) => new Date(t - new Date(t).getTimezoneOffset() * 60e3).toISOString().slice(0, 16);
  $('#exFrom').value = loc(from); $('#exTo').value = loc(to);
}
function renderExBuilder() {
  const series = ex.catalog?.series || [];
  const hostsList = [...new Set(series.map((s) => s.host))].sort((a, b) => (a === 'apps') - (b === 'apps') || a.localeCompare(b));
  const hs = $('#exHost'), cur = hostsList.includes(hs.value) ? hs.value : hostsList.find((h) => h !== 'apps') || '';
  hs.innerHTML = hostsList.map((h) => `<option value="${esc(h)}" ${h === cur ? 'selected' : ''}>${h === 'apps' ? 'App health' : h === 'watches' ? 'Watches' : esc(h)}</option>`).join('');
  renderExMetrics();
  $('#exTarget').innerHTML = '<option value="new">a new chart</option>' + ex.panels.map((p, i) => `<option value="${i}">${esc(p.title || 'Chart ' + (i + 1))}</option>`).join('');
}
function renderExMetrics() {
  const h = $('#exHost').value, series = (ex.catalog?.series || []).filter((s) => s.host === h);
  const ms = [...new Set(series.map((s) => s.metric))];
  const cur = ms.includes($('#exMetric').value) ? $('#exMetric').value : ms[0];
  $('#exMetric').innerHTML = ms.map((m) => `<option value="${esc(m)}" ${m === cur ? 'selected' : ''}>${esc(ex.catalog.metrics[m]?.title || m)}</option>`).join('');
  renderExLabels();
}
function renderExLabels() {
  const h = $('#exHost').value, m = $('#exMetric').value;
  const labels = (ex.catalog?.series || []).filter((s) => s.host === h && s.metric === m).map((s) => s.label);
  const sel = $('#exLabel');
  sel.hidden = labels.length === 1 && labels[0] === '';
  sel.innerHTML = (labels.length > 1 ? `<option value="*">all ${labels.length}</option>` : '') + labels.map((l) => `<option value="${esc(l)}">${esc(h === 'apps' ? L.apps.find((a) => a.id === l)?.name || l : l || '(total)')}</option>`).join('');
}
function renderExPanels() {
  ex.charts.forEach((c) => c?.destroy()); ex.charts = [];
  const box = $('#exPanels');
  if (!ex.panels.length) { box.innerHTML = '<div class="ex-empty">No charts yet — pick a machine and metric above and press Add.<br>History starts when a series was first seen and is kept up to a year.</div>'; return; }
  box.innerHTML = ex.panels.map((p, i) => `<div class="ex-panel" data-p="${i}">
    <div class="ex-ph"><h3 contenteditable="plaintext-only" spellcheck="false" data-title="${i}">${esc(p.title || 'Chart')}</h3><span class="sp"></span>
      <button data-up="${i}" title="Move up">${ic('up')}</button><button data-edit="${i}" title="Remove a series">${ic('sliders')}</button><button data-del="${i}" title="Remove chart">${ic('x')}</button></div>
    <div class="ex-chart" id="exc-${i}"></div>
    <div class="ex-pf"><span id="exr-${i}"></span><a data-logs="${i}">${ic('logs')} Logs from this window</a></div></div>`).join('');
  ex.panels.forEach((_p, i) => {
    ex.charts[i] = new TimeChart($(`#exc-${i}`), {
      onHover: (t) => { ex.hoverT = t; ex.charts.forEach((c) => c?.draw()); },
      onZoom: (f, t) => { ex.custom = { from: f, to: t }; renderExViews(); refreshExData(); },
      onReset: () => { ex.custom = null; renderExViews(); refreshExData(); },
    });
  });
  refreshExData();
}
async function refreshExData() {
  if ($('#view-explore').hidden || !ex.panels.length) return;
  const { from, to } = exWindow();
  const points = Math.min(1200, Math.round(Math.max(320, ($('#exPanels').clientWidth || 1000) / (innerWidth > 1200 ? 2 : 1))));
  await Promise.all(ex.panels.map(async (p, i) => {
    const results = (await Promise.all(p.series.map((s) => api(`/api/metrics/query?${new URLSearchParams({ host: s.host, metric: s.metric, label: s.label || '', from, to, points })}`).catch(() => null)))).filter(Boolean);
    const ts = [...new Set(results.flatMap((r) => r.t))].sort((a, b) => a - b);
    const idx = new Map(ts.map((t, k) => [t, k]));
    const series = results.map((r, k) => {
      const a = new Array(ts.length).fill(null);
      r.t.forEach((t, j) => { a[idx.get(t)] = r.avg[j]; });
      // bridge holes from machines sampling at different moments; real outages stay as gaps
      const maxGap = Math.max(60_000, 2.5 * (r.bucketMs || 10_000));
      let last = -1;
      for (let j = 0; j < a.length; j++) if (a[j] != null) { if (last >= 0 && j - last > 1 && ts[j] - ts[last] <= maxGap) for (let q = last + 1; q < j; q++) a[q] = a[last]; last = j; }
      return { label: seriesName(r), color: EX_COLORS[k % EX_COLORS.length], vals: a, unit: r.unit };
    });
    const unit = results[0]?.unit || 'num';
    const res = results.find((r) => r.resolution)?.resolution;
    const el = $(`#exr-${i}`);
    if (el) el.textContent = res ? `${res === 'raw' ? '10-second' : res === 'm1' ? '1-minute' : 'hourly'} data${results.some((r) => r.unit !== unit) ? ' · mixed units' : ''}` : 'no data in this window yet';
    ex.charts[i]?.set({ t: ts, series }, from, to, unit);
  }));
  const st = await api('/api/metrics/stats').catch(() => null);
  if (st) $('#exFoot').textContent = `${st.series} series stored · raw 10 s kept 2 days, 1-minute for 30 days, hourly for 1 year`;
}
function bindExplore() {
  $('#exView').onchange = () => {
    const id = $('#exView').value;
    if (id) loadView(id); else { ex.viewId = ''; store.set('exView', ''); ex.panels = defaultPanels(); ex.dirty = false; }
    history.replaceState(null, '', '#explore' + (id ? '/' + id : ''));
    renderExViews(); renderExBuilder(); renderExPanels();
  };
  $('#exSave').onclick = async () => {
    const cur = ex.views.find((v) => v.id === ex.viewId);
    const name = await ask('Save this view', 'Name', cur?.name || 'My investigation'); if (!name) return;
    try {
      const v = await api('/api/views', { method: 'POST', body: { id: cur && cur.name === name ? cur.id : undefined, name, panels: ex.panels, range: ex.custom ? '24h' : ex.range } });
      ex.views = await api('/api/views'); ex.viewId = v.id; ex.dirty = false; store.set('exView', v.id);
      history.replaceState(null, '', '#explore/' + v.id); renderExViews(); toast(`Saved “${v.name}”`);
    } catch (e) { toast(e.message, true); }
  };
  $('#exDelete').onclick = async () => {
    const v = ex.views.find((x) => x.id === ex.viewId); if (!v || !(await H.confirmBox(`Delete the view “${v.name}”?`, 'Only the saved view goes; the data stays.', 'Delete'))) return;
    try { await api('/api/views/' + v.id, { method: 'DELETE' }); ex.views = ex.views.filter((x) => x.id !== v.id); ex.viewId = ''; store.set('exView', ''); ex.panels = defaultPanels(); renderExViews(); renderExBuilder(); renderExPanels(); } catch (e) { toast(e.message, true); }
  };
  $('#exRanges').addEventListener('click', (e) => { const b = e.target.closest('[data-r]'); if (!b) return; ex.range = b.dataset.r; ex.custom = null; renderExViews(); refreshExData(); });
  for (const id of ['#exFrom', '#exTo']) $(id).addEventListener('change', () => { const f = Date.parse($('#exFrom').value), t = Date.parse($('#exTo').value); if (f && t && t > f) { ex.custom = { from: f, to: t }; renderExViews(); refreshExData(); } });
  $('#exAuto').onclick = () => { ex.auto = !ex.auto; $('#exAuto').classList.toggle('on', ex.auto); };
  $('#exHost').onchange = renderExMetrics;
  $('#exMetric').onchange = renderExLabels;
  $('#exAdd').onclick = () => {
    const h = $('#exHost').value, m = $('#exMetric').value, l = $('#exLabel').hidden ? '' : $('#exLabel').value;
    if (!h || !m) return;
    const labels = l === '*' ? ex.catalog.series.filter((s) => s.host === h && s.metric === m).map((s) => s.label) : [l];
    const add = labels.map((label) => ({ host: h, metric: m, label }));
    const target = $('#exTarget').value;
    if (target === 'new') ex.panels.unshift({ title: add.length > 1 ? `${h} · ${ex.catalog.metrics[m]?.title || m}` : seriesName(add[0]), series: add });
    else ex.panels[+target].series.push(...add.filter((a) => !ex.panels[+target].series.some((s) => s.host === a.host && s.metric === a.metric && s.label === a.label)));
    ex.dirty = true; renderExViews(); renderExBuilder(); renderExPanels();
  };
  $('#exPanels').addEventListener('input', (e) => { const t = e.target.closest('[data-title]'); if (t) { ex.panels[+t.dataset.title].title = t.textContent.trim(); ex.dirty = true; } });
  $('#exPanels').addEventListener('click', (e) => {
    const b = e.target.closest('button,a'); if (!b) return;
    const d = b.dataset;
    if (d.del != null) { ex.panels.splice(+d.del, 1); ex.dirty = true; renderExViews(); renderExBuilder(); renderExPanels(); }
    if (d.up != null && +d.up > 0) { const [p] = ex.panels.splice(+d.up, 1); ex.panels.splice(+d.up - 1, 0, p); ex.dirty = true; renderExPanels(); }
    if (d.edit != null) { const p = ex.panels[+d.edit]; H.showMenu(b, p.series.map((s, j) => ['x', seriesName(s), () => { p.series.splice(j, 1); ex.dirty = true; if (!p.series.length) ex.panels.splice(+d.edit, 1); renderExViews(); renderExPanels(); }])); }
    if (d.logs != null) {
      const p = ex.panels[+d.logs]; const { from, to } = exWindow();
      const host = [...new Set(p.series.map((s) => s.host).filter((h) => h !== 'apps' && h !== 'watches'))];
      location.hash = `logs/~/${encodeURIComponent(host.length === 1 ? host[0] : '*')}/${Math.round(from)}/${Math.round(to)}`;
    }
  });
  let rt; addEventListener('resize', () => { clearTimeout(rt); rt = setTimeout(() => { if (!$('#view-explore').hidden) ex.charts.forEach((c) => c?.draw()); }, 150); });
}

// ---------------------------------------------------------------- WATCH
let watchList = [], wTimer = null;
async function loadWatches() {
  try { watchList = await api('/api/watches'); } catch (e) { toast(e.message, true); }
  renderWatches();
  clearInterval(wTimer);
  wTimer = setInterval(() => { if (!$('#view-watch').hidden) api('/api/watches').then((w) => { watchList = w; renderWatches(); }).catch(() => {}); }, 10_000);
}
function renderWatches() {
  const host = (u) => { try { return new URL(u).hostname.replace(/^www\./, ''); } catch { return u; } };
  $('#watches').innerHTML = watchList.map((w) => {
    const l = w.last, ok = l?.status === 'ok';
    const st = w.running ? ['wait', 'checking…'] : !l ? ['wait', 'not checked yet'] : !ok ? ['bad', l.status] : w.kind === 'change' ? ['out', 'checked'] : l.inStock === true ? ['in', 'In stock'] : l.inStock === false ? ['out', 'Out of stock'] : ['out', 'checked'];
    const lastOk = ok ? l : [...(w.history || [])].reverse().find((x) => x.status === 'ok');
    const hist = (w.history || []).slice(-30).map((x) => `<i class="${x.status === 'ok' ? `ok ${x.inStock === false ? 'out' : 'in'}` : x.status === 'skipped' ? 'skip' : 'fail'}" style="height:${x.status === 'ok' ? 100 : 45}%" title="${esc(new Date(x.at).toLocaleString())} — ${esc(x.status)}${x.price != null ? ' $' + x.price : ''}: ${esc(x.summary || '')}"></i>`).join('');
    const tr = w.trend;
    return `<div class="watch ${w.enabled ? '' : 'off'}">
      <div class="w-top"><b>${esc(w.name)}</b><span class="sp"></span><span class="w-state ${st[0]}">${esc(st[1])}</span></div>
      ${lastOk?.price != null ? `<div class="w-price">$${lastOk.price.toFixed(2)}${w.targetPrice != null ? `<small>target $${(+w.targetPrice).toFixed(2)}</small>` : ''}</div>` : ''}
      ${tr ? `<div class="w-trend">${tr.samples > 1 ? `30d <b>$${tr.low30.toFixed(2)}–$${tr.high30.toFixed(2)}</b> · 1y low <b>$${tr.low365.toFixed(2)}</b>${tr.change7d != null ? ` · 7d <b class="${tr.change7d < 0 ? 'down' : tr.change7d > 0 ? 'up' : ''}">${tr.change7d > 0 ? '+' : ''}${tr.change7d}%</b>` : ''}` : `tracking since ${new Date(tr.first).toLocaleDateString([], { month: 'short', day: 'numeric' })}`}</div>` : ''}
      ${l?.summary ? `<p class="w-sum">${esc(l.summary)}</p>` : ''}
      ${hist ? `<div class="w-hist">${hist}</div>` : ''}
      <div class="w-meta"><a href="${esc(w.url)}" target="_blank" rel="noopener">${esc(host(w.url))}</a>${w.notify === false ? `<span class="chip">${ic('belloff')} tracking only</span>` : ''}<span>every ${w.everyMin >= 60 ? w.everyMin / 60 + ' h' : w.everyMin + ' min'}</span>${l ? `<span>checked ${msAgo(l.at)}</span>` : ''}${w.createdBy === 'openclaw' ? '<span>added by OpenClaw</span>' : ''}</div>
      <div class="stack-actions">
        <button data-wcheck="${w.id}" ${w.running ? 'disabled class="busy"' : ''}>${ic('refresh')}Check now</button>
        <button data-wtoggle="${w.id}">${ic(w.enabled ? 'pause' : 'play')}${w.enabled ? 'Pause' : 'Resume'}</button>
        <button data-wbell="${w.id}">${ic(w.notify === false ? 'bell' : 'belloff')}${w.notify === false ? 'Alerts on' : 'Alerts off'}</button>
        <button data-wedit="${w.id}">${ic('edit')}Edit</button>
        ${lastOk?.price != null ? `<button data-whist="${w.id}">${ic('chart')}Price history</button>` : ''}
        <button data-wdel="${w.id}" title="Delete">${ic('trash')}</button>
      </div></div>`;
  }).join('') || '<p class="empty" style="grid-column:1/-1">Nothing watched yet. Add a page — or ask OpenClaw: “watch this product for a restock”.</p>';
}
function watchForm(w) {
  const { box, close } = H.modal(`<h3>${ic('eye')}${w ? 'Edit watch' : 'New watch'}</h3>
    <form class="form" id="wForm">
      <label>Name<input name="name" required value="${esc(w?.name || '')}" placeholder="IronWolf Pro 20TB @ Micro Center"></label>
      <label>Page<input name="url" required type="url" value="${esc(w?.url || '')}" placeholder="https://…"></label>
      <div class="row2"><label>Look for<select name="kind"><option value="restock">Restock + price</option><option value="price">Price only</option><option value="change">Any important change</option></select></label>
      <label>Check<select name="everyMin"><option value="15">every 15 min</option><option value="30">every 30 min</option><option value="60">every hour</option><option value="180">every 3 h</option><option value="360">every 6 h</option><option value="720">every 12 h</option><option value="1440">daily</option></select></label></div>
      <label><span>Target price <span class="hint">optional, $</span></span><input name="targetPrice" inputmode="decimal" value="${w?.targetPrice ?? ''}"></label>
      <label><span>Question <span class="hint">optional — what should OpenClaw look for?</span></span><textarea name="question" rows="2">${esc(w?.question || '')}</textarea></label>
      <label class="chk"><input type="checkbox" name="notify" ${!w || w.notify !== false ? 'checked' : ''}> Message me on Signal (off = just track the price)</label>
      <p class="err" id="wErr"></p>
      <div class="modal-actions"><button type="button" class="ghost-btn" data-x>Cancel</button><button class="primary-btn">${w ? 'Save changes' : 'Save watch'}</button></div>
    </form>`);
  const f = box.querySelector('#wForm');
  f.kind.value = w?.kind || 'restock'; f.everyMin.value = String(w?.everyMin || 60);
  box.querySelector('[data-x]').onclick = close;
  f.onsubmit = async (e) => {
    e.preventDefault();
    const tp = f.targetPrice.value.trim();
    const body = { name: f.name.value.trim(), url: f.url.value.trim(), kind: f.kind.value, targetPrice: tp === '' ? null : Number(tp.replace(/[$,]/g, '')), everyMin: Number(f.everyMin.value), question: f.question.value.trim(), notify: f.notify.checked };
    try {
      if (w) await api('/api/watches/' + w.id, { method: 'PATCH', body }); else await api('/api/watches', { method: 'POST', body });
      close(); toast(w ? 'Watch updated' : 'Watching — the first check runs within a few minutes'); loadWatches();
    } catch (er) { box.querySelector('#wErr').textContent = er.message; }
  };
  f.name.focus();
}
function bindWatch() {
  $('#wNewBtn').onclick = () => watchForm(null);
  $('#watches').addEventListener('click', async (e) => {
    const b = e.target.closest('button'); if (!b) return;
    const d = b.dataset, w = watchList.find((x) => x.id === (d.wcheck || d.wtoggle || d.wedit || d.wdel || d.whist || d.wbell)); if (!w) return;
    if (d.whist) { location.hash = `explore/q/watches/watch_price/${w.id}`; return; }
    if (d.wedit) return watchForm(w);
    try {
      if (d.wcheck) { await api(`/api/watches/${w.id}/check`, { method: 'POST' }); toast('Queued — OpenClaw checks it next (a minute or two)'); }
      if (d.wtoggle) await api('/api/watches/' + w.id, { method: 'PATCH', body: { enabled: !w.enabled } });
      if (d.wbell) { await api('/api/watches/' + w.id, { method: 'PATCH', body: { notify: w.notify === false } }); toast(w.notify === false ? 'Alerts on' : 'Alerts off — still tracking the price'); }
      if (d.wdel) { if (!(await H.confirmBox(`Stop watching “${w.name}”?`, 'Its history is kept in Explore.', 'Delete'))) return; await api('/api/watches/' + w.id, { method: 'DELETE' }); }
      loadWatches();
    } catch (er) { toast(er.message, true); }
  });
}

// ---------------------------------------------------------------- APPS (install queue, compose apps, catalog)
const proj = { list: [], out: {}, busy: {}, at: 0 };
function renderApps() {
  const sub = L.appsSub || 'installed';
  $$('#appsTabs button').forEach((b) => b.classList.toggle('on', b.dataset.sub === sub));
  $('#sub-installed').hidden = sub !== 'installed'; $('#installedTools').hidden = sub !== 'installed';
  $('#sub-add').hidden = sub !== 'add'; $('#addTools').hidden = sub !== 'add';
  const dockerHosts = L.hosts.filter((h) => h.docker && h.online).map((h) => h.name);
  const sel = $('#targetHost'), cur = sel.value || S.labHost;
  sel.innerHTML = dockerHosts.map((n) => `<option ${n === cur ? 'selected' : ''}>${esc(n)}</option>`).join('');
  const ph = $('#projHost'), pcur = ph.value;
  ph.innerHTML = '<option value="">All machines</option>' + dockerHosts.map((n) => `<option ${n === pcur ? 'selected' : ''}>${esc(n)}</option>`).join('');
  const pending = L.queue.filter((s) => s.status === 'pending');
  const shown = [...pending, ...(sub === 'add' ? L.queue.filter((s) => s.status !== 'pending').slice(0, 8) : [])];
  $('#queueHead').hidden = !shown.length;
  $('#queueMeta').textContent = pending.length ? `${pending.length} waiting for approval` : 'recent requests';
  $('#queueHead h2').textContent = pending.length ? 'Waiting for you' : 'Recent requests';
  const verb = (s) => (s.action === 'op' ? `${s.op} app` : s.action === 'edit' ? 'change compose file' : s.action === 'remove' ? 'remove' : 'install');
  const approve = (s) => (s.action === 'op' ? `Approve ${s.op}` : s.action === 'edit' ? 'Approve & apply' : s.action === 'remove' ? 'Approve removal' : 'Approve & install');
  $('#queue').innerHTML = shown.map((s) => `<div class="q-item ${s.status}">
    <div class="q-top"><b>${esc(s.name)}</b><span class="chip">${esc(s.host)}</span><span class="chip">${esc(verb(s))}</span><span class="q-status ${s.status}">${esc(s.status)}</span><span class="muted small">by ${esc(s.requestedBy)} · ${msAgo(s.createdAt)}</span>
      ${s.status === 'pending' ? `<div class="q-actions"><button class="danger-btn" data-reject="${s.id}">Reject</button><button class="primary-btn" data-approve="${s.id}">${approve(s)}</button></div>` : ''}
      ${s.status === 'done' && s.app?.href ? `<div class="q-actions"><a class="primary-btn" href="${esc(s.app.href)}" target="_blank" rel="noopener">Open</a></div>` : ''}</div>
    ${s.notes ? `<p>${esc(s.notes)}</p>` : ''}
    ${s.action === 'edit' && s.status === 'pending' ? `<div data-diff="${s.id}"><button class="ghost-btn" data-showdiff="${s.id}">${ic('file')} Show changes</button></div>` : ''}
    ${s.compose && s.action !== 'edit' ? `<details ${s.status === 'pending' ? 'open' : ''}><summary>docker-compose.yml</summary><pre class="json">${esc(s.compose)}</pre></details>` : ''}
    ${s.log ? `<details><summary>Output</summary><pre class="json">${esc(s.log)}</pre></details>` : ''}</div>`).join('');
  if (sub === 'installed') loadProjects();
}
async function loadProjects(force) {
  if (!force && Date.now() - proj.at < 8000) return renderProjects();
  proj.at = Date.now();
  try { proj.list = await api('/api/projects'); } catch { /* keep the last list */ }
  if (L.demoOn) proj.list = proj.list.map((g) => ({ ...g, projects: g.projects.filter((p) => !sensitiveName(p.name) && !p.services.some((sv) => sensitiveName(sv.container) || sensitiveName(sv.image))) }));
  renderProjects();
}
function renderProjects() {
  const q = $('#projQ').value.toLowerCase(), hostF = $('#projHost').value, showStopped = $('#projStopped').checked;
  const order = orderedHosts().map((h) => h.name);
  const groups = proj.list.filter((g) => !hostF || g.host === hostF).sort((a, b) => order.indexOf(a.host) - order.indexOf(b.host));
  const html = groups.map((g) => {
    const list = g.projects.filter((p) => (showStopped || p.running) && (!q || p.name.includes(q) || p.services.some((s) => (s.container || '').toLowerCase().includes(q))));
    if (!list.length) return '';
    return `<section class="proj-host"><h2>${esc(g.host)} <span class="muted">${g.projects.filter((p) => p.running).length} running · ${g.projects.length} apps${g.online ? '' : ' · machine offline'}</span></h2><div class="stacks">${list.map((p) => projCard(g.host, p)).join('')}</div></section>`;
  }).join('');
  $('#projects').innerHTML = html || `<p class="empty">${proj.list.length ? 'No apps match.' : 'Waiting for machines to report their apps…'}</p>`;
}
function projCard(host, p) {
  const key = `${host}/${p.name}`, total = p.services.length, busy = proj.busy[key], out = proj.out[key];
  const st = !total ? 'stopped' : p.running === total ? 'ok' : p.running ? 'warn' : 'stopped';
  const label = !total ? 'not created' : p.running === total ? `running ${p.running}/${total}` : p.running ? `partly running ${p.running}/${total}` : `stopped 0/${total}`;
  const btn = (op, icon, text, dis) => `<button data-op="${op}" data-key="${esc(key)}" ${dis || busy ? 'disabled' : ''} class="${busy === op ? 'busy' : ''}">${ic(busy === op ? 'refresh' : icon)}${text}</button>`;
  return `<div class="stack ${st === 'stopped' ? 'stopped' : ''}">
    <div class="stack-top"><span class="dot dot-${st === 'stopped' ? '' : st}"></span><b>${esc(p.name)}</b><span class="chip">${esc(label)}</span>${p.known ? `<span class="chip warn" title="${esc(p.known.note || '')}">known issue</span>` : ''}${p.protected ? '<span class="chip warn" title="Managed by Pterodactyl — stop/update from the panel">protected</span>' : ''}</div>
    ${p.known ? `<div class="known-note">${ic('note')} ${esc(p.known.note || 'Marked as a known issue')} <span class="muted">· since ${new Date(p.known.since).toLocaleDateString([], { month: 'short', day: 'numeric' })}</span></div>` : ''}
    <div class="stack-path" title="${esc((p.configFiles || []).join(', '))}">${esc((p.configFiles || [])[0] || p.workingDir || '')}</div>
    ${total ? `<div class="svc">${p.services.map((s) => `<span data-logs="${esc(host)}/container/${esc(s.container)}" title="${esc(s.image)} — ${esc(s.status)} · click for logs"><i class="dot dot-${s.state === 'running' ? (/unhealthy/.test(s.status) ? 'warn' : 'ok') : s.state === 'restarting' ? 'crit' : ''}"></i>${esc(s.service || s.container)}</span>`).join('')}</div>` : ''}
    <div class="stack-actions">
      ${p.running < total || !total ? btn('start', 'play', 'Start') : btn('stop', 'stop', 'Stop', p.protected)}
      ${btn('restart', 'restart', 'Restart', !p.running)}
      ${btn('update', 'download', 'Update', p.protected)}
      ${btn('edit', 'edit', 'Edit')}
      ${p.known ? `<button data-unknown="project:${esc(host)}/${esc(p.name)}">${ic('check')}Clear issue</button>` : `<button data-known="${esc(key)}">${ic('note')}Known issue…</button>`}
      ${total ? `<button data-hist="${esc(host)}/${esc(p.services[0].container)}">${ic('chart')}History</button>` : ''}
    </div>
    ${out ? `<pre class="stack-out ${out.ok ? '' : 'fail'}">${esc(out.text)}</pre>` : ''}</div>`;
}
async function openProjectEditor(host, name) {
  let f;
  try { f = await api(`/api/projects/${encodeURIComponent(host)}/${encodeURIComponent(name)}/file`); } catch (e) { return toast(e.message, true); }
  const orig = f.content;
  const { box, close } = H.modal(`<h3>${ic('file')}${esc(name)} on ${esc(host)}</h3><small class="muted">${esc(f.path)}${f.files?.length > 1 ? ` (+${f.files.length - 1} more files, editing the first)` : ''}</small>
    <div class="seg seg-sm" id="peTabs" style="justify-self:start"><button data-ed="edit" class="on">Edit</button><button data-ed="diff">Changes</button></div>
    <textarea class="code" id="peText" spellcheck="false" wrap="off"></textarea><div class="diff" id="peDiff" hidden></div><p class="err" id="peErr"></p>
    <div class="modal-actions"><span class="note">A backup is kept next to the file. Invalid compose is rolled back automatically.</span><button class="ghost-btn" data-x>Cancel</button><button class="ghost-btn" data-save="0">Save</button><button class="primary-btn" data-save="1">Save &amp; apply</button></div>`, { wide: true });
  const ta = box.querySelector('#peText'); ta.value = orig;
  ta.addEventListener('keydown', (e) => { if (e.key === 'Tab') { e.preventDefault(); ta.setRangeText('  ', ta.selectionStart, ta.selectionEnd, 'end'); } });
  box.querySelector('[data-x]').onclick = close;
  box.querySelector('#peTabs').onclick = (e) => {
    const b = e.target.closest('button'); if (!b) return;
    $$('#peTabs button', box).forEach((x) => x.classList.toggle('on', x === b));
    const diff = b.dataset.ed === 'diff'; ta.hidden = diff; box.querySelector('#peDiff').hidden = !diff;
    if (diff) box.querySelector('#peDiff').innerHTML = H.lineDiff(orig, ta.value).map((o) => `<div class="${o.t}">${o.t === 'add' ? '+ ' : o.t === 'del' ? '- ' : '  '}${esc(o.s)}</div>`).join('');
  };
  for (const sb of $$('[data-save]', box)) sb.onclick = async () => {
    const apply = sb.dataset.save === '1';
    $$('button', box).forEach((x) => (x.disabled = true));
    try {
      const j = await api(`/api/projects/${encodeURIComponent(host)}/${encodeURIComponent(name)}/file`, { method: 'PUT', body: { content: ta.value, apply } });
      if (j.status !== 'done') throw new Error(j.log || `save ${j.status}`);
      proj.out[`${host}/${name}`] = { ok: true, text: j.log };
      close(); toast(apply ? 'Saved and applied' : 'Saved'); loadProjects(true);
    } catch (er) { box.querySelector('#peErr').textContent = er.message; $$('button', box).forEach((x) => (x.disabled = false)); }
  };
  ta.focus();
}
let catalogItems = [], catalogTimer;
async function loadCatalog() {
  try { catalogItems = await api('/api/catalog?q=' + encodeURIComponent($('#catalogQ').value)); renderCatalog(); }
  catch (e) { $('#catalog').innerHTML = `<p class="empty">Catalog unavailable: ${esc(e.message)}</p>`; }
}
function renderCatalog() {
  $('#catalog').innerHTML = catalogItems.map((c, i) => `<div class="cat">${c.logo ? `<span class="app-icon light"><img src="/api/icon?src=${encodeURIComponent(c.logo)}" alt="" loading="lazy" data-fallback="${esc(c.name)}"></span>` : iconHtml('', c.name)}
    <div><h4>${esc(c.name)}</h4><p>${esc(c.description)}</p><div class="cat-foot">${c.category ? `<span class="chip">${esc(c.category.split(',')[0])}</span>` : ''}<span class="muted small">★ ${c.stars}</span><button data-cat="${i}">${ic('sparkle')}Set up</button></div></div></div>`).join('') || '<p class="empty">No matches.</p>';
}
function bindApps() {
  $('#appsTabs').addEventListener('click', (e) => { const b = e.target.closest('[data-sub]'); if (b) location.hash = b.dataset.sub === 'add' ? 'apps/add' : 'apps'; });
  $('#queue').addEventListener('click', async (e) => {
    const d = e.target.closest('[data-showdiff]');
    if (d) {
      const s = L.queue.find((x) => x.id === d.dataset.showdiff);
      try { const cur = await api(`/api/projects/${encodeURIComponent(s.host)}/${encodeURIComponent(s.name)}/file`); d.parentElement.innerHTML = `<div class="diff" style="min-height:0">${H.lineDiff(cur.content, s.compose).map((o) => `<div class="${o.t}">${o.t === 'add' ? '+ ' : o.t === 'del' ? '- ' : '  '}${esc(o.s)}</div>`).join('')}</div>`; }
      catch (er) { toast(er.message, true); }
      return;
    }
    const b = e.target.closest('[data-approve],[data-reject]'); if (!b) return;
    const id = b.dataset.approve || b.dataset.reject;
    try { await api(`/api/stacks/${id}/${b.dataset.approve ? 'approve' : 'reject'}`, { method: 'POST' }); toast(b.dataset.approve ? 'Approved — running it now' : 'Rejected'); refresh(); setTimeout(refresh, 4000); }
    catch (er) { toast(er.message, true); }
  });
  $('#projQ').addEventListener('input', renderProjects);
  $('#projHost').onchange = renderProjects;
  $('#projStopped').onchange = renderProjects;
  $('#projects').addEventListener('click', async (e) => {
    const lg = e.target.closest('[data-logs]'); if (lg) { location.hash = 'logs/' + lg.dataset.logs.split('/').map(encodeURIComponent).join('/'); return; }
    const hi = e.target.closest('[data-hist]'); if (hi) { const [h, c] = hi.dataset.hist.split('/'); location.hash = `explore/q/${encodeURIComponent(h)}/ctr_mem/${encodeURIComponent(c)}`; return; }
    const kn = e.target.closest('[data-known]'); if (kn) { const [h, n] = kn.dataset.known.split('/'); return markKnown({ project: { host: h, name: n } }, `${n} on ${h}`); }
    const uk = e.target.closest('[data-unknown]'); if (uk) return clearKnown(uk.dataset.unknown);
    const b = e.target.closest('[data-op]'); if (!b) return;
    const key = b.dataset.key, op = b.dataset.op, [host, name] = key.split('/');
    if (op === 'edit') return openProjectEditor(host, name);
    if (op === 'stop' && !(await H.confirmBox(`Stop ${name} on ${host}?`, 'Its containers stay, so Start brings it back.', 'Stop'))) return;
    proj.busy[key] = op; delete proj.out[key]; renderProjects();
    try {
      let j = await api(`/api/projects/${encodeURIComponent(host)}/${encodeURIComponent(name)}/${op}`, { method: 'POST' });
      while (j && (j.status === 'queued' || j.status === 'running')) { await new Promise((r) => setTimeout(r, 2000)); j = await api('/api/jobs/' + j.id); }
      proj.out[key] = { ok: j?.status === 'done', text: j?.log || j?.status || 'no answer' };
      toast(j?.status === 'done' ? `${name}: ${op} done` : `${name}: ${op} failed`, j?.status !== 'done');
    } catch (er) { proj.out[key] = { ok: false, text: er.message }; }
    delete proj.busy[key];
    loadProjects(true);
  });
  $('.activity').addEventListener('toggle', async (e) => {
    if (!e.target.open) return;
    try {
      const rows = await api('/api/activity');
      $('#activity').innerHTML = rows.map((r) => `<div class="act-row"><span>${new Date(r.at).toLocaleString([], { month: 'short', day: 'numeric', hour: '2-digit', minute: '2-digit' })}</span><span>${esc(r.host || '')}</span><b>${esc(r.what)}</b><span style="color:var(${r.ok ? '--good' : '--crit'})">${r.ok ? 'ok' : 'failed'} · ${esc(r.by)}</span></div>`).join('') || '<p class="muted">Nothing yet.</p>';
    } catch (er) { toast(er.message, true); }
  });
  $('#catalogQ').addEventListener('input', () => { clearTimeout(catalogTimer); catalogTimer = setTimeout(loadCatalog, 250); });
  $('#catalog').addEventListener('click', (e) => {
    const b = e.target.closest('[data-cat]'); if (!b) return;
    const c = catalogItems[+b.dataset.cat], host = $('#targetHost').value;
    const cfg = c.config ? `\nKnown config from linuxserver.io:\n- ports: ${c.config.ports.map((p) => `${p.internal} (${p.desc || ''})`).join('; ') || 'none'}\n- volumes: ${c.config.volumes.map((v) => `${v.path} (${v.desc || ''})`).join('; ') || 'none'}\n- env: ${c.config.env.map((v) => `${v.name}=${v.value}`).join('; ') || 'none'}` : '';
    openClaw(`Set up ${c.name} (${c.image}) on ${host}.${cfg}\nPick a host port that is free on ${host}, put data under ~/omegaenv-data/${c.name}, use PUID/PGID 1000 and TZ America/New_York, then submit it to the install queue with an app tile so I can approve it.`);
  });
}

// ---------------------------------------------------------------- LOGS (every machine)
const logs = { sel: null, win: null, level: 'all', q: '', hours: 6, live: true, lines: [], lastTs: 0, sources: [], collapsed: new Set(store.get('logCollapsed', [])), loading: false, timer: null, truncated: false };
async function openLogs(parts) {
  if (parts[0] === '~') { logs.win = { host: parts[1] && parts[1] !== '*' ? parts[1] : '', from: Number(parts[2]), to: Number(parts[3]) }; parts = []; }
  const [host, kind, source] = parts;
  const sel = host && kind && source ? { host, kind, source } : null;
  if (sel) logs.win = null;
  const changed = JSON.stringify(sel) !== JSON.stringify(logs.sel);
  logs.sel = sel;
  $('#llHours').hidden = !!sel || !!logs.win;
  if (changed || !logs.lines.length || logs.win) await loadLogLines();
  loadLogSources();
  clearInterval(logs.timer);
  logs.timer = setInterval(pollLogs, 2500);
}
async function loadLogSources() {
  try {
    const r = await api('/api/logs/sources');
    logs.sources = L.demoOn ? r.sources.filter((s) => !sensitiveName(s.source)) : r.sources;
    $('#llStats').textContent = `${r.sources.length} sources · ${r.stats.bytes ? bytes(r.stats.bytes) + ' stored · ' : ''}kept ${r.stats.retainDays} days`;
    renderLogSources();
  } catch (e) { toast(e.message, true); }
}
function renderLogSources() {
  const f = $('#llSrcQ').value.toLowerCase(), order = orderedHosts().map((h) => h.name), byHost = new Map();
  for (const s of logs.sources) { if (f && !(s.source.toLowerCase().includes(f) || s.host.includes(f))) continue; if (!byHost.has(s.host)) byHost.set(s.host, []); byHost.get(s.host).push(s); }
  const hosts = [...byHost.keys()].sort((a, b) => (order.indexOf(a) + 1 || 99) - (order.indexOf(b) + 1 || 99));
  const sel = logs.sel;
  $('#llAll').classList.toggle('sel', !sel);
  $('#llSources').innerHTML = hosts.map((h) => {
    const list = byHost.get(h).sort((a, b) => b.error - a.error || (a.kind === b.kind ? a.source.localeCompare(b.source) : a.kind === 'container' ? -1 : 1));
    const errs = list.reduce((n, s) => n + s.error, 0), open = f || !logs.collapsed.has(h);
    return `<div class="log-group clickable" data-collapse="${esc(h)}"><span>${ic(open ? 'chevron' : 'chevronr')} ${esc(h)}</span><span>${errs ? `<span style="color:var(--crit)">${errs} err</span> · ` : ''}${list.length}</span></div>` +
      (open ? list.map((s) => {
        const fresh = Date.now() - s.lastTs < 10 * 60_000, isSel = sel && sel.host === s.host && sel.kind === s.kind && sel.source === s.source;
        return `<button class="log-src ${isSel ? 'sel' : ''}" data-src="${esc(s.host)}/${esc(s.kind)}/${esc(s.source)}" title="${esc(s.kind)} · ${s.lines} lines in the last hour"><span class="dot dot-${s.error ? 'crit' : s.warn ? 'warn' : fresh ? 'ok' : ''}"></span><span>${ic(s.kind === 'container' ? 'box' : 'cpu')} ${esc(s.source)}</span><small class="${s.error ? 'err' : s.warn ? 'warn' : ''}">${s.error ? s.error + ' err' : s.warn ? s.warn + ' warn' : s.lines || ''}</small></button>`;
      }).join('') : '');
  }).join('') || '<p class="muted" style="padding:10px">No logs yet — this server and its agents ship every 2 s.</p>';
}
function logParams(extra = {}) {
  const p = new URLSearchParams({ level: logs.level, q: logs.q, ...extra });
  if (logs.sel) { p.set('host', logs.sel.host); p.set('kind', logs.sel.kind); p.set('source', logs.sel.source); p.set('limit', '1000'); }
  else if (logs.win) { if (logs.win.host) p.set('host', logs.win.host); p.set('from', logs.win.from); p.set('to', logs.win.to); p.set('limit', '1000'); }
  else { p.set('hours', logs.hours); p.set('limit', '400'); }
  return p;
}
async function loadLogLines() {
  logs.loading = true;
  $('#llTitle').innerHTML = logs.sel ? `${ic(logs.sel.kind === 'container' ? 'box' : 'cpu')}${esc(logs.sel.source)} <small class="muted">on ${esc(logs.sel.host)}</small>`
    : logs.win ? `${ic('globe')}${esc(logs.win.host || 'All machines')} <span class="ll-filter">${new Date(logs.win.from).toLocaleString([], { month: 'short', day: 'numeric', hour: '2-digit', minute: '2-digit' })} → ${new Date(logs.win.to).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' })}<button id="llWinClear" title="Clear time window">✕</button></span>`
    : `${ic('globe')}All machines <small class="muted">${logs.q || logs.level !== 'all' ? 'search' : 'newest lines'}</small>`;
  $('#llLines').innerHTML = '<div class="log-empty">Loading…</div>';
  try {
    if (logs.sel) logs.lines = await api('/api/logs?' + logParams());
    else { const r = await api('/api/logs/search?' + logParams()); logs.lines = r.results.reverse(); logs.truncated = r.truncated; }
    logs.lastTs = logs.lines.at(-1)?.ts || Date.now() - 5000;
    renderLogLines(true);
  } catch (e) { $('#llLines').innerHTML = `<div class="log-empty">${esc(e.message)}</div>`; }
  logs.loading = false;
}
let searchPollAt = 0;
async function pollLogs() {
  if ($('#view-labl').hidden || !logs.live || logs.loading) return;
  try {
    if (logs.sel) {
      const fresh = await api('/api/logs?' + logParams({ after: logs.lastTs }));
      if (fresh.length) { logs.lines.push(...fresh); logs.lastTs = fresh.at(-1).ts; if (logs.lines.length > 3000) logs.lines.splice(0, logs.lines.length - 3000); renderLogLines(false); }
    } else if (!logs.win && Date.now() - searchPollAt > 15_000) {
      searchPollAt = Date.now();
      const r = await api('/api/logs/search?' + logParams());
      const fresh = r.results.reverse().filter((x) => x.ts > logs.lastTs);
      if (fresh.length) { logs.lines.push(...fresh); logs.lastTs = fresh.at(-1).ts; renderLogLines(false); }
    }
  } catch { /* next poll */ }
}
function renderLogLines(jump) {
  const box = $('#llLines'), atBottom = box.scrollHeight - box.scrollTop - box.clientHeight < 60;
  if (!logs.lines.length) { box.innerHTML = `<div class="log-empty">${logs.sel ? 'No lines match.' : 'Nothing matched in this time window.'}</div>`; return; }
  const q = logs.q ? esc(logs.q).replace(/[.*+?^${}()|[\]\\]/g, '\\$&') : null;
  const hl = (t) => { const s = esc(t); return q ? s.replace(new RegExp(q, 'gi'), (m) => `<mark>${m}</mark>`) : s; };
  const fmtDay = (ts) => new Date(ts).toLocaleDateString([], { weekday: 'short', month: 'short', day: 'numeric' });
  let lastDay = '';
  const rows = [];
  if (logs.truncated && !logs.sel) rows.push('<div class="ll day">Search stopped early to stay fast — narrow it with a machine, level or a shorter window.</div>');
  for (const l of logs.lines.slice(-2000)) {
    const d = fmtDay(l.ts);
    if (d !== lastDay) { rows.push(`<div class="ll day">${d}</div>`); lastDay = d; }
    rows.push(`<div class="ll lab ${l.level === 'error' ? 'error' : l.level === 'warn' ? 'warn' : ''}"><time title="${new Date(l.ts).toISOString()}">${new Date(l.ts).toLocaleTimeString([], { hour12: false })}</time>${l.host ? `<span class="src" data-src="${esc(l.host)}/${esc(l.kind)}/${esc(l.source)}">${esc(l.host)}/${esc(l.source)}</span>` : '<span></span>'}<span class="m">${hl(l.msg)}</span></div>`);
  }
  box.innerHTML = rows.join('');
  if (jump || (logs.live && atBottom)) box.scrollTop = box.scrollHeight;
}
function bindLogs() {
  $('#llSrcQ').addEventListener('input', renderLogSources);
  $('#llSources').addEventListener('click', (e) => {
    const c = e.target.closest('[data-collapse]');
    if (c) { const h = c.dataset.collapse; logs.collapsed.has(h) ? logs.collapsed.delete(h) : logs.collapsed.add(h); store.set('logCollapsed', [...logs.collapsed]); return renderLogSources(); }
    const b = e.target.closest('[data-src]'); if (b) location.hash = 'logs/' + b.dataset.src.split('/').map(encodeURIComponent).join('/');
  });
  $('#llAll').onclick = () => { logs.win = null; logs.sel = null; location.hash = 'logs'; loadLogLines(); };
  $('#llTitle').addEventListener('click', (e) => { if (e.target.id === 'llWinClear') { logs.win = null; $('#llHours').hidden = false; loadLogLines(); } });
  $('#llLines').addEventListener('click', (e) => { const s = e.target.closest('.src'); if (s) location.hash = 'logs/' + s.dataset.src.split('/').map(encodeURIComponent).join('/'); });
  $('#llLevel').addEventListener('click', (e) => { const b = e.target.closest('[data-lv]'); if (!b) return; logs.level = b.dataset.lv; H.segOn('#llLevel', b); loadLogLines(); });
  let qt; $('#llQ').addEventListener('input', () => { clearTimeout(qt); qt = setTimeout(() => { logs.q = $('#llQ').value.trim(); loadLogLines(); }, 350); });
  $('#llHours').onchange = () => { logs.hours = Number($('#llHours').value); loadLogLines(); };
  $('#llLive').onclick = () => { logs.live = !logs.live; $('#llLive').classList.toggle('on', logs.live); };
  $('#llAsk').onclick = () => {
    const errs = logs.lines.filter((l) => l.level !== 'info'), pick = (errs.length >= 5 ? errs : logs.lines).slice(-40);
    const where = logs.sel ? `${logs.sel.source} (${logs.sel.kind}) on ${logs.sel.host}` : 'my homelab';
    openClaw(`Here are recent log lines from ${where}. What is going wrong and what should I do? Use tail_logs/search_logs if you need more.\n\n${pick.map((l) => `${new Date(l.ts).toISOString()} ${l.host ? l.host + '/' + l.source + ' ' : ''}[${l.level}] ${l.msg.slice(0, 300)}`).join('\n')}`);
  };
}

// ---------------------------------------------------------------- OpenClaw
let chat = store.get('chat', []);
let thread = store.get('thread', '') || (() => { const t = 'oghost-' + Math.random().toString(36).slice(2, 10); store.set('thread', t); return t; })();
const SUGGEST = ['Why is my storage filling up?', 'What uses the most RAM right now?', 'Set up Uptime Kuma', 'Which apps are slow today?'];
let sending = false, chatAbort = null;
function openClaw(prefill, send = false) {
  if (L.demoOn) return toast('OpenClaw is off in demo mode');
  $('#claw').classList.add('open'); $('#claw').setAttribute('aria-hidden', 'false');
  renderChat();
  if (prefill) { $('#chatText').value = prefill; autosize(); }
  if (send && prefill) sendChat(); else $('#chatText').focus();
}
function closeClaw() { $('#claw').classList.remove('open'); $('#claw').setAttribute('aria-hidden', 'true'); }
function renderClawState() { const s = L.station; const el = $('#clawState'); if (el) el.textContent = s?.llama_server === false ? 'brain is off — station is in Gaming mode' : 'local agent on station'; }
function md(s) { return esc(s).replace(/```[\w-]*\n([\s\S]*?)```/g, (_, c) => `<pre class="json">${c}</pre>`).replace(/`([^`]+)`/g, '<code>$1</code>').replace(/\*\*([^*]+)\*\*/g, '<b>$1</b>'); }
const secs = (ms) => { const t = Math.max(0, Math.round(ms / 1000)); return t < 60 ? `${t}s` : `${Math.floor(t / 60)}m ${String(t % 60).padStart(2, '0')}s`; };
function progressHtml(m) {
  const p = m.progress; if (!p) return '';
  const steps = p.steps.map((st) => `<li class="step ${st.status}">${ic(st.status === 'running' ? 'refresh' : st.status === 'failed' ? 'x' : 'check', st.status === 'running' ? 'spin' : '')}<span>${esc(st.title)}</span>${st.note ? `<small>${esc(st.note)}</small>` : ''}</li>`).join('');
  const appr = p.approvals.map((a) => `<div class="approval" data-appr="${esc(a.id)}"><b>${ic('shield')} OpenClaw wants to run</b><pre>${esc(a.command)}</pre><div><button data-dec="allow-once">Allow once</button><button data-dec="allow-always" class="ghost">Always allow</button><button data-dec="deny" class="ghost">Deny</button></div></div>`).join('');
  if (!m.typing) return steps ? `<details class="steps-done"><summary>${p.steps.length} step${p.steps.length === 1 ? '' : 's'} · ${secs(p.endedAt - p.startedAt)}</summary><ul class="steps">${steps}</ul></details>` : '';
  const thinking = !m.content && p.thinking ? `<div class="thinking">${esc(p.thinking.slice(-220))}</div>` : '';
  return `<div class="progress"><div class="status-line">${ic('refresh', 'spin')}<span>${esc(p.label)}</span><small data-since="${p.startedAt}">${secs(Date.now() - p.startedAt)}</small></div>${steps ? `<ul class="steps">${steps}</ul>` : ''}${thinking}${appr}</div>`;
}
function renderChat() {
  renderClawState();
  $('#chat').innerHTML = chat.length ? chat.map((m) => `<div class="msg ${m.role}${m.typing && m.content ? ' typing' : ''}">${m.role === 'assistant' ? progressHtml(m) : ''}${md(m.content)}</div>`).join('')
    : '<div class="msg sys">OpenClaw runs on your own GPU and can see this lab’s machines and apps.<br>It can draft installs and automations; you approve anything that changes the lab.</div>';
  $('#chatSuggest').innerHTML = chat.length ? '' : SUGGEST.map((q) => `<button>${esc(q)}</button>`).join('');
  $('#chat').scrollTop = 1e9;
}
function onClawEvent(reply, ev) {
  const p = reply.progress;
  switch (ev.type) {
    case 'hello': if (!ev.gateway) p.label = 'Waiting for OpenClaw…'; break;
    case 'phase': p.label = ev.phase === 'memory-start' ? 'Checking memory…' : ev.phase === 'finishing' ? 'Wrapping up…' : 'Thinking…'; break;
    case 'thinking': p.thinking = ev.text; if (!p.steps.some((s) => s.status === 'running')) p.label = 'Thinking…'; break;
    case 'tool': {
      let st = p.steps.find((x) => x.id === ev.id);
      if (!st) p.steps.push((st = { id: ev.id, title: ev.title || ev.name, status: 'running' }));
      st.title = ev.title || st.title;
      st.status = ev.phase === 'end' ? (ev.status === 'failed' ? 'failed' : 'done') : 'running';
      p.label = st.status === 'running' ? `Running ${st.title}` : 'Thinking…'; p.thinking = '';
      break;
    }
    case 'output': { const st = p.steps.find((x) => x.id === ev.id), err = /"error":\s*"([^"]+)"/.exec(ev.text || ''); if (st && err) { st.note = err[1]; st.status = 'failed'; } break; }
    case 'writing': p.label = 'Writing reply…'; break;
    case 'approval': if (!p.approvals.some((a) => a.id === ev.id)) p.approvals.push(ev); p.label = 'Waiting for your approval'; break;
    case 'approval-resolved': p.approvals = p.approvals.filter((a) => a.id !== ev.id); break;
  }
}
async function readSSE(resp, onData) {
  const reader = resp.body.getReader(), dec = new TextDecoder(); let buf = '';
  for (;;) {
    const { value, done } = await reader.read(); if (done) break;
    buf += dec.decode(value, { stream: true });
    const parts = buf.split('\n\n'); buf = parts.pop();
    for (const part of parts) for (const line of part.split('\n')) if (line.startsWith('data:')) onData(line.slice(5).trim());
  }
}
async function followClaw(reply, signal) {
  try {
    const r = await fetch('/api/assistant/events?thread=' + encodeURIComponent(thread), { signal });
    if (!r.ok) return;
    await readSSE(r, (d) => { try { onClawEvent(reply, JSON.parse(d)); if (reply.typing) renderChat(); } catch { /* not JSON */ } });
  } catch { /* aborted when the reply ends */ }
}
function setSending(on) { sending = on; const b = $('#chatForm .send'); b.innerHTML = ic(on ? 'stop' : 'send'); b.title = on ? 'Stop' : 'Send'; b.classList.toggle('stop', on); }
async function stopChat() { chatAbort?.abort(); try { await api('/api/assistant/stop', { method: 'POST', body: { thread } }); } catch { /* best effort */ } }
async function sendChat() {
  const text = $('#chatText').value.trim();
  if (!text || sending) return;
  setSending(true);
  $('#chatText').value = ''; autosize();
  chat.push({ role: 'user', content: text });
  const reply = { role: 'assistant', content: '', typing: true, progress: { label: 'Sending…', steps: [], approvals: [], thinking: '', startedAt: Date.now() } };
  chat.push(reply); renderChat();
  const ac = (chatAbort = new AbortController()), follow = new AbortController();
  followClaw(reply, follow.signal);
  try {
    const r = await fetch('/api/assistant/chat', { method: 'POST', headers: { 'Content-Type': 'application/json', 'X-OG-UI': '1' }, signal: ac.signal, body: JSON.stringify({ thread, messages: chat.filter((m) => m !== reply && !m.error).map(({ role, content }) => ({ role, content })) }) });
    if (!r.ok) throw new Error((await r.json().catch(() => ({}))).error || r.statusText);
    await readSSE(r, (d) => { if (d === '[DONE]') return; try { reply.content += JSON.parse(d).choices?.[0]?.delta?.content || ''; renderChat(); } catch { /* keepalive */ } });
    if (!reply.content) reply.content = '(no reply)';
  } catch (e) {
    reply.content = e.name === 'AbortError' ? (reply.content ? reply.content + '\n\n' : '') + '(stopped)' : `Couldn't reach OpenClaw: ${e.message}${L.station?.llama_server === false ? '\nstation is in Gaming mode, so its model is unloaded. Switch to AI mode on Home.' : ''}`;
    reply.error = true;
  }
  follow.abort(); chatAbort = null;
  reply.typing = false; reply.progress.endedAt = Date.now(); reply.progress.approvals = [];
  setSending(false);
  store.set('chat', chat.slice(-40)); renderChat(); refresh();
}
function autosize() { const t = $('#chatText'); t.style.height = 'auto'; t.style.height = Math.min(160, t.scrollHeight) + 'px'; }
function bindClaw() {
  H.hydrateIcons($('#claw'));
  $('#clawBtn').onclick = () => openClaw();
  $('#clawClose').onclick = closeClaw;
  $('#clawNew').onclick = () => { chat = []; thread = 'oghost-' + Math.random().toString(36).slice(2, 10); store.set('thread', thread); store.set('chat', chat); renderChat(); };
  $('#chatSuggest').addEventListener('click', (e) => { if (e.target.tagName === 'BUTTON') { $('#chatText').value = e.target.textContent; sendChat(); } });
  $('#chatText').addEventListener('input', autosize);
  $('#chatText').addEventListener('keydown', (e) => { if (e.key === 'Enter' && !e.shiftKey) { e.preventDefault(); sendChat(); } });
  $('#chatForm').onsubmit = (e) => { e.preventDefault(); sending ? stopChat() : sendChat(); };
  $('#chat').addEventListener('click', async (e) => {
    const b = e.target.closest('[data-dec]'); if (!b) return;
    const card = b.closest('[data-appr]'); card.querySelectorAll('button').forEach((x) => (x.disabled = true));
    try { await api('/api/assistant/approve', { method: 'POST', body: { id: card.dataset.appr, decision: b.dataset.dec } }); card.remove(); }
    catch (er) { toast(`Approval failed: ${er.message}`, true); card.querySelectorAll('button').forEach((x) => (x.disabled = false)); }
  });
  setInterval(() => { for (const el of document.querySelectorAll('#chat [data-since]')) el.textContent = secs(Date.now() - +el.dataset.since); }, 1000);
  document.addEventListener('keydown', (e) => { if (e.key === 'Escape') closeClaw(); });
}

// ---------------------------------------------------------------- DEMO MODE
async function loadDemoCfg() { try { L.demoCfg = await api('/api/demo'); } catch { /* defaults */ } }
const reEsc = (k) => k.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
function sensitiveName(n) { if (!n || !L.demoCfg.keywords?.length) return false; return new RegExp(L.demoCfg.keywords.map(reEsc).join('|'), 'i').test(String(n)); }
const sensitiveApp = (a) => !!(a.sensitive || L.demoCfg.groups.includes(a.group) || sensitiveName(a.name) || sensitiveName(a.href));
function applyDemoFilters() {
  if (!L.demoOn) return;
  L.apps = L.apps.filter((a) => !sensitiveApp(a));
  L.groups = (L.groups || []).filter((g) => !L.demoCfg.groups.includes(g));
  for (const h of L.hosts) { h.containers = (h.containers || []).filter((c) => !sensitiveName(c.name) && !sensitiveName(c.project) && !sensitiveName(c.image)); h.projects = (h.projects || []).filter((p) => !sensitiveName(p.name)); }
  if (L.overview) {
    const ids = new Set(L.apps.map((a) => a.id));
    L.overview.alerts = L.overview.alerts.filter((a) => !sensitiveName(a.title) && !sensitiveName(a.detail) && !(a.appId && !ids.has(a.appId)));
    const vis = L.apps.filter((a) => !a.hidden);
    L.overview.apps = { ...L.overview.apps, total: vis.length, up: vis.filter((a) => healthState(a) === 'up').length, down: vis.filter((a) => healthState(a) === 'down').length };
  }
  L.queue = L.queue.filter((x) => !sensitiveName(x.name));
}
const MASKS = [[/\b(password|passwd|pwd|pass|token|api[_ -]?key|secret|key)(\s*[:=]\s*)[^\s,;)]+/gi, '$1$2••••••'], [/\b(?:\d{1,3}\.){3}\d{1,3}\b/g, '•••.•••.•••.•••'], [/\b[a-z0-9-]+\.tail[0-9a-f]+\.ts\.net\b/gi, 'tailnet'], [/\btail[0-9a-f]{6}\b/gi, 'tailnet'], [/\+?1?\d{10}\b/g, '••••••••••']];
const maskText = (t) => MASKS.reduce((acc, [re, r]) => acc.replace(re, r), t);
function maskNode(root) {
  if (!L.demoOn || !root) return;
  if (root.nodeType === 3) { const m = maskText(root.nodeValue); if (m !== root.nodeValue) root.nodeValue = m; return; }
  if (root.nodeType !== 1 || /^(SCRIPT|STYLE|TEXTAREA)$/.test(root.tagName)) return;
  for (const el of [root, ...root.querySelectorAll('[title]')]) if (el.title) { const m = maskText(el.title); if (m !== el.title) el.title = m; }
  const w = document.createTreeWalker(root, NodeFilter.SHOW_TEXT);
  for (let n = w.nextNode(); n; n = w.nextNode()) { const m = maskText(n.nodeValue); if (m !== n.nodeValue) n.nodeValue = m; }
}
function applyDemoChrome() {
  document.body.classList.toggle('demo', L.demoOn);
  $('#demoBtn').classList.toggle('on', L.demoOn);
  $('#demoBtn').title = L.demoOn ? 'Demo mode is ON — click to leave (needs the demo password)' : 'Demo mode: hide sensitive apps, IPs and names';
  if (L.demoOn) { closeClaw(); maskNode(document.body); if (location.hash.startsWith('#logs') || location.hash.startsWith('#ctlogs')) location.hash = 'home'; }
}
function bindDemo() {
  new MutationObserver((muts) => { if (!L.demoOn) return; for (const m of muts) { if (m.type === 'characterData') maskNode(m.target); else m.addedNodes.forEach(maskNode); } }).observe(document.body, { childList: true, subtree: true, characterData: true });
  $('#demoBtn').onclick = async () => {
    if (!L.demoOn) { L.demoOn = true; store.set('demo', true); await loadDemoCfg(); applyDemoChrome(); toast('Demo mode on'); return refresh(); }
    const k = await ask('Leave demo mode', 'Demo password', '', {}); if (k === null) return;
    try { await api('/api/demo/exit', { method: 'POST', body: { code: k } }); } catch { return toast('Wrong password', true); }
    L.demoOn = false; store.set('demo', false); applyDemoChrome(); toast('Demo mode off'); location.reload();
  };
}

// ---------------------------------------------------------------- palette
function paletteItems(q) {
  const apps = L.apps.filter((a) => !a.hidden).map((a) => ({ icon: 'external', label: a.name, hint: a.group || 'app', go: () => window.open(a.href, '_blank', 'noopener') }));
  const hosts = L.hosts.map((h) => ({ icon: 'server', label: h.name, hint: 'machine', go: () => { L.openHosts.add(h.name); location.hash = 'monitor'; } }));
  const claw = q && !L.demoOn ? [{ icon: 'sparkle', label: `Ask OpenClaw: “${q}”`, hint: 'assistant', always: true, go: () => openClaw(q, true) }] : [];
  return [...apps, ...hosts, ...claw];
}

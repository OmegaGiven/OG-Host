// Backups: datastores, backup jobs, the backups in a datastore (restore, browse files,
// protect, notes), and the task log of what ran.
let H, $, esc, ic, api, toast, S;

const B = { store: 'local', stores: [], jobs: [], snaps: [], q: '', timer: null };
const fail = (e) => toast(e.message, true);
const when = (t) => (t ? new Date(t * 1000).toLocaleString() : '—');
const ago = (t) => (t ? H.ago(t) : '—');
const form = (f) => Object.fromEntries(new FormData(f).entries());
const rowBtn = (attr, icon, title, cls = '') => `<button class="row-btn ${cls}" ${attr} title="${esc(title)}">${ic(icon)}</button>`;
const canStore = () => H.can('Datastore.Allocate', '/storage');

export function init(h) {
  H = h; ({ $, esc, ic, api, toast, S } = h);
  H.ROUTES.backups = { el: 'backups', tab: S.lab ? 'server' : 'backups', run: () => load(), leave: () => clearInterval(B.timer) };
  H.taskLog = taskLog;
  H.backupNow = backupNow;
  $('#view-backups').addEventListener('click', onClick);
  $('#bkQ').addEventListener('input', (e) => { B.q = e.target.value.trim(); paintSnaps(); });
  $('#bkQForm').addEventListener('submit', (e) => e.preventDefault());
}

async function load() {
  try {
    const [stores, jobs] = await Promise.all([api('/v1/storage'), api('/v1/backup/jobs').catch(() => [])]);
    B.stores = stores; B.jobs = jobs;
    if (!stores.some((s) => s.id === B.store)) B.store = stores[0]?.id || 'local';
    paintStores(); paintJobs();
    loadSnaps(); loadTasks();
    clearInterval(B.timer);
    B.timer = setInterval(() => { if (!document.hidden) loadTasks(); }, 5000);
  } catch (e) { fail(e); }
}

// ---------------------------------------------------------------- datastores
function paintStores() {
  $('#bkStores').innerHTML = B.stores.map((s) => {
    const used = s.total ? s.total - s.free : 0;
    const pct = s.total ? Math.round((used / s.total) * 100) : 0;
    const where = s.type === 'dir' ? s.path : s.type === 'nfs' ? `nfs://${s.server}${s.export}` : `smb://${s.server}/${s.share}`;
    return `<div class="card ds-card ${s.id === B.store ? 'sel' : ''}" data-ds="${esc(s.id)}">
      <div class="ds-head"><b>${ic(s.type === 'dir' ? 'drive' : 'globe')} ${esc(s.id)}</b>${s.encrypt ? `<span class="chip good" title="Key ${esc(s.key || 'missing!')}">${ic('shield')} encrypted</span>` : ''}${s.enabled ? '' : '<span class="chip warn">off</span>'}${s.mounted ? '' : '<span class="chip">not mounted</span>'}<span class="sp"></span>
        ${canStore() ? `<button class="row-btn" data-ds-menu="${esc(s.id)}" title="More">${ic('more')}</button>` : ''}</div>
      <small class="muted addr">${esc(where)}</small>
      ${s.total ? `<div class="bar ${pct > 90 ? 'crit' : pct > 75 ? 'warn' : ''}"><i style="width:${pct}%"></i></div><small class="muted">${H.bytes(s.free)} free of ${H.bytes(s.total)}</small>` : '<small class="muted">space unknown until mounted</small>'}
      <small class="muted" id="dsStat-${esc(s.id)}"></small>
    </div>`;
  }).join('') + (canStore() ? `<button class="card ds-add" id="dsAdd">${ic('plus')} Add datastore</button>` : '');
  H.hydrateIcons($('#bkStores'));
  for (const s of B.stores) {
    if (!s.mounted && s.type !== 'dir') continue;
    api(`/v1/storage/${encodeURIComponent(s.id)}`).then((st) => {
      const el = document.getElementById(`dsStat-${s.id}`);
      if (el) el.textContent = `${st.backups} backups · ${H.bytes(st.logical)} in ${H.bytes(st.bytes)}${st.dedup > 1 ? ` · ×${st.dedup.toFixed(1)} dedup` : ''}`;
    }).catch(() => {});
  }
}

function storeModal(s) {
  const fresh = !s;
  s = s || { id: '', type: 'dir', path: '', server: '', export: '', share: '', username: '', domain: '', options: '', encrypt: true, enabled: true, comment: '', gc_schedule: 'sun 03:30' };
  const { box, close } = H.modal(`<h3>${ic('drive')}${fresh ? 'Add datastore' : `Datastore ${esc(s.id)}`}</h3>
    <form class="form" id="dsForm">
      <div class="row2"><label>Name<input name="id" value="${esc(s.id)}" ${fresh ? 'required' : 'readonly'} pattern="[A-Za-z0-9_][A-Za-z0-9_\\-]*" maxlength="32"></label>
      <label>Type<select name="type" ${fresh ? '' : 'disabled'}><option value="dir">Directory on this server</option><option value="nfs">NFS share</option><option value="cifs">SMB / Windows share</option></select></label></div>
      <div data-t="dir"><label>Directory<input name="path" value="${esc(s.path)}" placeholder="/mnt/backup-disk/og"></label></div>
      <div data-t="nfs cifs" class="row2"><label>Server<input name="server" value="${esc(s.server)}" placeholder="nas.local"></label>
        <label data-t="nfs">Export<input name="export" value="${esc(s.export)}" placeholder="/volume1/backups"></label>
        <label data-t="cifs">Share<input name="share" value="${esc(s.share)}" placeholder="backups"></label></div>
      <div data-t="cifs" class="row2"><label>User<input name="username" value="${esc(s.username)}" autocomplete="off"></label><label>Password <span class="muted small">${fresh ? '' : 'leave empty to keep'}</span><input name="password" type="password" autocomplete="new-password"></label></div>
      <div data-t="nfs cifs"><label>Mount options <span class="muted small">optional, e.g. vers=4.2</span><input name="options" value="${esc(s.options)}"></label></div>
      <div class="row2"><label>Garbage collection <span class="muted small">frees space of deleted backups</span><input name="gc_schedule" value="${esc(s.gc_schedule)}" placeholder="sun 03:30"></label><label>Comment<input name="comment" value="${esc(s.comment)}"></label></div>
      <label class="check"><input type="checkbox" name="encrypt" ${s.encrypt ? 'checked' : ''}> Encrypt backups <span class="muted small">(a key is made on this server; keep a copy of it)</span></label>
      <label class="check"><input type="checkbox" name="enabled" ${s.enabled ? 'checked' : ''}> On</label>
      <p class="err" id="dsErr"></p>
      <div class="modal-actions"><button type="button" class="ghost-btn" data-x>Cancel</button><button class="primary-btn">${fresh ? 'Add' : 'Save'}</button></div>
    </form>`, { wide: true });
  const f = box.querySelector('#dsForm');
  f.type.value = s.type;
  const sync = () => { for (const el of box.querySelectorAll('[data-t]')) el.hidden = !el.dataset.t.split(' ').includes(f.type.value); };
  f.type.onchange = sync; sync();
  box.querySelector('[data-x]').onclick = close;
  f.onsubmit = async (e) => {
    e.preventDefault();
    const v = form(f);
    const body = { ...v, type: f.type.value, encrypt: f.encrypt.checked, enabled: f.enabled.checked };
    if (!v.password) delete body.password;
    try {
      if (fresh) await api('/v1/storage', { method: 'POST', body }); else await api(`/v1/storage/${encodeURIComponent(s.id)}`, { method: 'PUT', body });
      close(); toast(fresh ? 'Datastore added' : 'Saved'); B.store = v.id || s.id; load();
    } catch (err) { box.querySelector('#dsErr').textContent = err.message; }
  };
}

async function showKey(id) {
  try {
    const k = await api(`/v1/storage/${encodeURIComponent(id)}/key`);
    const { box, close } = H.modal(`<h3>${ic('shield')}Encryption key of ${esc(id)}</h3>
      <p class="muted">Without this key nobody (you included) can read the backups in this datastore. Keep a copy somewhere safe and away from this server, e.g. in your password manager. To restore at another site, add the same share there and put this key in.</p>
      <pre class="secret">${esc(k.key)}</pre><p class="muted small">Fingerprint ${esc(k.fingerprint)}</p>
      <div class="modal-actions"><button class="ghost-btn" data-copy>${ic('copy')}Copy</button><button class="primary-btn" data-x>Done</button></div>`);
    box.querySelector('[data-x]').onclick = close;
    box.querySelector('[data-copy]').onclick = () => navigator.clipboard.writeText(k.key).then(() => toast('Copied'), fail);
  } catch (e) { fail(e); }
}

function setKey(id) {
  const { box, close } = H.modal(`<h3>${ic('shield')}Put in a key for ${esc(id)}</h3><form class="form"><p class="muted">For backups made elsewhere with this key (64 hex characters). The current key is replaced: keep a copy of it first.</p><label>Key<input name="key" required pattern="[0-9a-fA-F]{64}" autocomplete="off"></label><p class="err"></p><div class="modal-actions"><button type="button" class="ghost-btn" data-x>Cancel</button><button class="primary-btn">Save</button></div></form>`);
  box.querySelector('[data-x]').onclick = close;
  box.querySelector('form').onsubmit = async (e) => {
    e.preventDefault();
    try { const r = await api(`/v1/storage/${encodeURIComponent(id)}/key`, { method: 'POST', body: { key: e.target.key.value.trim() } }); close(); toast(`Key ${r.fingerprint} saved`); load(); } catch (err) { box.querySelector('.err').textContent = err.message; }
  };
}

// ---------------------------------------------------------------- jobs
const keepText = (k) => Object.entries(k || {}).filter(([, v]) => v).map(([n, v]) => `${v} ${n}`).join(', ') || 'keep all';
function paintJobs() {
  $('#bkJobs').innerHTML = B.jobs.map(({ job: j, state, next, targets }) => `<tr>
    <td class="name"><b>${esc(j.id)}</b> ${j.enabled ? '' : '<span class="chip">off</span>'}<small>${esc(j.comment || '')}</small></td>
    <td>${esc(j.schedule)}<small class="muted block">${next ? `next ${when(next)}` : ''}</small></td>
    <td>${j.all ? `everything (${targets.length})` : targets.map((t) => `<span class="chip">${esc(t)}</span>`).join(' ')}${j.exclude?.length ? `<small class="muted block">except ${esc(j.exclude.join(', '))}</small>` : ''}</td>
    <td>${esc(j.store)}<small class="muted block">${esc(j.opts.mode)} · ${esc(keepText(j.keep))}</small></td>
    <td>${state ? `<span class="chip ${state.last_status === 'ok' ? 'good' : 'crit'}" data-task="${esc(state.last_task)}" title="${esc(state.last_status)}">${state.last_status === 'ok' ? 'ok' : 'failed'}</span> <small class="muted">${ago(state.last_run)}</small>` : '<span class="muted">never ran</span>'}</td>
    <td>${canStore() ? `<div class="row-actions">${rowBtn(`data-job-run="${esc(j.id)}"`, 'play', 'Run now')}${rowBtn(`data-job-edit="${esc(j.id)}"`, 'edit', 'Edit')}${rowBtn(`data-job-del="${esc(j.id)}"`, 'trash', 'Remove', 'stop')}</div>` : ''}</td></tr>`).join('') || '<tr><td colspan="6" class="muted">No backup jobs yet: add one to back up on a schedule.</td></tr>';
  H.hydrateIcons($('#bkJobs'));
}

async function jobModal(j) {
  const fresh = !j;
  j = j || { id: '', enabled: true, schedule: 'daily 02:00', store: B.store, all: true, stacks: [], containers: [], exclude: [], opts: { mode: 'pause', include_binds: false }, keep: { last: 3, daily: 7, weekly: 4, monthly: 6, yearly: 0, hourly: 0 }, notify: 'failure', comment: '' };
  const stacks = await api('/v1/stacks').catch(() => []);
  const lone = S.containers.filter((c) => !c.project);
  const opt = (v, cur) => `<option ${v === cur ? 'selected' : ''}>${esc(v)}</option>`;
  const { box, close } = H.modal(`<h3>${ic('shield')}${fresh ? 'New backup job' : `Job ${esc(j.id)}`}</h3>
    <form class="form" id="jobForm">
      <div class="row2"><label>Name<input name="id" value="${esc(j.id)}" ${fresh ? 'required' : 'readonly'} pattern="[A-Za-z0-9_][A-Za-z0-9_\\-]*"></label>
      <label>When <span class="muted small">daily 02:00 · sun 03:30 · every 6h · cron</span><input name="schedule" value="${esc(j.schedule)}" required list="schedList"></label></div>
      <datalist id="schedList"><option>daily 02:00</option><option>hourly</option><option>every 6h</option><option>mon..fri 22:00</option><option>sun 03:00</option></datalist>
      <div class="row2"><label>Datastore<select name="store">${B.stores.map((s) => opt(s.id, j.store)).join('')}</select></label>
      <label>Mode<select name="mode"><option value="pause" ${j.opts.mode === 'pause' ? 'selected' : ''}>pause (consistent, no restart)</option><option value="stop" ${j.opts.mode === 'stop' ? 'selected' : ''}>stop and start again</option><option value="live" ${j.opts.mode === 'live' ? 'selected' : ''}>live (no pause)</option></select></label></div>
      <label class="check"><input type="checkbox" name="all" ${j.all ? 'checked' : ''}> Everything on this server (new stacks and containers too)</label>
      <div class="pick-grid" id="jobPick">
        <fieldset><legend>Stacks</legend>${stacks.map((s) => `<label class="check"><input type="checkbox" name="s:${esc(s.name)}" ${(j.all ? !j.exclude.includes(s.name) : j.stacks.includes(s.name)) ? 'checked' : ''}> ${esc(s.name)}</label>`).join('') || '<span class="muted small">none</span>'}</fieldset>
        <fieldset><legend>Containers (not in a stack)</legend>${lone.map((c) => `<label class="check"><input type="checkbox" name="c:${esc(c.name)}" ${(j.all ? !j.exclude.includes(c.name) : j.containers.includes(c.name)) ? 'checked' : ''}> ${esc(c.name)}</label>`).join('') || '<span class="muted small">none</span>'}</fieldset>
      </div>
      <p class="muted small" id="jobPickHint"></p>
      <label class="check"><input type="checkbox" name="binds" ${j.opts.include_binds ? 'checked' : ''}> Also bind mounts outside the stack folders <span class="muted small">(can be large: media libraries…)</span></label>
      <fieldset class="keep"><legend>Keep</legend>${['last', 'hourly', 'daily', 'weekly', 'monthly', 'yearly'].map((k) => `<label>${k}<input name="k:${k}" type="number" min="0" max="1000" value="${j.keep[k] || 0}"></label>`).join('')}</fieldset>
      <div class="row2"><label>Message me (Signal)<select name="notify">${['failure', 'always', 'never'].map((v) => `<option value="${v}" ${j.notify === v ? 'selected' : ''}>${v === 'failure' ? 'when something fails' : v}</option>`).join('')}</select></label><label>Comment<input name="comment" value="${esc(j.comment)}"></label></div>
      <label class="check"><input type="checkbox" name="enabled" ${j.enabled ? 'checked' : ''}> On</label>
      <p class="err" id="jobErr"></p>
      <div class="modal-actions"><button type="button" class="ghost-btn" data-x>Cancel</button><button class="primary-btn">${fresh ? 'Add' : 'Save'}</button></div>
    </form>`, { wide: true });
  const f = box.querySelector('#jobForm');
  const hint = () => { $('#jobPickHint').textContent = f.all.checked ? 'Unticked ones are left out.' : 'Only the ticked ones are backed up.'; };
  f.all.onchange = hint; hint();
  box.querySelector('[data-x]').onclick = close;
  f.onsubmit = async (e) => {
    e.preventDefault();
    const v = form(f);
    const ticked = (p) => [...f.querySelectorAll(`input[name^="${p}:"]`)].filter((x) => x.checked).map((x) => x.name.slice(2));
    const unticked = (p) => [...f.querySelectorAll(`input[name^="${p}:"]`)].filter((x) => !x.checked).map((x) => x.name.slice(2));
    const body = {
      id: v.id, schedule: v.schedule, store: v.store, all: f.all.checked, enabled: f.enabled.checked, notify: v.notify, comment: v.comment,
      stacks: f.all.checked ? [] : ticked('s'), containers: f.all.checked ? [] : ticked('c'), exclude: f.all.checked ? [...unticked('s'), ...unticked('c')] : [],
      opts: { mode: v.mode, include_binds: f.binds.checked, notes: '' },
      keep: Object.fromEntries(['last', 'hourly', 'daily', 'weekly', 'monthly', 'yearly'].map((k) => [k, +v[`k:${k}`] || 0])),
    };
    try { await api('/v1/backup/jobs', { method: 'POST', body }); close(); toast('Saved'); load(); } catch (err) { $('#jobErr').textContent = err.message; }
  };
}

// ---------------------------------------------------------------- backups in the datastore
async function loadSnaps() {
  $('#bkStoreName').textContent = B.store;
  try { B.snaps = await api(`/v1/storage/${encodeURIComponent(B.store)}/backups`); } catch (e) { B.snaps = []; $('#bkList').innerHTML = `<p class="empty">${esc(e.message)}</p>`; return; }
  paintSnaps();
}

function paintSnaps() {
  const q = B.q.toLowerCase();
  const groups = {};
  for (const s of B.snaps) if (!q || s.group.toLowerCase().includes(q) || (s.notes || '').toLowerCase().includes(q)) (groups[s.group] ||= []).push(s);
  const canRestore = H.can('Backup.Restore', '/storage');
  $('#bkMeta').textContent = `${B.snaps.length} backups in ${Object.keys(groups).length} groups`;
  $('#bkList').innerHTML = Object.entries(groups).map(([g, list]) => `
    <div class="panel table-wrap bk-group">
      <div class="bk-group-head"><b>${ic(g.startsWith('stack/') ? 'layers' : 'box')} ${esc(g)}</b><span class="muted">${list.length} · newest ${ago(list[0].time)}</span><span class="sp"></span>${H.can('Backup.Create', '*') ? `<button class="link-btn" data-bk-now="${esc(g)}">${ic('play')} Back up now</button>` : ''}</div>
      <table class="table"><tbody>${list.map((s) => `<tr>
        <td>${when(s.time)}${s.notes ? `<small class="muted block">${esc(s.notes)}</small>` : ''}</td>
        <td class="num">${H.bytes(s.size)}</td>
        <td>${s.encrypted ? `<span class="chip" title="key ${esc(s.encrypted)}">${ic('shield')}</span>` : ''}${s.protected ? '<span class="chip accent">protected</span>' : ''}${s.verified ? (s.verified.ok ? '<span class="chip good" title="verified">verified</span>' : `<span class="chip crit" title="${esc(s.verified.error)}">damaged</span>`) : ''}${s.skipped_binds ? `<span class="chip warn" title="bind mounts outside the stack folder weren't included">${s.skipped_binds} bind(s) left out</span>` : ''}</td>
        <td class="muted">${esc(s.mode || '')} · ${(s.duration_ms / 1000).toFixed(1)} s</td>
        <td><div class="row-actions">
          ${canRestore ? rowBtn(`data-restore="${esc(s.id)}"`, 'restart', 'Restore') + rowBtn(`data-browse="${esc(s.id)}"`, 'folder', 'Browse files') : ''}
          ${canStore() ? rowBtn(`data-protect="${esc(s.id)}" data-on="${s.protected ? 1 : 0}"`, 'pin', s.protected ? 'Unprotect' : 'Protect from pruning') + rowBtn(`data-notes="${esc(s.id)}"`, 'note', 'Notes') + rowBtn(`data-verify="${esc(s.id)}"`, 'check', 'Verify') + rowBtn(`data-bk-del="${esc(s.id)}" ${s.protected ? 'disabled' : ''}`, 'trash', 'Delete', 'stop') : ''}
        </div></td></tr>`).join('')}</tbody></table>
    </div>`).join('') || '<p class="empty">No backups in this datastore yet.</p>';
  H.hydrateIcons($('#bkList'));
}

async function restoreModal(id) {
  const s = B.snaps.find((x) => x.id === id);
  const sys = H.can('Sys.Modify', '/');
  const { box, close } = H.modal(`<h3>${ic('restart')}Restore ${esc(s.group)}</h3>
    <p class="muted">From ${when(s.time)} (${H.bytes(s.size)}).</p>
    <form class="form" id="rsForm">
      <label class="check"><input type="radio" name="how" value="place" checked> Put it back: stops ${s.kind === 'stack' ? 'the stack' : 'the container'}, <b>replaces its current data</b> with the backup, starts it again</label>
      ${sys ? '<label class="check"><input type="radio" name="how" value="dir"> Unpack into a directory, to pick files out</label><label data-dir hidden>Directory<input name="to" placeholder="/srv/restore"></label>' : ''}
      <fieldset><legend>What</legend>${s.archives.map((a) => `<label class="check"><input type="checkbox" name="a:${esc(a.name)}" checked> ${esc(a.name)} <span class="muted small">${H.bytes(a.size)}</span></label>`).join('')}</fieldset>
      <label class="check" data-place><input type="checkbox" name="start"> Start it afterwards even if it wasn't running</label>
      <p class="err" id="rsErr"></p>
      <div class="modal-actions"><button type="button" class="ghost-btn" data-x>Cancel</button><button class="danger-btn">Restore</button></div>
    </form>`, { wide: true });
  const f = box.querySelector('#rsForm');
  f.onchange = () => { const dir = f.how.value === 'dir'; box.querySelector('[data-dir]') && (box.querySelector('[data-dir]').hidden = !dir); box.querySelector('[data-place]').hidden = dir; };
  box.querySelector('[data-x]').onclick = close;
  f.onsubmit = async (e) => {
    e.preventDefault();
    const archives = [...f.querySelectorAll('input[name^="a:"]')].filter((x) => x.checked).map((x) => x.name.slice(2));
    const body = { store: B.store, snapshot: id, archives: archives.length === s.archives.length ? [] : archives, start: f.start.checked };
    if (f.how.value === 'dir') { if (!f.to.value.trim()) { $('#rsErr').textContent = 'Give a directory'; return; } body.to = f.to.value.trim(); }
    try { const r = await api('/v1/restore', { method: 'POST', body }); close(); taskLog(r.task); } catch (err) { $('#rsErr').textContent = err.message; }
  };
}

async function browse(id) {
  const s = B.snaps.find((x) => x.id === id);
  const { box } = H.modal(`<h3>${ic('folder')}Files in ${esc(s.group)}</h3><p class="muted">${when(s.time)}</p>
    <div class="seg seg-sm" id="brArch">${s.archives.map((a, i) => `<button data-a="${esc(a.name)}" class="${i ? '' : 'on'}">${esc(a.name)}</button>`).join('')}</div>
    <form class="q" id="brQ" style="margin:10px 0"><i data-icon="search"></i><input placeholder="Filter paths"></form>
    <div class="br-list" id="brList"><p class="muted">Loading…</p></div>`, { wide: true });
  let files = [], arch = s.archives[0]?.name;
  const paint = () => {
    const q = box.querySelector('#brQ input').value.toLowerCase();
    const shown = files.filter((f) => !q || f.path.toLowerCase().includes(q)).slice(0, 500);
    box.querySelector('#brList').innerHTML = `<table class="table"><tbody>${shown.map((f) => `<tr><td class="addr">${ic(f.kind === 'dir' ? 'folder' : f.kind === 'link' ? 'link' : 'file')} ${esc(f.path)}</td><td class="num">${f.kind === 'file' ? H.bytes(f.size) : ''}</td><td>${f.kind === 'file' && H.can('Backup.Restore', '/storage') ? `<a class="row-btn" title="Download" href="/v1/storage/${encodeURIComponent(B.store)}/backups/download?snapshot=${encodeURIComponent(id)}&archive=${encodeURIComponent(arch)}&path=${encodeURIComponent(f.path)}">${ic('download')}</a>` : ''}</td></tr>`).join('')}</tbody></table>${files.length > shown.length ? `<p class="muted small">${files.length - shown.length} more: filter to narrow down</p>` : ''}`;
  };
  const loadArch = async (a) => {
    arch = a; box.querySelector('#brList').innerHTML = '<p class="muted">Loading…</p>';
    try { files = await api(`/v1/storage/${encodeURIComponent(B.store)}/backups/files?snapshot=${encodeURIComponent(id)}&archive=${encodeURIComponent(a)}`); paint(); } catch (e) { box.querySelector('#brList').innerHTML = `<p class="err">${esc(e.message)}</p>`; }
  };
  box.querySelector('#brArch').onclick = (e) => { const b = e.target.closest('[data-a]'); if (!b) return; H.segOn('#brArch', b); loadArch(b.dataset.a); };
  box.querySelector('#brQ').onsubmit = (e) => e.preventDefault();
  box.querySelector('#brQ input').oninput = paint;
  if (arch) loadArch(arch);
}

/** Back up now: `preset` = a group like "stack/demo", or nothing for a picker. */
async function backupNow(preset) {
  const stacks = await api('/v1/stacks').catch(() => []);
  const items = [...stacks.map((s) => ['stack', s.name]), ...S.containers.filter((c) => !c.project).map((c) => ['container', c.name])];
  const isOn = (k, n) => preset ? preset === `${k === 'container' ? 'ct' : k}/${n}` : false;
  const { box, close } = H.modal(`<h3>${ic('shield')}Back up now</h3>
    <form class="form" id="bnForm">
      <div class="pick-grid"><fieldset><legend>What</legend>${items.map(([k, n]) => `<label class="check"><input type="checkbox" name="${k}:${esc(n)}" ${isOn(k, n) ? 'checked' : ''}> ${ic(k === 'stack' ? 'layers' : 'box')} ${esc(n)}</label>`).join('') || '<span class="muted">Nothing to back up.</span>'}</fieldset></div>
      <div class="row2"><label>Datastore<select name="store">${B.stores.map((s) => `<option ${s.id === B.store ? 'selected' : ''}>${esc(s.id)}</option>`).join('')}</select></label>
      <label>Mode<select name="mode"><option value="pause">pause (consistent, no restart)</option><option value="stop">stop and start again</option><option value="live">live</option></select></label></div>
      <label>Notes<input name="notes" placeholder="e.g. before upgrading"></label>
      <p class="err" id="bnErr"></p>
      <div class="modal-actions"><button type="button" class="ghost-btn" data-x>Cancel</button><button class="primary-btn">Back up</button></div>
    </form>`, { wide: true });
  box.querySelector('[data-x]').onclick = close;
  box.querySelector('#bnForm').onsubmit = async (e) => {
    e.preventDefault();
    const f = e.target;
    const targets = [...f.querySelectorAll('input[type=checkbox]')].filter((x) => x.checked).map((x) => { const [kind, ...n] = x.name.split(':'); return { kind, name: n.join(':') }; });
    if (!targets.length) { $('#bnErr').textContent = 'Pick something'; return; }
    try { const r = await api('/v1/backup', { method: 'POST', body: { store: f.store.value, targets, opts: { mode: f.mode.value, notes: f.notes.value } } }); close(); taskLog(r.task, () => { if (S.view === 'backups') load(); }); } catch (err) { $('#bnErr').textContent = err.message; }
  };
}

// ---------------------------------------------------------------- tasks
async function loadTasks() {
  let list;
  try { list = await api('/v1/tasks?limit=12'); } catch { return; }
  const el = $('#bkTasks'); if (!el) return;
  el.innerHTML = list.map((t) => `<tr class="click" data-task="${esc(t.id)}"><td>${t.status === 'running' ? '<span class="chip accent">running</span>' : `<span class="chip ${t.status === 'ok' ? 'good' : t.status === 'warnings' ? 'warn' : 'crit'}">${esc(t.status)}</span>`}</td><td><b>${esc(t.kind)}</b> ${esc(t.target)}</td><td class="muted">${esc(t.user)}</td><td class="muted">${ago(t.start)}</td><td class="num">${t.end ? `${t.end - t.start} s` : ''}</td></tr>`).join('') || '<tr><td class="muted">Nothing ran yet.</td></tr>';
}

/** Live log of a task in a modal; `then` runs once it ends. */
function taskLog(id, then) {
  const { box, close } = H.modal(`<h3>${ic('logs')}<span id="tlTitle">Task</span></h3><pre class="task-log" id="tlLog"></pre><div class="modal-actions"><button class="ghost-btn" id="tlStop" hidden>Stop</button><button class="primary-btn" data-x>Close</button></div>`, { wide: true });
  let n = 0, timer = null, ended = false;
  const tick = async () => {
    try {
      const r = await api(`/v1/tasks/${encodeURIComponent(id)}?since=${n}`);
      const log = box.querySelector('#tlLog'); if (!log) { clearInterval(timer); return; }
      n += r.log.length;
      const atEnd = log.scrollTop + log.clientHeight >= log.scrollHeight - 8;
      log.textContent += r.log.map((l) => l.replace(/^\S+Z /, '')).join('\n') + (r.log.length ? '\n' : '');
      if (atEnd) log.scrollTop = log.scrollHeight;
      const i = r.info;
      box.querySelector('#tlTitle').innerHTML = `${esc(i.kind)} ${esc(i.target)} <span class="chip ${i.status === 'ok' ? 'good' : i.status === 'running' ? 'accent' : i.status === 'warnings' ? 'warn' : 'crit'}">${esc(i.status)}</span>`;
      box.querySelector('#tlStop').hidden = i.status !== 'running' || !H.can('Sys.Modify', '/');
      if (i.status !== 'running' && !ended) { ended = true; clearInterval(timer); loadTasks(); if (then) then(i); }
    } catch (e) { clearInterval(timer); fail(e); }
  };
  box.querySelector('[data-x]').onclick = () => { clearInterval(timer); close(); };
  box.querySelector('#tlStop').onclick = () => api(`/v1/tasks/${encodeURIComponent(id)}/stop`, { method: 'POST' }).catch(fail);
  tick(); timer = setInterval(tick, 1000);
}

// ---------------------------------------------------------------- clicks
async function onClick(e) {
  const t = e.target;
  const on = (sel) => t.closest(sel);
  let el;
  if ((el = on('[data-ds-menu]'))) {
    e.stopPropagation();
    const s = B.stores.find((x) => x.id === el.dataset.dsMenu);
    const items = [['edit', 'Edit', () => storeModal(s)], ['check', 'Verify backups', () => api(`/v1/storage/${encodeURIComponent(s.id)}/verify`, { method: 'POST', body: {} }).then((r) => taskLog(r.task, () => loadSnaps()), fail)], ['trash', 'Garbage collection now', () => api(`/v1/storage/${encodeURIComponent(s.id)}/gc`, { method: 'POST' }).then((r) => taskLog(r.task, () => load()), fail)]];
    if (H.can('Sys.Modify', '/')) items.push(null, ['shield', 'Show encryption key', () => showKey(s.id)], ['shield', 'Put in a key…', () => setKey(s.id)]);
    if (s.id !== 'local') items.push(null, ['trash', 'Remove (keeps the backups)', async () => { if (await H.confirmBox(`Remove datastore ${s.id}?`, 'OG-Host forgets it; the backups stay on the disk or share. Its encryption key is kept on this server, renamed.', 'Remove')) api(`/v1/storage/${encodeURIComponent(s.id)}`, { method: 'DELETE' }).then(() => { toast('Removed'); load(); }, fail); }, 'stop']);
    H.showMenu(el, items);
  } else if ((el = on('[data-ds]'))) { B.store = el.dataset.ds; paintStores(); loadSnaps(); }
  else if (on('#dsAdd')) storeModal(null);
  else if (on('#jobAdd')) jobModal(null);
  else if (on('#bkNow')) backupNow();
  else if ((el = on('[data-job-run]'))) api(`/v1/backup/jobs/${encodeURIComponent(el.dataset.jobRun)}/run`, { method: 'POST' }).then((r) => taskLog(r.task, () => load()), fail);
  else if ((el = on('[data-job-edit]'))) jobModal(B.jobs.find((x) => x.job.id === el.dataset.jobEdit).job);
  else if ((el = on('[data-job-del]'))) { if (await H.confirmBox(`Remove job ${el.dataset.jobDel}?`, 'Backups it made stay.', 'Remove')) api(`/v1/backup/jobs/${encodeURIComponent(el.dataset.jobDel)}`, { method: 'DELETE' }).then(load, fail); }
  else if ((el = on('[data-task]'))) taskLog(el.dataset.task);
  else if ((el = on('[data-restore]'))) restoreModal(el.dataset.restore);
  else if ((el = on('[data-browse]'))) browse(el.dataset.browse);
  else if ((el = on('[data-bk-now]'))) backupNow(el.dataset.bkNow);
  else if ((el = on('[data-protect]'))) api(`/v1/storage/${encodeURIComponent(B.store)}/backups/notes`, { method: 'POST', body: { snapshot: el.dataset.protect, protected: el.dataset.on !== '1' } }).then(loadSnaps, fail);
  else if ((el = on('[data-verify]'))) api(`/v1/storage/${encodeURIComponent(B.store)}/verify`, { method: 'POST', body: { snapshot: el.dataset.verify } }).then((r) => taskLog(r.task, () => loadSnaps()), fail);
  else if ((el = on('[data-notes]'))) {
    const s = B.snaps.find((x) => x.id === el.dataset.notes);
    const { box, close } = H.modal(`<h3>${ic('note')}Notes</h3><form class="form"><textarea name="n" rows="4">${esc(s.notes || '')}</textarea><div class="modal-actions"><button type="button" class="ghost-btn" data-x>Cancel</button><button class="primary-btn">Save</button></div></form>`);
    box.querySelector('[data-x]').onclick = close;
    box.querySelector('form').onsubmit = (ev) => { ev.preventDefault(); api(`/v1/storage/${encodeURIComponent(B.store)}/backups/notes`, { method: 'POST', body: { snapshot: s.id, notes: ev.target.n.value } }).then(() => { close(); loadSnaps(); }, fail); };
  } else if ((el = on('[data-bk-del]'))) {
    if (await H.confirmBox('Delete this backup?', `${el.dataset.bkDel} is deleted for good (its space comes back at the next garbage collection).`, 'Delete')) api(`/v1/storage/${encodeURIComponent(B.store)}/backups?snapshot=${encodeURIComponent(el.dataset.bkDel)}`, { method: 'DELETE' }).then(loadSnaps, fail);
  }
}

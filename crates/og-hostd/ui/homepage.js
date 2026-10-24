// Apps homepage and go-links aliases (GET /v1/apps, /v1/aliases). Tiles come from the
// compose projects on this node; aliases redirect from a tailnet name like go2/<alias>.
let H, $, $$, esc, ic, api, toast, S;

const A = { tiles: [], node: null, listen: null, links: {}, timer: null, q: '', showHidden: false };
const fail = (e) => toast(e.message, true);
const STATE = { running: ['good', 'Up'], sleeping: ['', 'Sleeping'], stopped: ['crit', 'Stopped'], partial: ['warn', 'Partly up'], link: ['', 'Link'] };

export function init(h) {
  H = h; ({ $, $$, esc, ic, api, toast, S } = h);
  H.ROUTES.myapps = { el: 'myapps', tab: 'myapps', run: () => open('apps'), leave: () => clearInterval(A.timer) };
  H.ROUTES.links = { el: 'myapps', tab: 'myapps', run: () => open('links'), leave: () => clearInterval(A.timer) };
  const v = $('#view-myapps');
  v.addEventListener('click', onClick);
  $('#maQ').addEventListener('input', (e) => { A.q = e.target.value.trim().toLowerCase(); paint(); });
}

async function load() {
  try {
    const [a, l] = await Promise.all([api('/v1/apps'), api('/v1/aliases')]);
    A.tiles = a.tiles || []; A.node = a.node; A.listen = a.alias_listen; A.name = a.alias_name; A.links = l.links || {};
    paint();
  } catch (e) { fail(e); }
}

function open(sub) {
  A.sub = sub;
  for (const b of $$('#maTabs button')) b.classList.toggle('on', b.dataset.sub === sub);
  $('#maApps').hidden = sub !== 'apps'; $('#maLinks').hidden = sub !== 'links';
  load();
  clearInterval(A.timer);
  A.timer = setInterval(() => { if (!document.hidden) load(); }, 15000);
}
const canEdit = () => H.can('Sys.Modify', '/');
const match = (...xs) => !A.q || xs.some((x) => String(x || '').toLowerCase().includes(A.q));

function initials(t) { return (t.match(/\b\w/g) || ['?']).slice(0, 2).join('').toUpperCase(); }
function tileIcon(t) {
  if (t.icon && /^(https?:|data:)/.test(t.icon)) return `<span class="ma-ico"><img src="${esc(t.icon)}" alt="" referrerpolicy="no-referrer"></span>`;
  if (t.icon && H.ICONS[t.icon]) return `<span class="ma-ico">${ic(t.icon)}</span>`;
  return `<span class="ma-ico ma-letters">${esc(initials(t.title))}</span>`;
}

function paint() {
  $('#maSub').textContent = A.node ? `${A.node} · aliases ${A.listen ? 'on' : 'off'}` : '';
  if (A.sub === 'links') return paintLinks();
  const vis = A.tiles.filter((t) => match(t.title, t.id, t.description, t.url, ...(t.aliases || [])));
  const shown = vis.filter((t) => !t.hidden && t.url);
  const noWeb = vis.filter((t) => !t.hidden && !t.url);
  const hidden = vis.filter((t) => t.hidden);
  const groups = new Map();
  for (const t of shown) { const g = t.group || 'Apps'; if (!groups.has(g)) groups.set(g, []); groups.get(g).push(t); }
  const order = [...groups.keys()].sort((a, b) => (a === 'Apps' ? -1 : b === 'Apps' ? 1 : a.localeCompare(b)));
  let html = order.map((g) => `<div class="section-head${g === order[0] ? ' first' : ''}"><h2>${esc(g)}</h2><span class="muted">${groups.get(g).length}</span></div>
    <div class="ma-grid">${groups.get(g).sort((a, b) => a.title.localeCompare(b.title)).map(tile).join('')}</div>`).join('');
  if (!shown.length) html = `<p class="muted">${A.q ? 'Nothing matches.' : 'No apps with a web address yet.'}</p>`;
  if (noWeb.length) html += `<details class="ma-more"><summary>Without a web address (${noWeb.length})</summary><div class="ma-grid">${noWeb.map(tile).join('')}</div><p class="muted small">Give one an address with the ${ic('edit')} button, or an <code>og.url</code> label.</p></details>`;
  if (hidden.length) html += `<details class="ma-more"><summary>Hidden (${hidden.length})</summary><div class="ma-grid">${hidden.map(tile).join('')}</div></details>`;
  $('#maApps').innerHTML = html;
}

function tile(t) {
  const [cls, label] = STATE[t.state] || ['', t.state];
  const alias = A.listen && t.aliases && t.aliases[0] ? `<span class="chip">${esc(aliasHost())}/${esc(t.aliases[0])}</span>` : '';
  const body = `${tileIcon(t)}<span class="ma-txt"><b>${esc(t.title)}</b><small>${esc(t.description || (t.url ? t.url.replace(/^https?:\/\//, '') : 'no web address'))}</small><span class="ma-meta"><span class="chip ${cls}">${esc(label)}</span>${alias}</span></span>`;
  const edit = canEdit() ? `<button class="row-btn ma-edit" data-edit="${esc(t.id)}" title="Edit tile">${ic('edit')}</button>` : '';
  return t.url
    ? `<div class="ma-tile"><a class="ma-link" href="${esc(t.url)}" target="_blank" rel="noopener noreferrer">${body}</a>${edit}</div>`
    : `<div class="ma-tile ma-off"><span class="ma-link">${body}</span>${edit}</div>`;
}

// the alias node's short name; links say go2/… when that's what aliases.json was set up for
const aliasHost = () => A.name || 'go';

function paintLinks() {
  const rows = Object.entries(A.links).filter(([k, l]) => match(k, l.url, l.title));
  const auto = A.tiles.filter((t) => t.url && !t.hidden).flatMap((t) => (t.aliases || []).filter((a) => !A.links[a]).map((a) => [a, t]));
  const edit = canEdit();
  $('#maLinks').innerHTML = `
    ${A.listen ? '' : `<div class="panel ma-note">${ic('alert')} The alias listener is off. On the node: <code>og-hostd alias listen 127.0.0.1:8412</code>, then restart og-hostd.</div>`}
    <div class="section-head first"><h2>Your aliases</h2><span class="muted">${rows.length}</span><span class="sp"></span>${edit ? `<button class="primary-btn" data-add="1">${ic('plus')} New alias</button>` : ''}</div>
    <div class="panel ma-table">${rows.length ? rows.map(([k, l]) => `<div class="ma-row"><a href="${esc(l.url)}" target="_blank" rel="noopener noreferrer"><b>${esc(aliasHost())}/${esc(k)}</b></a><span class="muted ma-url">${esc(l.url)}</span>${l.tile ? '<span class="chip accent">tile</span>' : ''}${edit ? `<button class="row-btn" data-ledit="${esc(k)}" title="Edit">${ic('edit')}</button><button class="row-btn danger" data-ldel="${esc(k)}" title="Delete">${ic('trash')}</button>` : ''}</div>`).join('') : '<p class="muted">No aliases yet.</p>'}</div>
    <div class="section-head"><h2>From apps</h2><span class="muted">automatic: each app's name</span></div>
    <div class="panel ma-table">${auto.filter(([a, t]) => match(a, t.url)).map(([a, t]) => `<div class="ma-row"><a href="${esc(t.url)}" target="_blank" rel="noopener noreferrer"><b>${esc(aliasHost())}/${esc(a)}</b></a><span class="muted ma-url">${esc(t.url)}</span></div>`).join('') || '<p class="muted">None.</p>'}</div>`;
  H.hydrateIcons($('#maLinks'));
}

function linkForm(name, l = {}) {
  const { box, close } = H.modal(`<h3>${ic('link')}${name ? `Edit ${esc(aliasHost())}/${esc(name)}` : 'New alias'}</h3>
    <label>Alias<input id="lfName" value="${esc(name || '')}" ${name ? 'disabled' : ''} placeholder="host2" pattern="[a-z0-9._-]+" required></label>
    <label>Goes to<input id="lfUrl" value="${esc(l.url || '')}" placeholder="https://og-host2.tail6c6721.ts.net:8006" required></label>
    <label>Title (optional)<input id="lfTitle" value="${esc(l.title || '')}"></label>
    <label class="chk"><input type="checkbox" id="lfTile" ${l.tile ? 'checked' : ''}> Show as a tile on Apps</label>
    <label>Group (optional)<input id="lfGroup" value="${esc(l.group || '')}" placeholder="Apps"></label>
    <p class="err" id="lfErr"></p>
    <div class="modal-actions"><button class="ghost-btn" data-c="0">Cancel</button><button class="primary-btn" data-c="1">Save</button></div>`);
  box.onclick = async (e) => {
    const b = e.target.closest('[data-c]'); if (!b) return;
    if (b.dataset.c === '0') return close();
    const n = (name || $('#lfName', box).value).trim().toLowerCase();
    const body = { url: $('#lfUrl', box).value.trim(), title: $('#lfTitle', box).value.trim(), tile: $('#lfTile', box).checked, group: $('#lfGroup', box).value.trim() };
    try { await api(`/v1/aliases/${encodeURIComponent(n)}`, { method: 'PUT', body }); close(); toast(`${aliasHost()}/${n} saved`); load(); }
    catch (err) { $('#lfErr', box).textContent = err.message; }
  };
}

function tileForm(t) {
  if (t.id.startsWith('link:')) { const k = t.id.slice(5); return linkForm(k, A.links[k]); }
  const { box, close } = H.modal(`<h3>${ic('edit')}${esc(t.title)}</h3>
    <p class="muted small">Overrides what the containers say (<code>og.*</code> labels). Leave a field empty to use the found value.</p>
    <label>Title<input id="tfTitle" placeholder="${esc(t.title)}"></label>
    <label>Address<input id="tfUrl" placeholder="${esc(t.url || 'https://…')}"></label>
    <label>Description<input id="tfDesc" placeholder="${esc(t.description || '')}"></label>
    <label>Group<input id="tfGroup" placeholder="${esc(t.group || 'Apps')}"></label>
    <label>Icon (an image URL)<input id="tfIcon" placeholder="${esc(t.icon || '')}"></label>
    <label>Extra aliases, comma-separated<input id="tfAlias" placeholder="${esc((t.aliases || []).slice(1).join(', '))}"></label>
    <label class="chk"><input type="checkbox" id="tfHidden" ${t.hidden ? 'checked' : ''}> Hide this tile</label>
    <p class="err" id="tfErr"></p>
    <div class="modal-actions"><button class="ghost-btn" data-c="r" title="Remove every override">Reset</button><span class="sp"></span><button class="ghost-btn" data-c="0">Cancel</button><button class="primary-btn" data-c="1">Save</button></div>`);
  box.onclick = async (e) => {
    const b = e.target.closest('[data-c]'); if (!b) return;
    if (b.dataset.c === '0') return close();
    const v = (id) => $(id, box).value.trim() || undefined;
    const body = b.dataset.c === 'r' ? {} : { title: v('#tfTitle'), url: v('#tfUrl'), description: v('#tfDesc'), group: v('#tfGroup'), icon: v('#tfIcon'), alias: v('#tfAlias'), hidden: $('#tfHidden', box).checked || (t.hidden ? false : undefined) };
    try { await api(`/v1/apps/${encodeURIComponent(t.id)}`, { method: 'PUT', body }); close(); load(); }
    catch (err) { $('#tfErr', box).textContent = err.message; }
  };
}

async function onClick(e) {
  const t = e.target.closest('[data-sub],[data-edit],[data-add],[data-ledit],[data-ldel]');
  if (!t) return;
  if (t.dataset.sub) { location.hash = t.dataset.sub === 'links' ? '#links' : '#myapps'; return; }
  e.preventDefault();
  if (t.dataset.edit) { const x = A.tiles.find((y) => y.id === t.dataset.edit); if (x) tileForm(x); return; }
  if (t.dataset.add) return linkForm(null);
  if (t.dataset.ledit) return linkForm(t.dataset.ledit, A.links[t.dataset.ledit]);
  if (t.dataset.ldel) {
    const k = t.dataset.ldel;
    if (!(await H.confirmBox(`Delete ${aliasHost()}/${k}?`, A.links[k].url, 'Delete'))) return;
    try { await api(`/v1/aliases/${encodeURIComponent(k)}`, { method: 'DELETE' }); load(); } catch (err) { fail(err); }
  }
}

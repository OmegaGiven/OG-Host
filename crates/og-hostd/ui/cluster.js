// Cluster: the nodes (live status), making / joining a cluster, adding nodes, sync jobs
// (backups copied to other sites), and the node switcher in the top bar: pick a node and
// the server pages manage that node (through this one).
let H, $, esc, ic, api, toast, S;

const C = { data: null, jobs: [], timer: null };
const fail = (e) => toast(e.message, true);
const rowBtn = (attr, icon, title, cls = '') => `<button class="row-btn ${cls}" ${attr} title="${esc(title)}">${ic(icon)}</button>`;

export function init(h) {
  H = h; ({ $, esc, ic, api, toast, S } = h);
  H.ROUTES.cluster = { el: 'cluster', tab: S.lab ? 'server' : 'cluster', run: () => load(), leave: () => clearInterval(C.timer) };
  $('#view-cluster').addEventListener('click', onClick);
  // the node switcher
  $('#hostChip').classList.add('switch');
  $('#hostChip').title = 'Switch node';
  $('#hostChip').addEventListener('click', async (e) => {
    e.preventDefault(); e.stopPropagation();
    let d;
    try { d = await api('/v1/cluster'); } catch { return; }
    if (!d.cluster) { location.hash = '#cluster'; return; }
    H.showMenu(e.currentTarget, [
      ...d.nodes.map((n) => [n.online ? 'server' : 'alert', `${n.name}${n.site ? ` · ${n.site}` : ''}${n.local ? ' (this one)' : ''}${n.online ? '' : ' · offline'}${(S.node || d.me) === n.name ? '  ✓' : ''}`, () => H.switchNode(n.local ? null : n.name)]),
      null, ['servers', 'Cluster…', () => (location.hash = '#cluster')],
    ]);
  });
  api('/v1/cluster').then((d) => { S.selfNode = d.me; S.inCluster = !!d.cluster; }).catch(() => {});
}

async function load() {
  try {
    const [d, jobs] = await Promise.all([api('/v1/cluster'), api('/v1/cluster/sync-jobs').catch(() => [])]);
    C.data = d; C.jobs = jobs;
    paint();
  } catch (e) { fail(e); }
  clearInterval(C.timer);
  C.timer = setInterval(async () => { if (document.hidden) return; try { C.data = await api('/v1/cluster'); paintNodes(); } catch { /* keep */ } }, 5000);
}

const canEdit = () => H.can('Cluster.Modify', '/nodes');

function paint() {
  const d = C.data;
  $('#view-cluster').innerHTML = `<div class="toolbar"><h1 class="inline">${d.cluster ? `Cluster ${esc(d.cluster.name)}` : 'Cluster'}</h1><span class="muted">${d.cluster ? `${d.nodes.length} nodes · cluster port ${d.port}` : 'this server is on its own'}</span><span class="sp"></span>
    ${canEdit() ? (d.cluster ? `<button class="primary-btn" id="clAdd">${ic('plus')}Add a node</button>` : `<button class="ghost-btn" id="clJoin">${ic('link')}Join a cluster</button><button class="primary-btn" id="clCreate">${ic('plus')}Make a cluster</button>`) : ''}</div>
    ${d.cluster ? '' : `<div class="panel set-card"><p class="muted">Link servers, at home or at other sites, into one cluster: manage them all from any of them, share accounts and permissions, and copy backups to another site so a stack can come back up there. Nodes talk over mutual TLS on port ${d.port}; each still runs its own containers and VMs.</p></div>`}
    <div class="node-grid" id="clNodes"></div>
    ${d.cluster ? `<div class="section-head"><h2>Sync jobs</h2><span class="muted">copy backups to a datastore on another node</span><span class="sp"></span>${canEdit() ? `<button class="link-btn" id="sjAdd">${ic('plus')} New sync job</button>` : ''}</div>
    <div class="panel table-wrap"><table class="table"><thead><tr><th>Job</th><th>When</th><th>From</th><th>To</th><th>Keep there</th><th></th></tr></thead><tbody id="sjRows"></tbody></table></div>` : ''}`;
  H.hydrateIcons($('#view-cluster'));
  paintNodes();
  paintJobs();
}

function paintNodes() {
  const el = $('#clNodes'); if (!el) return;
  el.innerHTML = C.data.nodes.map((n) => {
    const i = n.info || {};
    const memUsed = i.mem_total ? i.mem_total - i.mem_available : 0;
    const memPct = i.mem_total ? Math.round((memUsed / i.mem_total) * 100) : 0;
    const loadPct = i.cores ? Math.min(100, Math.round((i.load / i.cores) * 100)) : 0;
    const managing = (S.node || C.data.me) === n.name;
    return `<div class="card node-card ${n.online ? '' : 'offline'} ${managing ? 'sel' : ''}">
      <div class="ds-head"><span class="dot dot-${n.online ? 'ok' : 'crit'}"></span><b>${ic('server')} ${esc(n.name)}</b>${n.local ? '<span class="chip">this one</span>' : ''}${n.site ? `<span class="chip accent">${esc(n.site)}</span>` : ''}<span class="sp"></span>
        ${!n.local && canEdit() && C.data.cluster ? rowBtn(`data-node-rm="${esc(n.name)}"`, 'trash', 'Remove from the cluster', 'stop') : ''}</div>
      <small class="muted addr">${esc(n.addr || '')}${n.online ? '' : ` · ${esc(n.error || 'offline')}${n.last_seen ? `, last seen ${H.ago(n.last_seen)}` : ''}`}</small>
      ${n.online && i.cores ? `
        <div class="kv"><span>CPU</span><b>${i.load.toFixed(2)} load · ${i.cores} cores</b></div><div class="bar ${loadPct > 90 ? 'crit' : loadPct > 70 ? 'warn' : ''}"><i style="width:${loadPct}%"></i></div>
        <div class="kv"><span>Memory</span><b>${H.bytes(memUsed)} of ${H.bytes(i.mem_total)}</b></div><div class="bar ${memPct > 90 ? 'crit' : memPct > 75 ? 'warn' : ''}"><i style="width:${memPct}%"></i></div>
        <div class="kv"><span>Workloads</span><b>${i.containers_running}/${i.containers} containers · ${i.vms_running}/${i.vms} VMs</b></div>
        <div class="kv"><span>OG-Host</span><b>${esc(i.version)} · up ${H.dur(i.uptime)}</b></div>` : ''}
      ${n.online ? `<button class="ghost-btn" data-manage="${n.local ? '' : esc(n.name)}">${managing ? `${ic('check')} Managing` : `${ic('chevronr')} Manage`}</button>` : ''}
    </div>`;
  }).join('');
  H.hydrateIcons(el);
}

function paintJobs() {
  const el = $('#sjRows'); if (!el) return;
  const keep = (k) => Object.entries(k || {}).filter(([, v]) => v).map(([n, v]) => `${v} ${n}`).join(', ') || 'all';
  el.innerHTML = C.jobs.map(({ job: j, next }) => `<tr><td class="name"><b>${esc(j.id)}</b> ${j.enabled ? '' : '<span class="chip">off</span>'}<small>${esc(j.comment || '')}</small></td>
    <td>${esc(j.schedule)}<small class="muted block">${next ? `next ${new Date(next * 1000).toLocaleString()}` : ''}</small></td>
    <td>${esc(j.store)}${j.groups.length ? `<small class="muted block">${esc(j.groups.join(', '))}</small>` : ''}</td><td>${esc(j.remote_store)} on <b>${esc(j.node)}</b></td><td>${esc(keep(j.keep))}</td>
    <td>${canEdit() ? `<div class="row-actions">${rowBtn(`data-sj-run="${esc(j.id)}"`, 'play', 'Run now')}${rowBtn(`data-sj-edit="${esc(j.id)}"`, 'edit', 'Edit')}${rowBtn(`data-sj-del="${esc(j.id)}"`, 'trash', 'Remove', 'stop')}</div>` : ''}</td></tr>`).join('') || '<tr><td colspan="6" class="muted">No sync jobs: add one to keep copies of your backups at another site.</td></tr>';
  H.hydrateIcons(el);
}

function form(title, icon, inner, submit, ok = 'Save') {
  const { box, close } = H.modal(`<h3>${ic(icon)}${esc(title)}</h3><form class="form">${inner}<p class="err"></p><div class="modal-actions"><button type="button" class="ghost-btn" data-x>Cancel</button><button class="primary-btn">${esc(ok)}</button></div></form>`, { wide: true });
  box.querySelector('[data-x]').onclick = close;
  const f = box.querySelector('form');
  f.onsubmit = async (e) => {
    e.preventDefault();
    const b = f.querySelector('.primary-btn'); b.disabled = true;
    try { if ((await submit(Object.fromEntries(new FormData(f).entries()), f)) !== false) close(); } catch (err) { box.querySelector('.err').textContent = err.message; } finally { b.disabled = false; }
  };
  return box;
}

function syncModal(j) {
  const fresh = !j;
  j = j || { id: '', enabled: true, schedule: 'daily 04:00', store: 'local', node: '', remote_store: 'local', groups: [], keep: { last: 7, daily: 14, weekly: 8, monthly: 12 }, comment: '' };
  const others = C.data.nodes.filter((n) => !n.local);
  form(fresh ? 'New sync job' : `Sync job ${j.id}`, 'servers', `
    <div class="row2"><label>Name<input name="id" value="${esc(j.id)}" ${fresh ? 'required' : 'readonly'} pattern="[A-Za-z0-9_][A-Za-z0-9_\\-]*"></label><label>When<input name="schedule" value="${esc(j.schedule)}" required></label></div>
    <div class="row2"><label>From datastore (here)<input name="store" value="${esc(j.store)}" required></label>
    <label>To node<select name="node">${others.map((n) => `<option ${n.name === j.node ? 'selected' : ''}>${esc(n.name)}</option>`).join('')}</select></label></div>
    <div class="row2"><label>Datastore there<input name="remote_store" value="${esc(j.remote_store)}" required></label><label>Only these groups <span class="muted small">space separated, e.g. stack/immich ct/web; empty = all</span><input name="groups" value="${esc(j.groups.join(' '))}"></label></div>
    <fieldset class="keep"><legend>Keep there</legend>${['last', 'hourly', 'daily', 'weekly', 'monthly', 'yearly'].map((k) => `<label>${k}<input name="k:${k}" type="number" min="0" value="${j.keep[k] || 0}"></label>`).join('')}</fieldset>
    <p class="muted small">Encrypted datastores stay encrypted: the other site stores the backups but can't read them without the key (Backups → datastore → Show key).</p>
    <label>Comment<input name="comment" value="${esc(j.comment)}"></label>
    <label class="check"><input type="checkbox" name="enabled" ${j.enabled ? 'checked' : ''}> On</label>`, async (v, f) => {
    await api('/v1/cluster/sync-jobs', { method: 'POST', body: { id: v.id, schedule: v.schedule, store: v.store, node: v.node, remote_store: v.remote_store, groups: v.groups.split(/\s+/).filter(Boolean), comment: v.comment, enabled: f.enabled.checked, keep: Object.fromEntries(['last', 'hourly', 'daily', 'weekly', 'monthly', 'yearly'].map((k) => [k, +v[`k:${k}`] || 0])) } });
    toast('Saved'); load();
  });
}

async function onClick(e) {
  const on = (s) => e.target.closest(s);
  let el;
  if (on('#clCreate')) {
    form('Make a cluster', 'servers', `<div class="row2"><label>Cluster name<input name="name" required pattern="[A-Za-z0-9][A-Za-z0-9\\-]*" placeholder="home"></label><label>Site <span class="muted small">where this server is</span><input name="site" placeholder="home"></label></div>
      <label>This server's address for the others <span class="muted small">host:port</span><input name="addr" value="${esc(C.data.suggested_addr)}"></label>`, async (v) => {
      await api('/v1/cluster/create', { method: 'POST', body: v }); toast('Cluster made: now add nodes'); load();
    }, 'Make');
  } else if (on('#clAdd')) {
    const r = await api('/v1/cluster/token', { method: 'POST', body: { minutes: 60 } }).catch(fail);
    if (!r) return;
    const { box, close } = H.modal(`<h3>${ic('link')}Add a node</h3><p class="muted">On the new server open <b>Cluster → Join a cluster</b> and paste this (works once, for an hour). It carries this cluster's certificate fingerprint, so the new node can check it's really talking to this cluster.</p><pre class="secret">${esc(r.token)}</pre><p class="muted small">The new node's own accounts are replaced by the cluster's.</p><div class="modal-actions"><button class="ghost-btn" data-copy>${ic('copy')}Copy</button><button class="primary-btn" data-x>Done</button></div>`);
    box.querySelector('[data-x]').onclick = () => { close(); load(); };
    box.querySelector('[data-copy]').onclick = () => navigator.clipboard.writeText(r.token).then(() => toast('Copied'), fail);
  } else if (on('#clJoin')) {
    form('Join a cluster', 'link', `<label>Join token <span class="muted small">from a member: Cluster → Add a node</span><textarea name="token" rows="3" required></textarea></label>
      <div class="row2"><label>This server's address for the others<input name="addr" value="${esc(C.data.suggested_addr)}"></label><label>Site<input name="site" placeholder="office"></label></div>
      <p class="muted small">This server's accounts are replaced by the cluster's: you'll sign in again with a cluster account.</p>`, async (v) => {
      await api('/v1/cluster/join', { method: 'POST', body: v }); toast('Joined. Sign in again.'); setTimeout(() => location.reload(), 1500);
    }, 'Join');
  } else if ((el = on('[data-node-rm]'))) {
    if (await H.confirmBox(`Remove ${el.dataset.nodeRm} from the cluster?`, 'It stops being trusted by the other nodes. Its containers, VMs and data stay on it.', 'Remove')) api(`/v1/cluster/nodes/${encodeURIComponent(el.dataset.nodeRm)}`, { method: 'DELETE' }).then(load, fail);
  } else if ((el = on('[data-manage]'))) {
    H.switchNode(el.dataset.manage || null); location.hash = '#overview';
  } else if (on('#sjAdd')) syncModal(null);
  else if ((el = on('[data-sj-edit]'))) syncModal(C.jobs.find((x) => x.job.id === el.dataset.sjEdit).job);
  else if ((el = on('[data-sj-run]'))) api(`/v1/cluster/sync-jobs/${encodeURIComponent(el.dataset.sjRun)}/run`, { method: 'POST' }).then((r) => H.taskLog(r.task), fail);
  else if ((el = on('[data-sj-del]'))) { if (await H.confirmBox(`Remove sync job ${el.dataset.sjDel}?`, 'Backups already copied stay at the other site.', 'Remove')) api(`/v1/cluster/sync-jobs/${encodeURIComponent(el.dataset.sjDel)}`, { method: 'DELETE' }).then(load, fail); }
}

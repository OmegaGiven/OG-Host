// Network page (host bridges, bonds, VLANs; OG-Host networks), VM storage pools on the
// Storage page, and the VMs panel on the Overview.
let H, $, esc, ic, api, toast, S;

const N = { cfg: null, dirty: false, timer: null };
const fail = (e) => toast(e.message, true);
const rowBtn = (attr, icon, title, cls = '') => `<button class="row-btn ${cls}" ${attr} title="${esc(title)}">${ic(icon)}</button>`;

export function init(h) {
  H = h; ({ $, esc, ic, api, toast, S } = h);
  H.ROUTES.network = { el: 'network', tab: S.lab ? 'server' : 'network', run: () => loadNet(), leave: () => clearInterval(N.timer) };
  $('#view-network').addEventListener('click', onNetClick);
  // VM storage pools under Storage
  const st = H.ROUTES.storage;
  if (st) { const run = st.run; st.run = (a, p) => { run(a, p); loadPools(); }; }
  // VMs on the Overview
  const ov = H.ROUTES.overview;
  if (ov) { const run = ov.run; ov.run = (a, p) => { run(a, p); loadOvVms(); }; }
  setInterval(() => { if (!document.hidden && S.view === 'overview') loadOvVms(); }, 5000);
}

// ---------------------------------------------------------------- overview: VMs
async function loadOvVms() {
  const el = $('#ovVms'); if (!el) return;
  let list;
  try { list = await api('/v1/vms'); } catch { el.innerHTML = '<p class="muted">—</p>'; return; }
  const vms = list.filter((v) => !v.vm.template);
  $('#ovVmsMeta').textContent = `${vms.filter((v) => v.state === 'running').length} of ${vms.length} running`;
  el.innerHTML = vms.map((v) => `<a class="ov-vm" href="#vms/${encodeURIComponent(v.vm.name)}"><span class="dot dot-${v.state === 'running' ? 'ok' : v.state === 'paused' ? 'warn' : 'off'}"></span><b>${esc(v.vm.name)}</b><span class="muted">${esc(v.vm.os)}</span><span class="sp"></span><span class="num">${v.cpu != null ? `${v.cpu.toFixed(0)}%` : ''}</span><span class="num">${v.rss ? H.bytes(v.rss) : ''}</span></a>`).join('') || `<p class="muted">No VMs yet. <a href="#vms">Make one</a>: Linux, Windows, or anything that boots from an ISO.</p>`;
}

// ---------------------------------------------------------------- storage pools
async function loadPools() {
  let wrap = $('#poolWrap');
  if (!wrap) {
    wrap = document.createElement('div');
    wrap.id = 'poolWrap';
    $('#view-storage').insertBefore(wrap, $('#view-storage').querySelector('.two-col'));
    wrap.addEventListener('click', onPoolClick);
  }
  let pools;
  try { pools = await api('/v1/pools'); } catch (e) { wrap.innerHTML = ''; return; }
  const edit = H.can('Datastore.Allocate', '/storage');
  wrap.innerHTML = `<div class="section-head"><h2>VM storage</h2><span class="muted">pools for virtual machine disks</span><span class="sp"></span>${edit ? `<button class="link-btn" id="poolImport">${ic('download')} Import a disk image</button><button class="link-btn" id="poolAdd">${ic('plus')} Add pool</button>` : ''}</div>
    <div class="ds-grid">${pools.map(({ pool: p, total, used }) => {
      const pct = total ? Math.round((used / total) * 100) : 0;
      const where = p.type === 'dir' ? p.path : p.type === 'lvmthin' ? `LVM-thin ${p.vg}/${p.thinpool}` : `ZFS ${p.dataset}`;
      return `<div class="card ds-card" data-pool="${esc(p.id)}"><div class="ds-head"><b>${ic(p.type === 'dir' ? 'folder' : 'hdd')} ${esc(p.id)}</b><span class="chip">${esc(p.type)}</span><span class="sp"></span>${edit && p.id !== 'local' ? rowBtn(`data-pool-rm="${esc(p.id)}"`, 'trash', 'Forget pool', 'stop') : ''}</div>
        <small class="muted addr">${esc(where)}</small>
        ${total ? `<div class="bar ${pct > 90 ? 'crit' : pct > 75 ? 'warn' : ''}"><i style="width:${pct}%"></i></div><small class="muted">${H.bytes(used)} used of ${H.bytes(total)}</small>` : ''}
        <button class="link-btn" data-pool-vols="${esc(p.id)}">${ic('layers')} Volumes</button></div>`;
    }).join('')}</div>`;
  H.hydrateIcons(wrap);
}

async function onPoolClick(e) {
  const on = (s) => e.target.closest(s);
  let el;
  if (on('#poolAdd')) {
    const { box, close } = H.modal(`<h3>${ic('hdd')}Add a VM storage pool</h3><form class="form" id="plForm">
      <div class="row2"><label>Name<input name="id" required pattern="[A-Za-z0-9_][A-Za-z0-9_\\-]*"></label>
      <label>Type<select name="type"><option value="dir">Directory (qcow2 files)</option><option value="lvmthin">LVM thin pool</option><option value="zfs">ZFS (zvols)</option></select></label></div>
      <label data-t="dir">Directory<input name="path" placeholder="/mnt/fast-ssd/vms"></label>
      <div class="row2" data-t="lvmthin"><label>Volume group<input name="vg" placeholder="pve"></label><label>Thin pool<input name="thinpool" placeholder="data"></label></div>
      <label data-t="zfs">Dataset<input name="dataset" placeholder="rpool/data"></label>
      <label>Comment<input name="comment"></label><p class="err"></p>
      <div class="modal-actions"><button type="button" class="ghost-btn" data-x>Cancel</button><button class="primary-btn">Add</button></div></form>`, { wide: true });
    const f = box.querySelector('form');
    const sync = () => { for (const x of box.querySelectorAll('[data-t]')) x.hidden = x.dataset.t !== f.type.value; };
    f.type.onchange = sync; sync();
    box.querySelector('[data-x]').onclick = close;
    f.onsubmit = async (ev) => { ev.preventDefault(); try { await api('/v1/pools', { method: 'POST', body: Object.fromEntries(new FormData(f).entries()) }); close(); toast('Pool added'); loadPools(); } catch (err) { box.querySelector('.err').textContent = err.message; } };
  } else if ((el = on('[data-pool-rm]'))) {
    if (await H.confirmBox(`Forget pool ${el.dataset.poolRm}?`, 'Its volumes stay where they are.', 'Forget')) api(`/v1/pools/${encodeURIComponent(el.dataset.poolRm)}`, { method: 'DELETE' }).then(loadPools, fail);
  } else if ((el = on('[data-pool-vols]'))) {
    const id = el.dataset.poolVols;
    const vols = await api(`/v1/pools/${encodeURIComponent(id)}/volumes`).catch((er) => { fail(er); return []; });
    const { box } = H.modal(`<h3>${ic('layers')}Volumes in ${esc(id)}</h3><div class="panel table-wrap"><table class="table"><tbody>${vols.map((v) => `<tr><td class="addr">${esc(v.volume)}</td><td class="num">${v.size ? H.bytes(v.size) : ''}${v.allocated ? ` <small class="muted">${H.bytes(v.allocated)} used</small>` : ''}</td><td>${v.vm ? `<a href="#vms/${encodeURIComponent(v.vm)}">${esc(v.vm)}</a>` : '<span class="muted">unused</span>'}</td><td>${!v.vm && H.can('Datastore.Allocate', '/storage') ? rowBtn(`data-vol-rm="${esc(v.volume)}"`, 'trash', 'Delete', 'stop') : ''}</td></tr>`).join('') || '<tr><td class="muted">Empty.</td></tr>'}</tbody></table></div>`, { wide: true });
    box.onclick = async (ev) => {
      const b = ev.target.closest('[data-vol-rm]'); if (!b) return;
      const [p, n] = b.dataset.volRm.split(':');
      if (await H.confirmBox(`Delete ${b.dataset.volRm}?`, 'The disk image is deleted for good.', 'Delete')) api(`/v1/pools/${encodeURIComponent(p)}/volumes/${encodeURIComponent(n)}`, { method: 'DELETE' }).then(() => { b.closest('tr').remove(); toast('Deleted'); }, fail);
    };
  } else if (on('#poolImport')) {
    const pools = (await api('/v1/pools')).map((x) => x.pool.id);
    const { box, close } = H.modal(`<h3>${ic('download')}Import a disk image</h3><form class="form">
      <p class="muted">A cloud image (Debian, Ubuntu, Fedora… "generic cloud" qcow2) becomes a volume; make a VM with it as its disk and cloud-init sets it up. Also takes raw, VMDK and VHDX disks from other hypervisors.</p>
      <label>Download from<input name="url" placeholder="https://cloud.debian.org/images/cloud/trixie/latest/debian-13-genericcloud-amd64.qcow2"></label>
      <div class="row2"><label>Into pool<select name="pool">${pools.map((p) => `<option>${esc(p)}</option>`).join('')}</select></label><label>Volume name<input name="name" required placeholder="debian-13-cloud.qcow2"></label></div>
      <label>Grow to (GiB) <span class="muted small">optional</span><input name="grow" type="number" min="1"></label><p class="err"></p>
      <div class="modal-actions"><button type="button" class="ghost-btn" data-x>Cancel</button><button class="primary-btn">Import</button></div></form>`, { wide: true });
    box.querySelector('[data-x]').onclick = close;
    box.querySelector('form').onsubmit = async (ev) => {
      ev.preventDefault();
      const v = Object.fromEntries(new FormData(ev.target).entries());
      try { const r = await api(`/v1/pools/${encodeURIComponent(v.pool)}/import`, { method: 'POST', body: { url: v.url.trim(), name: v.name.trim(), grow: v.grow ? +v.grow : null } }); close(); H.taskLog(r.task, () => loadPools()); } catch (err) { box.querySelector('.err').textContent = err.message; }
    };
  }
}

// ---------------------------------------------------------------- network page
async function loadNet() {
  let st;
  try { st = await api('/v1/network'); } catch (e) { $('#view-network').innerHTML = `<p class="empty">${esc(e.message)}</p>`; return; }
  if (!N.dirty || !N.cfg) N.cfg = structuredClone(st.config);
  const edit = H.can('Network.Modify', '/system');
  const nets = await api('/v1/docker/networks').catch(() => []);
  $('#view-network').innerHTML = `<div class="toolbar"><h1 class="inline">Network</h1><span class="sp"></span></div>
    ${st.pending ? `<div class="status-banner warn fw-pending"><b>${ic('alert')} New network settings are on trial.</b> They go back in <b id="netLeft">${st.pending.seconds_left}</b> s unless you keep them (you're seeing this page, so the server is reachable). <span class="sp"></span><button class="ghost-btn" id="netUndo">Undo now</button><button class="primary-btn" id="netKeep">${ic('check')}Keep</button></div>` : ''}
    ${st.networkd ? '' : '<div class="status-banner warn">This server\'s network isn\'t run by systemd-networkd, so OG-Host can show it but not change it.</div>'}
    <div class="section-head first"><h2>Interfaces</h2><span class="muted">what the server has now</span></div>
    <div class="panel table-wrap"><table class="table"><thead><tr><th>Name</th><th>Kind</th><th>Addresses</th><th>Link</th><th>Member of</th></tr></thead><tbody>${st.links.map((l) => `<tr><td><b>${esc(l.name)}</b><small class="muted block addr">${esc(l.mac)}</small></td><td>${esc(l.kind)}</td><td class="addr">${esc((l.addresses || []).join(', ')) || '<span class="muted">—</span>'}</td><td>${l.state === 'up' ? '<span class="chip good">up</span>' : `<span class="chip">${esc(l.state)}</span>`}${l.speed ? ` <span class="muted small">${l.speed >= 1000 ? `${l.speed / 1000} Gb/s` : `${l.speed} Mb/s`}</span>` : ''}</td><td>${l.master ? esc(l.master) : ''}</td></tr>`).join('')}</tbody></table></div>
    <div class="section-head"><h2>Bridges</h2><span class="muted">VMs straight on your LAN (like Proxmox's vmbr0)</span><span class="sp"></span>${edit ? `<button class="link-btn" data-add="bridge">${ic('plus')} Bridge</button><button class="link-btn" data-add="bond">${ic('plus')} Bond</button><button class="link-btn" data-add="vlan">${ic('plus')} VLAN</button>` : ''}</div>
    <div class="panel table-wrap"><table class="table"><tbody>
      ${N.cfg.bridges.map((b, i) => `<tr><td><b>${esc(b.name)}</b> ${b.vlan_aware ? '<span class="chip">VLAN-aware</span>' : ''}<small class="muted block">${esc(b.comment || '')}</small></td><td>ports: ${esc(b.ports.join(', ') || 'none')}</td><td>${addrTxt(b.addr)}</td><td>${edit ? rowBtn(`data-edit="bridges:${i}"`, 'edit', 'Edit') + rowBtn(`data-del="bridges:${i}"`, 'trash', 'Remove', 'stop') : ''}</td></tr>`).join('')}
      ${N.cfg.bonds.map((b, i) => `<tr><td><b>${esc(b.name)}</b> <span class="chip">bond</span></td><td>${esc(b.mode)} of ${esc(b.slaves.join(', '))}</td><td>${addrTxt(b.addr)}</td><td>${edit ? rowBtn(`data-edit="bonds:${i}"`, 'edit', 'Edit') + rowBtn(`data-del="bonds:${i}"`, 'trash', 'Remove', 'stop') : ''}</td></tr>`).join('')}
      ${N.cfg.vlans.map((v, i) => `<tr><td><b>${esc(v.name)}</b> <span class="chip">VLAN ${v.id}</span></td><td>on ${esc(v.parent)}</td><td>${addrTxt(v.addr)}</td><td>${edit ? rowBtn(`data-edit="vlans:${i}"`, 'edit', 'Edit') + rowBtn(`data-del="vlans:${i}"`, 'trash', 'Remove', 'stop') : ''}</td></tr>`).join('')}
      ${N.cfg.bridges.length + N.cfg.bonds.length + N.cfg.vlans.length ? '' : '<tr><td class="muted">None yet: VMs use OG-Host networks (below) or a NIC\'s LAN directly.</td></tr>'}
    </tbody></table></div>
    ${edit && N.dirty ? `<div class="toolbar"><span class="chip warn">Unsaved changes</span><span class="sp"></span><button class="ghost-btn" id="netReset">Discard</button><button class="primary-btn" id="netApply">${ic('check')}Apply</button></div><p class="muted small">Applying takes effect at once; if the server can't be reached afterwards it goes back by itself after 90 s.</p>` : ''}
    <div class="section-head"><h2>OG-Host networks</h2><span class="muted">private networks for containers and VMs (NAT, published ports, DHCP for VMs)</span></div>
    <div class="panel table-wrap"><table class="table"><tbody>${nets.map((n) => `<tr><td><b>${esc(n.Name)}</b></td><td class="addr">${esc(n.IPAM?.Config?.[0]?.Subnet || '–')}</td><td class="muted">${esc(n.Options?.['og-host.bridge'] || n.Driver)}</td></tr>`).join('')}</tbody></table></div>
    <details class="fw-script"><summary class="muted small">systemd-networkd files</summary>${st.files.map((f) => `<p class="muted small addr">${esc(f.name)}</p><pre class="secret">${esc(f.content)}</pre>`).join('')}</details>`;
  H.hydrateIcons($('#view-network'));
  clearInterval(N.timer);
  if (st.pending) {
    let left = st.pending.seconds_left;
    N.timer = setInterval(() => { left -= 1; const el = $('#netLeft'); if (!el || left < 0) { clearInterval(N.timer); if (el) loadNet(); return; } el.textContent = left; }, 1000);
  }
}

const addrTxt = (a) => (a.mode === 'dhcp' ? 'DHCP' : a.mode === 'static' ? `<span class="addr">${esc(a.cidr)}</span>${a.gateway ? ` via ${esc(a.gateway)}` : ''}` : '<span class="muted">no address</span>');

function addrForm(a = { mode: 'none', cidr: '', gateway: '', dns: [] }) {
  return `<div class="row2"><label>Address<select name="mode"><option value="none" ${a.mode === 'none' ? 'selected' : ''}>none</option><option value="dhcp" ${a.mode === 'dhcp' ? 'selected' : ''}>DHCP</option><option value="static" ${a.mode === 'static' ? 'selected' : ''}>static</option></select></label>
    <label>Static address<input name="cidr" value="${esc(a.cidr || '')}" placeholder="192.168.1.10/24"></label></div>
    <div class="row2"><label>Gateway<input name="gateway" value="${esc(a.gateway || '')}"></label><label>DNS servers<input name="dns" value="${esc((a.dns || []).join(' '))}" placeholder="1.1.1.1 9.9.9.9"></label></div>`;
}
const readAddr = (v) => ({ mode: v.mode, cidr: (v.cidr || '').trim(), gateway: (v.gateway || '').trim(), dns: (v.dns || '').split(/\s+/).filter(Boolean) });

function editItem(kind, idx) {
  const isNew = idx == null;
  const item = isNew ? null : N.cfg[kind][idx];
  const nics = (N.links || []).filter((l) => l.kind === 'nic').map((l) => l.name);
  let inner = '';
  if (kind === 'bridges') {
    const b = item || { name: `vmbr${N.cfg.bridges.length}`, ports: [], vlan_aware: true, vids: [], addr: { mode: 'dhcp' }, mtu: 0, comment: '' };
    inner = `<div class="row2"><label>Name<input name="name" value="${esc(b.name)}" required></label><label>Ports <span class="muted small">NICs, bonds or VLANs</span><input name="ports" value="${esc(b.ports.join(' '))}" placeholder="eth0"></label></div>
      <label class="check"><input type="checkbox" name="vlan_aware" ${b.vlan_aware ? 'checked' : ''}> VLAN-aware (VMs can be put on VLANs)</label>${addrForm(b.addr)}
      <div class="row2"><label>MTU <span class="muted small">0 = default</span><input name="mtu" type="number" value="${b.mtu}"></label><label>Comment<input name="comment" value="${esc(b.comment)}"></label></div>
      <p class="muted small">Moving the server's own NIC into a bridge moves its address too: give the bridge the address the NIC had (DHCP keeps the same one).</p>`;
  } else if (kind === 'bonds') {
    const b = item || { name: `bond${N.cfg.bonds.length}`, slaves: [], mode: 'active-backup', mtu: 0, addr: { mode: 'none' } };
    inner = `<div class="row2"><label>Name<input name="name" value="${esc(b.name)}" required></label><label>Mode<select name="bmode">${['active-backup', '802.3ad', 'balance-alb', 'balance-xor', 'balance-rr'].map((m) => `<option ${m === b.mode ? 'selected' : ''}>${m}</option>`).join('')}</select></label></div>
      <label>Network cards<input name="slaves" value="${esc(b.slaves.join(' '))}" placeholder="${esc(nics.join(' '))}"></label>${addrForm(b.addr)}`;
  } else {
    const v = item || { name: '', parent: nics[0] || '', id: 10, addr: { mode: 'none' } };
    inner = `<div class="row2"><label>Name<input name="name" value="${esc(v.name)}" placeholder="vlan10" required></label><label>VLAN id<input name="id" type="number" min="1" max="4094" value="${v.id}"></label></div><label>On<input name="parent" value="${esc(v.parent)}"></label>${addrForm(v.addr)}`;
  }
  const { box, close } = H.modal(`<h3>${ic('net')}${isNew ? 'Add' : 'Edit'} ${kind.slice(0, -1)}</h3><form class="form">${inner}<div class="modal-actions"><button type="button" class="ghost-btn" data-x>Cancel</button><button class="primary-btn">Done</button></div></form>`, { wide: true });
  box.querySelector('[data-x]').onclick = close;
  box.querySelector('form').onsubmit = (ev) => {
    ev.preventDefault();
    const f = ev.target;
    const v = Object.fromEntries(new FormData(f).entries());
    const list = (s) => (s || '').split(/[\s,]+/).filter(Boolean);
    let obj;
    if (kind === 'bridges') obj = { name: v.name.trim(), ports: list(v.ports), vlan_aware: f.vlan_aware.checked, vids: item?.vids || [], addr: readAddr(v), mtu: +v.mtu || 0, comment: v.comment };
    else if (kind === 'bonds') obj = { name: v.name.trim(), slaves: list(v.slaves), mode: v.bmode, mtu: item?.mtu || 0, addr: readAddr(v), comment: '' };
    else obj = { name: v.name.trim(), parent: v.parent.trim(), id: +v.id, addr: readAddr(v), comment: '' };
    if (isNew) N.cfg[kind].push(obj); else N.cfg[kind][idx] = obj;
    N.dirty = true; close(); loadNet();
  };
}

async function onNetClick(e) {
  const on = (s) => e.target.closest(s);
  let el;
  if ((el = on('[data-add]'))) { N.links = (await api('/v1/network')).links; editItem(`${el.dataset.add}s`, null); }
  else if ((el = on('[data-edit]'))) { N.links = (await api('/v1/network')).links; const [k, i] = el.dataset.edit.split(':'); editItem(k, +i); }
  else if ((el = on('[data-del]'))) { const [k, i] = el.dataset.del.split(':'); N.cfg[k].splice(+i, 1); N.dirty = true; loadNet(); }
  else if (on('#netReset')) { N.dirty = false; N.cfg = null; loadNet(); }
  else if (on('#netApply')) {
    try { await api('/v1/network', { method: 'PUT', body: N.cfg }); N.dirty = false; toast('Applied: check everything still works, then Keep'); setTimeout(loadNet, 2500); } catch (err) { fail(err); }
  } else if (on('#netKeep')) api('/v1/network/confirm', { method: 'POST' }).then(() => { toast('Network settings kept'); loadNet(); }, fail);
  else if (on('#netUndo')) api('/v1/network/rollback', { method: 'POST' }).then(() => { toast('Put back the previous settings'); N.cfg = null; setTimeout(loadNet, 2500); }, fail);
}

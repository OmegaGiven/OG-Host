// Virtual machines: list, create (with ISO upload), one VM's page (summary, console,
// hardware, snapshots, backups), power actions.
let H, $, esc, ic, api, toast, S;

const V = { list: [], host: null, cur: null, tab: 'summary', vnc: null, timer: null };
const fail = (e) => toast(e.message, true);
const rowBtn = (attr, icon, title, cls = '') => `<button class="row-btn ${cls}" ${attr} title="${esc(title)}">${ic(icon)}</button>`;
const can = (p, name) => H.can(p, '/vms') || (name && H.can(p, '*'));
const stateCls = (s) => (s === 'running' ? 'ok' : s === 'paused' ? 'warn' : s === 'starting' ? 'busy' : 'off');

export function init(h) {
  H = h; ({ $, esc, ic, api, toast, S } = h);
  H.ROUTES.vms = { el: 'vms', tab: S.lab ? 'server' : 'vms', run: (arg) => (arg ? openVm(decodeURIComponent(arg)) : openList()), leave: leave };
  $('#view-vms').addEventListener('click', onClick);
  H.paletteItems = ((prev) => (q) => [...(prev ? prev(q) : []), ...V.list.map((v) => ({ icon: 'monitor', label: v.vm.name, hint: `VM · ${v.state}`, go: () => (location.hash = `#vms/${encodeURIComponent(v.vm.name)}`) }))])(H.paletteItems);
}

function leave() {
  clearInterval(V.timer);
  if (V.vnc) { V.vnc.close(); V.vnc = null; }
}

async function refresh() {
  try { V.list = await api('/v1/vms'); } catch (e) { fail(e); }
}

// ---------------------------------------------------------------- list
async function openList() {
  leave();
  V.cur = null;
  $('#view-vms').innerHTML = `<div class="toolbar"><h1 class="inline">Virtual machines</h1><span class="muted" id="vmMeta"></span><span class="sp"></span>${can('VM.Allocate') ? `<button class="primary-btn" id="vmNew">${ic('plus')}New VM</button>` : ''}</div>
    <div id="vmHostWarn"></div>
    <div class="panel table-wrap"><table class="table"><thead><tr><th>Name</th><th>OS</th><th class="num">CPU</th><th class="num">Memory</th><th>Address</th><th></th></tr></thead><tbody id="vmRows"></tbody></table></div>`;
  H.hydrateIcons($('#view-vms'));
  const [_, host] = await Promise.all([refresh(), api('/v1/vms/host').catch(() => null)]);
  V.host = host;
  if (host && (!host.kvm || !host.qemu)) $('#vmHostWarn').innerHTML = `<div class="status-banner warn">${ic('alert')} ${!host.kvm ? 'This server has no hardware virtualization (/dev/kvm): turn on VT-x/AMD-V in the BIOS.' : 'QEMU is not installed.'}</div>`;
  paintList();
  V.timer = setInterval(async () => { if (document.hidden) return; await refresh(); paintList(); }, 3000);
}

function paintList() {
  const el = $('#vmRows'); if (!el) return;
  $('#vmMeta').textContent = `${V.list.length} VMs · ${V.list.filter((v) => v.state === 'running').length} running`;
  el.innerHTML = V.list.map((v) => {
    const vm = v.vm;
    const ip = v.nics.map((n) => n.ip || (n.network.startsWith('lan:') ? 'LAN (DHCP)' : '')).filter(Boolean).join(', ');
    return `<tr class="click" data-vm="${esc(vm.name)}">
      <td class="name"><span class="dot dot-${stateCls(v.state)}"></span> <b>${esc(vm.name)}</b> ${vm.realistic ? '<span class="chip" title="Looks like a real PC to the guest">real PC</span>' : ''}${vm.autostart ? '<span class="chip">autostart</span>' : ''}<small>${esc(v.state)}${vm.comment ? ` · ${esc(vm.comment)}` : ''}</small></td>
      <td>${esc(vm.os)} · ${esc(vm.firmware.toUpperCase())}${vm.tpm ? ' · TPM' : ''}</td>
      <td class="num">${v.cpu != null ? `${v.cpu.toFixed(0)}%` : ''} <small class="muted">${vm.cores} vCPU</small></td>
      <td class="num">${v.rss ? H.bytes(v.rss) : ''} <small class="muted">${H.bytes(vm.memory * 1048576)}</small></td>
      <td class="addr">${esc(ip)}</td>
      <td><div class="row-actions">${powerBtns(v)}</div></td></tr>`;
  }).join('') || `<tr><td colspan="6" class="muted">No virtual machines yet.${can('VM.Allocate') ? ' Make one with <b>New VM</b>: Linux, Windows (UEFI, Secure Boot, TPM 2.0) or anything that boots from an ISO.' : ''}</td></tr>`;
  H.hydrateIcons(el);
}

function powerBtns(v) {
  if (!can('VM.PowerMgmt', v.vm.name)) return '';
  const n = esc(v.vm.name);
  if (v.state === 'stopped') return rowBtn(`data-vact="start" data-n="${n}"`, 'play', 'Start');
  return `${rowBtn(`data-vconsole="${n}"`, 'monitor', 'Console')}${v.state === 'paused' ? rowBtn(`data-vact="resume" data-n="${n}"`, 'play', 'Resume') : ''}${rowBtn(`data-vact="shutdown" data-n="${n}"`, 'stop', 'Shut down')}${rowBtn(`data-vmenu="${n}"`, 'more', 'More')}`;
}

async function vmAction(act, name) {
  try {
    const r = await api(`/v1/vms/${encodeURIComponent(name)}/${act}`, { method: 'POST' });
    if (r && r.task) H.taskLog ? H.taskLog(r.task, () => reload()) : toast('Started');
    else toast(`${name}: ${act}`);
    reload();
  } catch (e) { fail(e); }
}

async function reload() {
  await refresh();
  if (V.cur) paintVm(); else paintList();
}

// ---------------------------------------------------------------- one VM
async function openVm(name, tab) {
  leave();
  V.cur = name;
  if (tab) V.tab = tab;
  await refresh();
  if (!V.list.find((v) => v.vm.name === name)) { toast(`No VM ${name}`, true); location.hash = '#vms'; return; }
  if (!V.host) V.host = await api('/v1/vms/host').catch(() => null);
  paintVm();
  V.timer = setInterval(async () => { if (document.hidden || V.tab === 'console') return; await refresh(); if (V.tab === 'summary') paintVm(true); }, 3000);
}

function cur() { return V.list.find((v) => v.vm.name === V.cur); }

function paintVm(soft) {
  const v = cur(); if (!v) return;
  const vm = v.vm;
  const tabs = [['summary', 'Summary'], ['console', 'Console'], ['hardware', 'Hardware'], ['snapshots', 'Snapshots'], ['backups', 'Backups']];
  if (!soft || !$('#vmBody')) {
    $('#view-vms').innerHTML = `<div class="toolbar"><a class="link-btn" href="#vms">${ic('chevronr')} VMs</a><h1 class="inline"><span class="dot dot-${stateCls(v.state)}" id="vmDot"></span> ${esc(vm.name)}</h1><span class="muted" id="vmState">${esc(v.state)}</span><span class="sp"></span><div class="row-actions" id="vmPower"></div></div>
      <div class="seg seg-sm set-tabs" id="vmTabs">${tabs.map(([id, l]) => `<button data-vt="${id}" class="${id === V.tab ? 'on' : ''}">${esc(l)}</button>`).join('')}</div>
      <div id="vmBody"></div>`;
    paintTab();
  } else if (V.tab === 'summary') paintTab();
  $('#vmDot').className = `dot dot-${stateCls(v.state)}`;
  $('#vmState').textContent = v.state;
  $('#vmPower').innerHTML = powerBtns(v).replace(`data-vconsole="${esc(vm.name)}"`, 'data-vt="console"');
  H.hydrateIcons($('#view-vms'));
}

function paintTab() {
  if (V.vnc && V.tab !== 'console') { V.vnc.close(); V.vnc = null; }
  segOn();
  const fn = { summary: tabSummary, console: tabConsole, hardware: tabHardware, snapshots: tabSnaps, backups: tabBackups }[V.tab];
  fn().catch(fail);
}

function segOn() { for (const b of document.querySelectorAll('#vmTabs button')) b.classList.toggle('on', b.dataset.vt === V.tab); }

const kv = (k, v) => `<div class="kv"><span>${esc(k)}</span><b>${v}</b></div>`;

async function tabSummary() {
  const v = cur(); const vm = v.vm;
  $('#vmBody').innerHTML = `<div class="set-grid">
    <div class="panel set-card"><h3>${ic('cpu')}Machine</h3>
      ${kv('State', esc(v.state))}${kv('CPU', `${v.cpu != null ? `${v.cpu.toFixed(0)}% of ` : ''}${vm.cores} vCPU${vm.cpu_limit ? ` (capped at ${vm.cpu_limit})` : ''}`)}
      ${kv('Memory', `${v.rss ? `${H.bytes(v.rss)} used of ` : ''}${H.bytes(vm.memory * 1048576)}${vm.balloon ? ` (balloon to ${H.bytes(vm.balloon * 1048576)})` : ''}`)}
      ${kv('Firmware', `${vm.firmware.toUpperCase()}${vm.secure_boot ? ' · Secure Boot' : ''}${vm.tpm ? ' · TPM 2.0' : ''}`)}
      ${kv('OS', esc(vm.os))}${vm.realistic ? kv('Looks like', `${esc(vm.smbios.board_manufacturer)} ${esc(vm.smbios.board_product)}`) : ''}
      ${kv('Autostart', vm.autostart ? 'yes' : 'no')}</div>
    <div class="panel set-card"><h3>${ic('net')}Network</h3>${v.nics.map((n) => kv(n.id, `${esc(n.network)} · <span class="addr">${esc(n.ip || (n.network.startsWith('lan:') ? 'address from the LAN' : 'no address yet'))}</span>${n.ports.length ? ` · ports ${esc(n.ports.join(', '))}` : ''}<small class="muted block addr">${esc(n.mac)}</small>`)).join('') || '<p class="muted">No network card.</p>'}</div>
    <div class="panel set-card"><h3>${ic('hdd')}Disks</h3>${v.disks.map((d) => kv(d.id, `${esc(d.bus)} · ${d.allocated != null ? `${H.bytes(d.allocated)} used` : ''}<small class="muted block addr">${esc(d.file)}</small>`)).join('') || '<p class="muted">No disks.</p>'}
      ${vm.cdroms.map((c, i) => kv(`cdrom${i}`, c ? esc(c) : '<span class="muted">empty</span>')).join('')}</div>
  </div>
  ${vm.comment ? `<p class="muted">${esc(vm.comment)}</p>` : ''}
  <div class="section-head"><h2>Serial console output</h2><span class="sp"></span><button class="link-btn" id="vmLogRefresh">${ic('refresh')} Refresh</button></div>
  <pre class="task-log" id="vmLog"></pre>`;
  H.hydrateIcons($('#vmBody'));
  loadSerial();
}

async function loadSerial() {
  try {
    const r = await fetch(`/v1/vms/${encodeURIComponent(V.cur)}/console?bytes=20000`, { headers: { 'X-OG-UI': '1' } });
    const t = await r.text();
    const el = $('#vmLog'); if (!el) return;
    el.textContent = t.replace(/\x1b\[[0-9;?]*[A-Za-z]/g, '').replace(/\r/g, '') || '(nothing yet)';
    el.scrollTop = el.scrollHeight;
  } catch { /* gone */ }
}

async function tabConsole() {
  const v = cur();
  if (v.state === 'stopped') {
    $('#vmBody').innerHTML = `<div class="empty">${ic('monitor')} The VM is off.${can('VM.PowerMgmt', V.cur) ? ` <button class="primary-btn" data-vact="start" data-n="${esc(V.cur)}">${ic('play')}Start</button>` : ''}</div>`;
    H.hydrateIcons($('#vmBody'));
    return;
  }
  if (v.vm.vga === 'none') { $('#vmBody').innerHTML = '<div class="empty">This VM has no display (a passed-through GPU drives its screen). See the serial output on Summary.</div>'; return; }
  $('#vmBody').innerHTML = `<div class="vnc-bar"><span class="muted" id="vncState">connecting…</span><span class="sp"></span>
    <button class="ghost-btn" id="vncCad">Ctrl+Alt+Del</button><button class="ghost-btn" id="vncType">${ic('copy')}Type text…</button><button class="ghost-btn" id="vncFull">${ic('expand')}Full screen</button><button class="ghost-btn" id="vncRe">${ic('refresh')}Reconnect</button></div>
    <div class="vnc-wrap" id="vncWrap"><canvas id="vncCanvas" class="vnc"></canvas></div>
    <p class="muted small">Click the screen to type into the VM. Keys go as scancodes, so the guest's own keyboard layout applies.</p>`;
  H.hydrateIcons($('#vmBody'));
  const { Vnc } = await import('./vnc.js');
  const connect = () => {
    if (V.vnc) V.vnc.close();
    const proto = location.protocol === 'https:' ? 'wss' : 'ws';
    V.vnc = new Vnc($('#vncCanvas'), `${proto}://${location.host}/v1/vms/${encodeURIComponent(V.cur)}/vnc`, { onstate: (s) => { const el = $('#vncState'); if (el) el.textContent = s; } });
    setTimeout(() => $('#vncCanvas')?.focus(), 300);
  };
  connect();
  $('#vncRe').onclick = connect;
  $('#vncCad').onclick = () => V.vnc && V.vnc.ctrlAltDel();
  $('#vncFull').onclick = () => $('#vncWrap').requestFullscreen?.().then(() => $('#vncCanvas').focus()).catch(() => {});
  $('#vncType').onclick = () => {
    const { box, close } = H.modal(`<h3>${ic('copy')}Type into the VM</h3><form class="form"><p class="muted">Sent as key presses with a US layout (for a password or a long command).</p><textarea name="t" rows="3"></textarea><div class="modal-actions"><button type="button" class="ghost-btn" data-x>Cancel</button><button class="primary-btn">Type</button></div></form>`);
    box.querySelector('[data-x]').onclick = close;
    box.querySelector('form').onsubmit = (e) => { e.preventDefault(); const t = e.target.t.value; close(); V.vnc && V.vnc.type(t).then(() => $('#vncCanvas').focus()); };
  };
}

async function tabHardware() {
  const v = cur(); const vm = v.vm;
  const edit = can('VM.Config', V.cur);
  const off = v.state === 'stopped';
  $('#vmBody').innerHTML = `${!off ? '<p class="muted small">Stop the VM to change its hardware; the disc and the comment can change while it runs.</p>' : ''}
  <div class="panel table-wrap"><table class="table"><tbody>
    <tr><td>Processor</td><td>${vm.cores} vCPU (host CPU model)${vm.cpu_limit ? ` · capped at ${vm.cpu_limit} cores` : ''}</td><td>${edit && off ? rowBtn('data-hw="cpu"', 'edit', 'Edit') : ''}</td></tr>
    <tr><td>Memory</td><td>${H.bytes(vm.memory * 1048576)}${vm.balloon ? ` · balloon down to ${H.bytes(vm.balloon * 1048576)}` : ''}</td><td>${edit && off ? rowBtn('data-hw="cpu"', 'edit', 'Edit') : ''}</td></tr>
    <tr><td>Firmware</td><td>${vm.firmware.toUpperCase()}${vm.secure_boot ? ' · Secure Boot' : ''}${vm.tpm ? ' · TPM 2.0' : ''}</td><td>${edit && off ? rowBtn('data-hw="fw"', 'edit', 'Edit') : ''}</td></tr>
    <tr><td>Display</td><td>${esc(vm.vga)}</td><td>${edit && off ? rowBtn('data-hw="fw"', 'edit', 'Edit') : ''}</td></tr>
    ${vm.disks.map((d) => `<tr><td>${esc(d.id)}</td><td>${esc(d.bus)} · ${esc(d.file)}${d.model ? ` · ${esc(d.model)}` : ''}${d.cache === 'writeback' ? ' · writeback' : ''}</td><td>${edit ? rowBtn(`data-resize="${esc(d.id)}"`, 'expand', 'Grow') : ''}</td></tr>`).join('')}
    ${vm.cdroms.map((c, i) => `<tr><td>cdrom${i}</td><td>${c ? esc(c) : '<span class="muted">empty</span>'}</td><td>${edit ? rowBtn(`data-cd="${i}"`, 'disc', 'Change disc') : ''}</td></tr>`).join('')}
    ${vm.nics.map((n) => `<tr><td>${esc(n.id)}</td><td>${esc(n.model)} on ${esc(n.network)} · <span class="addr">${esc(n.mac)}</span>${n.ports.length ? ` · ports ${esc(n.ports.join(', '))}` : ''}</td><td></td></tr>`).join('')}
    ${vm.hostpci.map((p) => `<tr><td>PCI</td><td class="addr">${esc(p)}</td><td></td></tr>`).join('')}
    ${vm.usb.map((u) => `<tr><td>USB</td><td class="addr">${esc(u)}</td><td></td></tr>`).join('')}
    <tr><td>Boot order</td><td>${esc(vm.boot.join(' → '))}</td><td></td></tr>
    <tr><td>Looks like a real PC</td><td>${vm.realistic ? `yes: ${esc(vm.smbios.board_manufacturer)} ${esc(vm.smbios.board_product)}, BIOS ${esc(vm.smbios.bios_vendor)}` : 'no'}</td><td></td></tr>
  </tbody></table></div>
  ${edit && off ? `<div class="toolbar" style="margin-top:12px"><button class="ghost-btn" id="hwDisk">${ic('plus')}Add disk</button><button class="ghost-btn" id="hwAll">${ic('edit')}Edit settings</button></div>` : ''}`;
  H.hydrateIcons($('#vmBody'));
}

async function tabSnaps() {
  const list = await api(`/v1/vms/${encodeURIComponent(V.cur)}/snapshots`).catch((e) => { fail(e); return []; });
  const edit = can('VM.Snapshot', V.cur);
  $('#vmBody').innerHTML = `<div class="toolbar"><p class="muted small" style="margin:0">Snapshots live inside the disk files (qcow2). Taken while the VM runs they include its memory, so going back resumes exactly there.</p><span class="sp"></span>${edit ? `<button class="primary-btn" id="snapNew">${ic('plus')}Take snapshot</button>` : ''}</div>
    <div class="panel table-wrap"><table class="table"><tbody>${list.map((s) => `<tr><td><b>${esc(s.name)}</b></td><td class="muted">${esc(s.date)}</td><td>${s.vm_state ? '<span class="chip">with memory</span>' : ''}</td><td><div class="row-actions">${edit ? rowBtn(`data-snap-back="${esc(s.name)}"`, 'restart', 'Go back to this') + rowBtn(`data-snap-del="${esc(s.name)}"`, 'trash', 'Delete', 'stop') : ''}</div></td></tr>`).join('') || '<tr><td class="muted">No snapshots.</td></tr>'}</tbody></table></div>`;
  H.hydrateIcons($('#vmBody'));
}

async function tabBackups() {
  const stores = await api('/v1/storage').catch(() => []);
  const rows = [];
  for (const s of stores) {
    const b = await api(`/v1/storage/${encodeURIComponent(s.id)}/backups?group=${encodeURIComponent(`vm/${V.cur}`)}`).catch(() => []);
    for (const x of b) rows.push({ ...x, store: s.id });
  }
  rows.sort((a, b) => b.time - a.time);
  $('#vmBody').innerHTML = `<div class="toolbar"><p class="muted small" style="margin:0">Backups hold the whole VM: settings, UEFI variables, TPM state and its disks (deduplicated).</p><span class="sp"></span>${H.can('Backup.Create', '*') ? `<button class="primary-btn" id="vmBk">${ic('shield')}Back up now</button>` : ''}</div>
    <div class="panel table-wrap"><table class="table"><tbody>${rows.map((r) => `<tr><td>${new Date(r.time * 1000).toLocaleString()}${r.notes ? `<small class="muted block">${esc(r.notes)}</small>` : ''}</td><td class="num">${H.bytes(r.size)}</td><td>${esc(r.store)}</td><td class="muted">${esc(r.mode || '')}</td><td>${H.can('Backup.Restore', '/storage') ? rowBtn(`data-vm-restore="${esc(r.id)}" data-store="${esc(r.store)}"`, 'restart', 'Restore') : ''}</td></tr>`).join('') || '<tr><td class="muted">No backups of this VM yet.</td></tr>'}</tbody></table></div>`;
  H.hydrateIcons($('#vmBody'));
}

// ---------------------------------------------------------------- create / edit
function vmForm(vm, fresh) {
  const host = V.host || { isos: [], networks: [], nics: [] };
  const nic = vm.nics[0] || { network: 'default', model: 'virtio', ports: [] };
  const nets = [...host.networks.map((n) => [n.name, `${n.name} (${n.subnet}, OG-Host network)`]), ...host.nics.map((n) => [`lan:${n}`, `${n}: straight on the LAN`])];
  return `
    <div class="row2"><label>Name<input name="name" value="${esc(vm.name)}" ${fresh ? 'required' : 'readonly'} pattern="[a-z0-9][a-z0-9\\-]*" maxlength="40" placeholder="win11"></label>
    <label>Operating system<select name="os">${[['linux', 'Linux'], ['windows', 'Windows'], ['other', 'Other']].map(([v, l]) => `<option value="${v}" ${vm.os === v ? 'selected' : ''}>${l}</option>`).join('')}</select></label></div>
    <div class="row2"><label>vCPUs<input name="cores" type="number" min="1" max="256" value="${vm.cores}"></label><label>Memory (MiB)<input name="memory" type="number" min="64" step="64" value="${vm.memory}"></label></div>
    ${fresh ? `<div class="row2"><label>Disk size (GiB)<input name="disk" type="number" min="1" value="${vm.os === 'windows' ? 64 : 32}"></label>
      <label>Installer ISO<select name="iso"><option value="">none</option>${host.isos.map((i) => `<option>${esc(i.name)}</option>`).join('')}</select></label></div>
      <div class="iso-up"><label class="ghost-btn">${ic('download')} Upload an ISO<input type="file" accept=".iso,.img" id="isoFile" hidden></label><span class="muted small" id="isoProg"></span></div>
      <div class="row2"><label>Network<select name="network">${nets.map(([v, l]) => `<option value="${esc(v)}" ${nic.network === v ? 'selected' : ''}>${esc(l)}</option>`).join('')}</select></label>
      <label>Published ports <span class="muted small">OG-Host networks, e.g. 3389:3389</span><input name="ports" value="${esc(nic.ports.join(' '))}" placeholder="2222:22"></label></div>` : ''}
    <div class="row2"><label>Firmware<select name="firmware"><option value="uefi" ${vm.firmware === 'uefi' ? 'selected' : ''}>UEFI</option><option value="bios" ${vm.firmware === 'bios' ? 'selected' : ''}>BIOS (legacy)</option></select></label>
    <label>Display<select name="vga">${['std', 'virtio', 'qxl', 'none'].map((x) => `<option ${vm.vga === x ? 'selected' : ''}>${x}</option>`).join('')}</select></label></div>
    <label class="check"><input type="checkbox" name="secure_boot" ${vm.secure_boot ? 'checked' : ''}> Secure Boot (Microsoft keys)</label>
    <label class="check"><input type="checkbox" name="tpm" ${vm.tpm ? 'checked' : ''}> TPM 2.0 <span class="muted small">(Windows 11 needs it, with Secure Boot)</span></label>
    <label class="check"><input type="checkbox" name="realistic" ${vm.realistic ? 'checked' : ''}> Looks like a real PC <span class="muted small">(hides the hypervisor from the guest: for games and software that refuse VMs; no virtio devices)</span></label>
    <label class="check"><input type="checkbox" name="autostart" ${vm.autostart ? 'checked' : ''}> Start with the server</label>
    <label>Comment<input name="comment" value="${esc(vm.comment)}"></label>`;
}

function newVm() {
  const vm = { name: '', os: 'linux', cores: 2, memory: 2048, firmware: 'uefi', vga: 'std', secure_boot: false, tpm: false, realistic: false, autostart: false, comment: '', nics: [{ network: 'default', model: 'virtio', ports: [] }] };
  const { box, close } = H.modal(`<h3>${ic('monitor')}New virtual machine</h3><form class="form" id="vmForm">${vmForm(vm, true)}<p class="err" id="vmErr"></p><div class="modal-actions"><button type="button" class="ghost-btn" data-x>Cancel</button><button class="primary-btn">Create and start</button></div></form>`, { wide: true });
  const f = box.querySelector('#vmForm');
  f.os.onchange = () => { const win = f.os.value === 'windows'; f.secure_boot.checked = win; f.tpm.checked = win; f.firmware.value = 'uefi'; f.disk.value = win ? 64 : 32; if (win && +f.memory.value < 4096) f.memory.value = 4096; };
  box.querySelector('[data-x]').onclick = close;
  box.querySelector('#isoFile').onchange = (e) => uploadIso(e.target.files[0], box, f);
  f.onsubmit = async (e) => {
    e.preventDefault();
    const v = Object.fromEntries(new FormData(f).entries());
    const realistic = f.realistic.checked;
    const body = {
      vm: {
        name: v.name, os: v.os, cores: +v.cores, memory: +v.memory, firmware: v.firmware, vga: v.vga, secure_boot: f.secure_boot.checked, tpm: f.tpm.checked, realistic, autostart: f.autostart.checked, comment: v.comment,
        disks: [{ id: 'disk0', bus: realistic ? 'sata' : (v.os === 'windows' ? 'sata' : 'virtio') }],
        cdroms: v.iso ? [v.iso] : [''],
        nics: [{ id: 'net0', model: realistic || v.os === 'windows' ? 'e1000e' : 'virtio', network: v.network, ports: v.ports.split(/[\s,]+/).filter(Boolean) }],
      },
      sizes: { disk0: +v.disk },
      start: true,
    };
    const b = f.querySelector('.primary-btn'); b.disabled = true;
    try { await api('/v1/vms', { method: 'POST', body }); close(); toast(`${v.name} created`); location.hash = `#vms/${encodeURIComponent(v.name)}`; V.tab = 'console'; } catch (err) { box.querySelector('#vmErr').textContent = err.message; } finally { b.disabled = false; }
  };
}

async function uploadIso(file, box, f) {
  if (!file) return;
  const name = file.name.replace(/[^A-Za-z0-9_.+-]/g, '_');
  const prog = box.querySelector('#isoProg');
  prog.textContent = `uploading ${name}…`;
  // XHR for upload progress (fetch has none)
  await new Promise((res) => {
    const x = new XMLHttpRequest();
    x.open('PUT', `/v1/iso/${encodeURIComponent(name)}`);
    x.setRequestHeader('X-OG-UI', '1');
    x.upload.onprogress = (e) => { if (e.lengthComputable) prog.textContent = `uploading ${name}: ${Math.round((e.loaded / e.total) * 100)}%`; };
    x.onload = () => {
      if (x.status >= 200 && x.status < 300) {
        prog.textContent = `${name} uploaded`;
        const o = document.createElement('option'); o.textContent = name; f.iso.appendChild(o); f.iso.value = name;
        if (V.host) V.host.isos.push({ name });
      } else { try { prog.textContent = JSON.parse(x.responseText).error; } catch { prog.textContent = `upload failed (${x.status})`; } }
      res();
    };
    x.onerror = () => { prog.textContent = 'upload failed'; res(); };
    x.send(file);
  });
}

function editVm() {
  const vm = structuredClone(cur().vm);
  const { box, close } = H.modal(`<h3>${ic('edit')}Settings of ${esc(vm.name)}</h3><form class="form" id="vmForm">${vmForm(vm, false)}
    <div class="row2"><label>CPU cap (cores, 0 = none)<input name="cpu_limit" type="number" min="0" step="0.25" value="${vm.cpu_limit}"></label><label>Balloon down to (MiB, 0 = off)<input name="balloon" type="number" min="0" step="64" value="${vm.balloon}"></label></div>
    <p class="err" id="vmErr"></p><div class="modal-actions"><button type="button" class="ghost-btn" data-x>Cancel</button><button class="primary-btn">Save</button></div></form>`, { wide: true });
  const f = box.querySelector('#vmForm');
  box.querySelector('[data-x]').onclick = close;
  f.onsubmit = async (e) => {
    e.preventDefault();
    const v = Object.fromEntries(new FormData(f).entries());
    Object.assign(vm, { os: v.os, cores: +v.cores, memory: +v.memory, firmware: v.firmware, vga: v.vga, secure_boot: f.secure_boot.checked, tpm: f.tpm.checked, realistic: f.realistic.checked, autostart: f.autostart.checked, comment: v.comment, cpu_limit: +v.cpu_limit, balloon: +v.balloon });
    try { await api(`/v1/vms/${encodeURIComponent(vm.name)}`, { method: 'PUT', body: vm }); close(); toast('Saved'); reload(); } catch (err) { box.querySelector('#vmErr').textContent = err.message; }
  };
}

// ---------------------------------------------------------------- clicks
async function onClick(e) {
  const t = e.target;
  const on = (s) => t.closest(s);
  let el;
  if ((el = on('[data-vact]'))) { e.stopPropagation(); vmAction(el.dataset.vact, el.dataset.n); }
  else if ((el = on('[data-vconsole]'))) { e.stopPropagation(); V.tab = 'console'; location.hash = `#vms/${encodeURIComponent(el.dataset.vconsole)}`; }
  else if ((el = on('[data-vmenu]'))) {
    e.stopPropagation();
    const n = el.dataset.vmenu;
    H.showMenu(el, [['restart', 'Reboot', () => vmAction('reboot', n)], ['pause', 'Pause', () => vmAction('pause', n)], ['restart', 'Reset (hard)', () => vmAction('reset', n)], null, ['stop', 'Power off now (no shutdown)', async () => { if (await H.confirmBox(`Pull the plug on ${n}?`, 'Like holding the power button: unsaved work in the VM is lost.', 'Power off')) vmAction('stop', n); }, 'stop']]);
  } else if ((el = on('[data-vt]'))) { V.tab = el.dataset.vt; if (!V.cur) return; paintTab(); }
  else if ((el = on('tr[data-vm]'))) location.hash = `#vms/${encodeURIComponent(el.dataset.vm)}`;
  else if (on('#vmNew')) { if (!V.host) V.host = await api('/v1/vms/host').catch(() => null); newVm(); }
  else if (on('#vmLogRefresh')) loadSerial();
  else if (on('#hwAll') || on('[data-hw]')) editVm();
  else if (on('#hwDisk')) {
    const v = cur();
    const { box, close } = H.modal(`<h3>${ic('hdd')}Add a disk</h3><form class="form"><div class="row2"><label>Size (GiB)<input name="size" type="number" min="1" value="32" required></label><label>Bus<select name="bus">${['virtio', 'sata', 'scsi', 'nvme'].map((b) => `<option>${b}</option>`).join('')}</select></label></div><p class="err"></p><div class="modal-actions"><button type="button" class="ghost-btn" data-x>Cancel</button><button class="primary-btn">Add</button></div></form>`);
    box.querySelector('[data-x]').onclick = close;
    box.querySelector('form').onsubmit = async (ev) => {
      ev.preventDefault();
      const n = v.vm.disks.length; let id = `disk${n}`; while (v.vm.disks.some((d) => d.id === id)) id += 'x';
      try { await api(`/v1/vms/${encodeURIComponent(V.cur)}/disks`, { method: 'POST', body: { disk: { id, bus: ev.target.bus.value }, size: +ev.target.size.value } }); close(); reload(); } catch (err) { box.querySelector('.err').textContent = err.message; }
    };
  } else if ((el = on('[data-resize]'))) {
    const { box, close } = H.modal(`<h3>${ic('expand')}Grow ${esc(el.dataset.resize)}</h3><form class="form"><p class="muted">Disks only grow; afterwards grow the partition inside the VM.</p><label>New size (GiB)<input name="size" type="number" min="1" required></label><p class="err"></p><div class="modal-actions"><button type="button" class="ghost-btn" data-x>Cancel</button><button class="primary-btn">Grow</button></div></form>`);
    box.querySelector('[data-x]').onclick = close;
    box.querySelector('form').onsubmit = async (ev) => { ev.preventDefault(); try { await api(`/v1/vms/${encodeURIComponent(V.cur)}/disks/${encodeURIComponent(el.dataset.resize)}/resize`, { method: 'POST', body: { size: +ev.target.size.value } }); close(); toast('Grown'); reload(); } catch (err) { box.querySelector('.err').textContent = err.message; } };
  } else if ((el = on('[data-cd]'))) {
    const isos = (await api('/v1/iso').catch(() => []));
    const { box, close } = H.modal(`<h3>${ic('disc')}Disc in cdrom${el.dataset.cd}</h3><form class="form"><label>ISO<select name="iso"><option value="">(eject)</option>${isos.map((i) => `<option>${esc(i.name)}</option>`).join('')}</select></label><p class="err"></p><div class="modal-actions"><button type="button" class="ghost-btn" data-x>Cancel</button><button class="primary-btn">Change</button></div></form>`);
    box.querySelector('[data-x]').onclick = close;
    box.querySelector('form').onsubmit = async (ev) => { ev.preventDefault(); try { await api(`/v1/vms/${encodeURIComponent(V.cur)}/cdrom`, { method: 'POST', body: { index: +el.dataset.cd, iso: ev.target.iso.value } }); close(); reload(); } catch (err) { box.querySelector('.err').textContent = err.message; } };
  } else if (on('#snapNew')) {
    const { box, close } = H.modal(`<h3>${ic('pin')}Take a snapshot</h3><form class="form"><label>Name<input name="n" required pattern="[A-Za-z0-9_\\-]+" maxlength="40" placeholder="before-update"></label><div class="modal-actions"><button type="button" class="ghost-btn" data-x>Cancel</button><button class="primary-btn">Take</button></div></form>`);
    box.querySelector('[data-x]').onclick = close;
    box.querySelector('form').onsubmit = async (ev) => { ev.preventDefault(); close(); try { const r = await api(`/v1/vms/${encodeURIComponent(V.cur)}/snapshots`, { method: 'POST', body: { name: ev.target.n.value } }); H.taskLog(r.task, () => paintTab()); } catch (err) { fail(err); } };
  } else if ((el = on('[data-snap-back]'))) {
    if (await H.confirmBox(`Go back to ${el.dataset.snapBack}?`, 'Everything since that snapshot is lost.', 'Go back')) api(`/v1/vms/${encodeURIComponent(V.cur)}/snapshots/${encodeURIComponent(el.dataset.snapBack)}/rollback`, { method: 'POST' }).then((r) => H.taskLog(r.task, () => paintTab()), fail);
  } else if ((el = on('[data-snap-del]'))) {
    if (await H.confirmBox(`Delete snapshot ${el.dataset.snapDel}?`, 'The VM stays as it is now.', 'Delete')) api(`/v1/vms/${encodeURIComponent(V.cur)}/snapshots/${encodeURIComponent(el.dataset.snapDel)}`, { method: 'DELETE' }).then(() => paintTab(), fail);
  } else if (on('#vmBk')) {
    const stores = await api('/v1/storage').catch(() => []);
    const { box, close } = H.modal(`<h3>${ic('shield')}Back up ${esc(V.cur)}</h3><form class="form"><div class="row2"><label>Datastore<select name="store">${stores.map((s) => `<option>${esc(s.id)}</option>`).join('')}</select></label><label>Mode<select name="mode"><option value="pause">pause (consistent, resumes after)</option><option value="stop">shut down and start again</option><option value="live">live</option></select></label></div><label>Notes<input name="notes"></label><div class="modal-actions"><button type="button" class="ghost-btn" data-x>Cancel</button><button class="primary-btn">Back up</button></div></form>`);
    box.querySelector('[data-x]').onclick = close;
    box.querySelector('form').onsubmit = async (ev) => { ev.preventDefault(); const f = ev.target; close(); try { const r = await api('/v1/backup', { method: 'POST', body: { store: f.store.value, targets: [{ kind: 'vm', name: V.cur }], opts: { mode: f.mode.value, notes: f.notes.value } } }); H.taskLog(r.task, () => paintTab()); } catch (err) { fail(err); } };
  } else if ((el = on('[data-vm-restore]'))) {
    if (await H.confirmBox(`Restore ${V.cur}?`, 'The VM is shut down and its disks and settings are replaced with the backup.', 'Restore')) api('/v1/restore', { method: 'POST', body: { store: el.dataset.store, snapshot: el.dataset.vmRestore } }).then((r) => H.taskLog(r.task, () => reload()), fail);
  }
}

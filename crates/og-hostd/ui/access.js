// Settings: the signed-in account (password, two-factor, API tokens, sessions) and, for
// account managers, users, groups, roles, permissions, all API tokens and the audit log.
let H, $, $$, esc, ic, api, toast, S;

const TABS = [
  ['account', 'user', 'My account', null],
  ['users', 'user', 'Users', 'Access.Audit'],
  ['groups', 'servers', 'Groups', 'Access.Audit'],
  ['roles', 'shield', 'Roles', 'Access.Audit'],
  ['acl', 'shieldok', 'Permissions', 'Access.Audit'],
  ['tokens', 'link', 'API tokens', 'Access.Audit'],
  ['audit', 'note', 'Audit log', 'Access.Audit'],
];
let cur = 'account';

export function init(h) {
  H = h; ({ $, $$, esc, ic, api, toast, S } = h);
  H.ROUTES.settings = { el: 'settings', tab: 'settings', run: (arg) => open(arg || 'account') };
  $('#setTabs').addEventListener('click', (e) => { const b = e.target.closest('button[data-t]'); if (b) location.hash = `#settings/${b.dataset.t}`; });
  H.paletteItems = ((prev) => (q) => [...(prev ? prev(q) : []), { icon: 'shield', label: 'Account & security', hint: 'settings', go: () => (location.hash = '#settings/account') }, ...(H.can('Access.Audit', '/access') ? [{ icon: 'user', label: 'Users & permissions', hint: 'settings', go: () => (location.hash = '#settings/users') }] : [])])(H.paletteItems);
}

const canManage = () => H.can('Access.Modify', '/access');
const when = (t) => (t ? new Date(t * 1000).toLocaleString() : '—');
const day = (t) => (t ? new Date(t * 1000).toLocaleDateString() : 'never');
const form = (f) => Object.fromEntries(new FormData(f).entries());
const fail = (e) => toast(e.message, true);

function open(tab) {
  const tabs = TABS.filter((t) => !t[3] || H.can(t[3], '/access'));
  cur = tabs.some((t) => t[0] === tab) ? tab : 'account';
  $('#setTabs').innerHTML = tabs.map(([id, icon, label]) => `<button data-t="${id}" class="${id === cur ? 'on' : ''}">${ic(icon)}${esc(label)}</button>`).join('');
  $('#setTabs').hidden = tabs.length < 2;
  $('#setMeta').textContent = S.me ? `signed in as ${S.me.user}` : '';
  $('#setBody').innerHTML = '<p class="muted">Loading…</p>';
  ({ account, users, groups, roles, acl, tokens, audit })[cur]().catch(fail);
}
const again = () => open(cur);

function table(head, rows, empty) {
  return `<div class="panel table-wrap"><table class="table"><thead><tr>${head.map((h) => `<th class="${h.startsWith('#') ? 'num' : ''}">${esc(h.replace(/^#/, ''))}</th>`).join('')}</tr></thead><tbody>${rows.join('') || `<tr><td colspan="${head.length}" class="muted">${esc(empty)}</td></tr>`}</tbody></table></div>`;
}
const head = (title, btn) => `<div class="section-head"><h2>${esc(title)}</h2><span class="sp"></span>${btn || ''}</div>`;
const addBtn = (id, label) => (canManage() ? `<button class="primary-btn" id="${id}">${ic('plus')}${esc(label)}</button>` : '');
const rowBtn = (attr, icon, title, cls = '') => `<button class="row-btn ${cls}" ${attr} title="${esc(title)}">${ic(icon)}</button>`;

function onClick(map) {
  $('#setBody').onclick = (e) => {
    for (const [sel, fn] of Object.entries(map)) {
      const el = e.target.closest(sel);
      if (el) { fn(el); return; }
    }
  };
}

/** Modal with a form; `submit(values)` may throw to show the error in the form. */
function formModal(title, icon, inner, submit, { ok = 'Save', wide = false } = {}) {
  const { box, close } = H.modal(`<h3>${ic(icon)}${esc(title)}</h3><form class="form" id="amForm">${inner}<p class="err" id="amErr"></p><div class="modal-actions"><button type="button" class="ghost-btn" data-x>Cancel</button><button class="primary-btn">${esc(ok)}</button></div></form>`, { wide });
  box.querySelector('[data-x]').onclick = close;
  const f = box.querySelector('#amForm');
  f.onsubmit = async (e) => {
    e.preventDefault();
    const b = f.querySelector('.primary-btn'); b.disabled = true;
    try { if ((await submit(form(f), f)) !== false) close(); } catch (err) { box.querySelector('#amErr').textContent = err.message; } finally { b.disabled = false; }
  };
  f.querySelector('input,select')?.focus();
  return { box, close };
}

function showSecret(title, text, note, onDone) {
  const { box, close } = H.modal(`<h3>${ic('shieldok')}${esc(title)}</h3><p class="muted">${esc(note)}</p><pre class="secret">${esc(text)}</pre><div class="modal-actions"><button class="ghost-btn" data-copy>${ic('copy')}Copy</button><button class="primary-btn" data-x>Done</button></div>`);
  box.querySelector('[data-x]').onclick = () => { close(); if (onDone) onDone(); };
  box.querySelector('[data-copy]').onclick = () => navigator.clipboard.writeText(text).then(() => toast('Copied'), fail);
}

// ---------------------------------------------------------------- my account
async function account() {
  const me = S.me || (await api('/v1/ui/me'));
  const [tfa, toks, sess] = await Promise.all([api('/v1/access/me/tfa'), api('/v1/access/me/tokens'), api('/v1/access/me/sessions')]);
  const local = me.userid.endsWith('@local');
  const isRoot = me.userid === 'root@pam';
  $('#setBody').innerHTML = `
    <div class="set-grid">
      <div class="panel set-card">
        <h3>${ic('user')}${esc(me.userid)}</h3>
        <p class="muted">${local ? 'OG-Host account' : 'Linux system account (PAM): change its password on the server with <code>passwd</code>.'}</p>
        ${local ? `<button class="ghost-btn" id="pwBtn">${ic('edit')}Change password</button>` : ''}
      </div>
      <div class="panel set-card">
        <h3>${ic('shield')}Two-factor sign-in ${tfa.enabled ? '<span class="chip good">on</span>' : '<span class="chip warn">off</span>'}</h3>
        <p class="muted">${tfa.enabled ? `A code from your authenticator app is asked for at every sign-in. ${tfa.recovery_left} recovery codes left.` : 'Ask for a code from an authenticator app (Aegis, Google Authenticator, 1Password…) as well as your password.'}</p>
        ${isRoot ? '<p class="muted small">root@pam is protected by the server\'s own login; add an OG-Host account for two-factor.</p>' : tfa.enabled ? `<button class="ghost-btn" id="tfaOff">${ic('x')}Turn off</button>` : `<button class="primary-btn" id="tfaOn">${ic('shieldok')}Set up</button>`}
      </div>
    </div>
    ${head('My API tokens', isRoot ? '' : `<button class="primary-btn" id="myTokNew">${ic('plus')}New token</button>`)}
    <p class="muted small">For scripts and tools: send <code>Authorization: OGAPIToken &lt;id&gt;=&lt;secret&gt;</code>. A separated-privileges token only gets what you grant it under Permissions (and never more than you have).</p>
    ${table(['Token', 'Separated', 'Expires', 'Comment', ''], toks.map((t) => `<tr><td class="addr">${esc(t.id)}</td><td>${t.privsep ? 'yes' : 'no'}</td><td>${day(t.expire)}</td><td>${esc(t.comment)}</td><td>${rowBtn(`data-mytok="${esc(t.id.split('!')[1])}"`, 'trash', 'Delete', 'stop')}</td></tr>`), 'No tokens.')}
    ${head('Signed-in sessions')}
    ${table(['From', 'Signed in', 'Expires', ''], sess.map((x) => `<tr><td class="addr">${esc(x.ip)} ${x.current ? '<span class="chip accent">this one</span>' : ''}</td><td>${when(x.created)}</td><td>${when(x.expires)}</td><td>${x.current ? '' : rowBtn(`data-sess="${esc(x.id)}"`, 'logout', 'Sign out', 'stop')}</td></tr>`), 'No sessions.')}`;
  H.hydrateIcons($('#setBody'));
  onClick({
    '#pwBtn': () => formModal('Change password', 'edit', `<label>Current password<input name="current" type="password" autocomplete="current-password" required></label><label>New password <span class="muted small">at least 10 characters</span><input name="new" type="password" autocomplete="new-password" minlength="10" required></label><label>New password again<input name="again" type="password" autocomplete="new-password" required></label>`, async (v) => {
      if (v.new !== v.again) throw new Error("The new passwords don't match");
      await api('/v1/access/me/password', { method: 'POST', body: { current: v.current, new: v.new } });
      toast('Password changed. Sign in again with the new one.');
      setTimeout(() => location.reload(), 1200);
    }),
    '#tfaOn': tfaSetup,
    '#tfaOff': () => formModal('Turn off two-factor', 'shield', '<label>A code from your app (or a recovery code)<input name="code" autocomplete="one-time-code" required></label>', async (v) => {
      await api('/v1/access/me/tfa', { method: 'DELETE', body: { code: v.code } });
      toast('Two-factor is off'); setTimeout(() => location.reload(), 800);
    }, { ok: 'Turn off' }),
    '#myTokNew': () => tokenModal(null),
    '[data-mytok]': async (el) => { if (await H.confirmBox('Delete token', `Scripts using ${el.dataset.mytok} stop working.`, 'Delete')) api(`/v1/access/me/tokens/${encodeURIComponent(el.dataset.mytok)}`, { method: 'DELETE' }).then(again, fail); },
    '[data-sess]': (el) => api(`/v1/access/me/sessions/${el.dataset.sess}`, { method: 'DELETE' }).then(again, fail),
  });
}

async function tfaSetup() {
  const t = await api('/v1/access/me/tfa', { method: 'POST' }).catch(fail);
  if (!t) return;
  formModal('Set up two-factor', 'shieldok', `
    <p class="muted">Scan this with your authenticator app, then type the 6-digit code it shows.</p>
    <div class="qr">${t.qr_svg}</div>
    <details><summary class="muted small">Can't scan? Enter the key by hand</summary><pre class="secret">${esc(t.secret.replace(/(.{4})/g, '$1 ').trim())}</pre></details>
    <label>Code from the app<input name="code" inputmode="numeric" autocomplete="one-time-code" maxlength="7" required></label>`, async (v) => {
    const r = await api('/v1/access/me/tfa/confirm', { method: 'POST', body: { code: v.code } });
    showSecret('Two-factor is on', r.recovery_codes.join('\n'), 'Keep these recovery codes somewhere safe (each works once): they get you in if you lose your phone. They are not shown again. Sign in again after this.', () => location.reload());
    return false;
  }, { ok: 'Turn on' });
}

function tokenModal(user) {
  formModal('New API token', 'link', `
    ${user === null ? '' : `<label>User<select name="user">${user.map((u) => `<option>${esc(u.id)}</option>`).join('')}</select></label>`}
    <label>Name <span class="muted small">letters, digits, _ . -</span><input name="name" pattern="[A-Za-z0-9_][A-Za-z0-9_.\\-]*" maxlength="64" required></label>
    <label class="check"><input type="checkbox" name="privsep" checked> Separated privileges <span class="muted small">(grant it access under Permissions)</span></label>
    <label>Expires <span class="muted small">optional</span><input name="expire" type="date"></label>
    <label>Comment<input name="comment" maxlength="200"></label>`, async (v, f) => {
    const body = { name: v.name, privsep: f.privsep.checked, comment: v.comment || '', expire: v.expire ? Math.floor(new Date(v.expire).getTime() / 1000) : 0 };
    const r = user === null ? await api('/v1/access/me/tokens', { method: 'POST', body }) : await api('/v1/access/tokens', { method: 'POST', body: { ...body, user: v.user } });
    showSecret('Token created', r.header, `Copy it now: the secret for ${r.id} is not shown again.`);
    again();
    return false;
  }, { ok: 'Create' });
}

// ---------------------------------------------------------------- users
async function users() {
  const [list, gs] = await Promise.all([api('/v1/access/users'), api('/v1/access/groups')]);
  $('#setBody').innerHTML = head('Users', addBtn('uNew', 'Add user')) + table(['User', 'Name', 'Groups', '2FA', 'Expires', 'Tokens', ''], list.map((u) => `<tr>
      <td class="name"><b>${esc(u.id)}</b> ${u.enable ? '' : '<span class="chip warn">disabled</span>'}<small>${esc(u.email || u.comment || '')}</small></td>
      <td>${esc(`${u.firstname} ${u.lastname}`.trim())}</td>
      <td>${u.groups.map((g) => `<span class="chip">${esc(g)}</span>`).join(' ')}</td>
      <td>${u.tfa ? '<span class="chip good">on</span>' : '<span class="muted">off</span>'}</td>
      <td>${day(u.expire)}</td><td class="num">${u.tokens}</td>
      <td><div class="row-actions">${canManage() ? `${rowBtn(`data-uedit="${esc(u.id)}"`, 'edit', 'Edit')}${u.tfa ? rowBtn(`data-utfa="${esc(u.id)}"`, 'shield', 'Turn off two-factor (lost phone)') : ''}${u.id !== S.me.userid ? rowBtn(`data-udel="${esc(u.id)}"`, 'trash', 'Remove', 'stop') : ''}` : ''}</div></td></tr>`), 'No users.');
  H.hydrateIcons($('#setBody'));
  const edit = (u) => {
    const fresh = !u; u = u || { id: '', enable: true, groups: [], firstname: '', lastname: '', email: '', comment: '', expire: 0 };
    const [name, realm] = fresh ? ['', 'local'] : u.id.split('@');
    formModal(fresh ? 'Add user' : `Edit ${u.id}`, 'user', `
      <div class="row2"><label>User name<input name="name" value="${esc(name)}" ${fresh ? 'required' : 'disabled'} pattern="[A-Za-z0-9_][A-Za-z0-9_.\\-]*"></label>
      <label>Realm<select name="realm" ${fresh ? '' : 'disabled'}><option value="local" ${realm === 'local' ? 'selected' : ''}>OG-Host</option><option value="pam" ${realm === 'pam' ? 'selected' : ''}>Linux system (PAM)</option></select></label></div>
      <label>Password <span class="muted small">${fresh ? 'OG-Host realm: at least 10 characters' : 'leave empty to keep it'}</span><input name="password" type="password" autocomplete="new-password" minlength="10"></label>
      <div class="row2"><label>First name<input name="firstname" value="${esc(u.firstname)}"></label><label>Last name<input name="lastname" value="${esc(u.lastname)}"></label></div>
      <div class="row2"><label>Email<input name="email" type="email" value="${esc(u.email)}"></label><label>Expires <span class="muted small">optional</span><input name="expire" type="date" value="${u.expire ? new Date(u.expire * 1000).toISOString().slice(0, 10) : ''}"></label></div>
      <label>Groups<select name="groups" multiple size="${Math.min(5, Math.max(2, gs.length))}">${gs.map((g) => `<option ${u.groups.includes(g.id) ? 'selected' : ''}>${esc(g.id)}</option>`).join('')}</select></label>
      <label>Comment<input name="comment" value="${esc(u.comment)}"></label>
      <label class="check"><input type="checkbox" name="enable" ${u.enable ? 'checked' : ''}> Enabled</label>`, async (v, f) => {
      const body = { firstname: v.firstname, lastname: v.lastname, email: v.email, comment: v.comment, enable: f.enable.checked, expire: v.expire ? Math.floor(new Date(v.expire).getTime() / 1000) : 0, groups: [...f.groups.selectedOptions].map((o) => o.value) };
      if (v.password) body.password = v.password;
      if (fresh) await api('/v1/access/users', { method: 'POST', body: { id: `${v.name}@${v.realm}`, ...body } });
      else await api(`/v1/access/users/${encodeURIComponent(u.id)}`, { method: 'POST', body });
      toast(fresh ? 'User added. Give them access under Permissions.' : 'Saved');
      again();
    }, { wide: true });
  };
  onClick({
    '#uNew': () => edit(null),
    '[data-uedit]': (el) => edit(list.find((u) => u.id === el.dataset.uedit)),
    '[data-utfa]': async (el) => { if (await H.confirmBox('Turn off two-factor', `${el.dataset.utfa} signs in with just a password until they set it up again.`, 'Turn off')) api(`/v1/access/users/${encodeURIComponent(el.dataset.utfa)}/tfa`, { method: 'DELETE' }).then(again, fail); },
    '[data-udel]': async (el) => { if (await H.confirmBox('Remove user', `${el.dataset.udel}, their tokens and permissions go.`, 'Remove')) api(`/v1/access/users/${encodeURIComponent(el.dataset.udel)}`, { method: 'DELETE' }).then(again, fail); },
  });
}

// ---------------------------------------------------------------- groups
async function groups() {
  const list = await api('/v1/access/groups');
  $('#setBody').innerHTML = head('Groups', addBtn('gNew', 'Add group')) + table(['Group', 'Members', 'Comment', ''], list.map((g) => `<tr><td><b>${esc(g.id)}</b></td><td>${g.members.map((m) => `<span class="chip">${esc(m)}</span>`).join(' ') || '<span class="muted">none</span>'}</td><td>${esc(g.comment)}</td><td>${canManage() ? rowBtn(`data-gdel="${esc(g.id)}"`, 'trash', 'Remove', 'stop') : ''}</td></tr>`), 'No groups. Groups let you grant access to several users at once.');
  H.hydrateIcons($('#setBody'));
  onClick({
    '#gNew': () => formModal('Add group', 'servers', '<label>Name<input name="id" required pattern="[A-Za-z0-9_][A-Za-z0-9_.\\-]*"></label><label>Comment<input name="comment"></label>', async (v) => { await api('/v1/access/groups', { method: 'POST', body: v }); again(); }),
    '[data-gdel]': async (el) => { if (await H.confirmBox('Remove group', `Members keep their accounts; permissions given to @${el.dataset.gdel} go.`, 'Remove')) api(`/v1/access/groups/${encodeURIComponent(el.dataset.gdel)}`, { method: 'DELETE' }).then(again, fail); },
  });
}

// ---------------------------------------------------------------- roles
async function roles() {
  const [list, privs] = await Promise.all([api('/v1/access/roles'), api('/v1/access/privileges')]);
  $('#setBody').innerHTML = head('Roles', addBtn('rNew', 'Add role')) + table(['Role', 'Privileges', ''], list.map((r) => `<tr><td class="name"><b>${esc(r.id)}</b> ${r.builtin ? '<span class="chip">built-in</span>' : ''}<small>${esc(r.comment)}</small></td><td class="wrap"><div class="chips">${r.privs.length === privs.length ? '<span class="chip accent">all</span>' : r.privs.map((p) => `<span class="chip">${esc(p)}</span>`).join('') || '<span class="muted">none</span>'}</div></td><td>${!r.builtin && canManage() ? `<div class="row-actions">${rowBtn(`data-redit="${esc(r.id)}"`, 'edit', 'Edit')}${rowBtn(`data-rdel="${esc(r.id)}"`, 'trash', 'Remove', 'stop')}</div>` : ''}</td></tr>`), '');
  H.hydrateIcons($('#setBody'));
  const edit = (r) => {
    const fresh = !r; r = r || { id: '', privs: [], comment: '' };
    const areas = [...new Set(privs.map((p) => p.split('.')[0]))];
    formModal(fresh ? 'Add role' : `Edit ${r.id}`, 'shield', `
      <label>Name<input name="id" value="${esc(r.id)}" ${fresh ? 'required' : 'readonly'} pattern="[A-Za-z0-9_][A-Za-z0-9_.\\-]*"></label>
      <div class="priv-grid">${areas.map((a) => `<fieldset><legend>${esc(a)}</legend>${privs.filter((p) => p.startsWith(`${a}.`)).map((p) => `<label class="check"><input type="checkbox" name="p:${esc(p)}" ${r.privs.includes(p) ? 'checked' : ''}> ${esc(p.split('.')[1])}</label>`).join('')}</fieldset>`).join('')}</div>
      <label>Comment<input name="comment" value="${esc(r.comment)}"></label>`, async (v) => {
      await api('/v1/access/roles', { method: 'POST', body: { id: v.id, comment: v.comment, privs: Object.keys(v).filter((k) => k.startsWith('p:')).map((k) => k.slice(2)) } });
      again();
    }, { wide: true });
  };
  onClick({
    '#rNew': () => edit(null),
    '[data-redit]': (el) => edit(list.find((r) => r.id === el.dataset.redit)),
    '[data-rdel]': async (el) => { if (await H.confirmBox('Remove role', `Permissions using ${el.dataset.rdel} go too.`, 'Remove')) api(`/v1/access/roles/${encodeURIComponent(el.dataset.rdel)}`, { method: 'DELETE' }).then(again, fail); },
  });
}

// ---------------------------------------------------------------- permissions
const PATHS = [['/', 'Everything'], ['/containers', 'All containers'], ['/stacks', 'All stacks'], ['/vms', 'All VMs'], ['/storage', 'Storage & backups'], ['/nodes', 'Cluster nodes'], ['/lab', 'Lab (home, monitor, apps)'], ['/access', 'Users & permissions'], ['/system', 'Firewall, updates']];
async function acl() {
  const [list, us, gs, rs, ts] = await Promise.all([api('/v1/access/acl'), api('/v1/access/users'), api('/v1/access/groups'), api('/v1/access/roles'), api('/v1/access/tokens')]);
  list.sort((a, b) => a.path.localeCompare(b.path) || a.ugid.localeCompare(b.ugid));
  $('#setBody').innerHTML = head('Permissions', addBtn('aNew', 'Grant')) + `<p class="muted small">A role on a path applies there and (when inherited) to everything under it, e.g. <code>/containers/web</code>. The grant closest to the item wins; a user's own grant beats their groups'. root@pam always has everything.</p>` + table(['Path', 'Who', 'Role', 'Inherited', ''], list.map((e, i) => `<tr><td class="addr">${esc(e.path)}</td><td>${e.ugid.startsWith('@') ? `<span class="chip">group ${esc(e.ugid.slice(1))}</span>` : e.ugid.includes('!') ? `<span class="chip">token</span> ${esc(e.ugid)}` : esc(e.ugid)}</td><td><b>${esc(e.role)}</b></td><td>${e.propagate ? 'yes' : 'no'}</td><td>${canManage() ? rowBtn(`data-adel="${i}"`, 'trash', 'Remove', 'stop') : ''}</td></tr>`), 'Nothing granted yet: only root@pam can do anything.');
  H.hydrateIcons($('#setBody'));
  onClick({
    '#aNew': () => formModal('Grant access', 'shieldok', `
      <label>Path<input name="path" list="aclPaths" value="/" required></label>
      <datalist id="aclPaths">${PATHS.map(([p, l]) => `<option value="${p}">${esc(l)}</option>`).join('')}${S.containers.map((c) => `<option value="/containers/${esc(c.name)}">container</option>`).join('')}</datalist>
      <label>Who<select name="ugid">
        <optgroup label="Users">${us.map((u) => `<option>${esc(u.id)}</option>`).join('')}</optgroup>
        ${gs.length ? `<optgroup label="Groups">${gs.map((g) => `<option value="@${esc(g.id)}">${esc(g.id)}</option>`).join('')}</optgroup>` : ''}
        ${ts.length ? `<optgroup label="API tokens">${ts.map((t) => `<option>${esc(t.id)}</option>`).join('')}</optgroup>` : ''}
      </select></label>
      <label>Role<select name="role">${rs.map((r) => `<option value="${esc(r.id)}" ${r.id === 'Operator' ? 'selected' : ''}>${esc(r.id)}${r.comment ? ` — ${esc(r.comment)}` : ''}</option>`).join('')}</select></label>
      <label class="check"><input type="checkbox" name="propagate" checked> Inherited by everything under the path</label>`, async (v, f) => {
      await api('/v1/access/acl', { method: 'POST', body: { path: v.path.trim(), ugid: v.ugid, role: v.role, propagate: f.propagate.checked } });
      again();
    }, { ok: 'Grant', wide: true }),
    '[data-adel]': async (el) => {
      const e = list[+el.dataset.adel];
      if (await H.confirmBox('Remove permission', `${e.ugid} loses ${e.role} on ${e.path}.`, 'Remove')) api('/v1/access/acl', { method: 'POST', body: { ...e, delete: true } }).then(again, fail);
    },
  });
}

// ---------------------------------------------------------------- all tokens
async function tokens() {
  const [list, us] = await Promise.all([api('/v1/access/tokens'), api('/v1/access/users')]);
  $('#setBody').innerHTML = head('API tokens', canManage() ? `<button class="primary-btn" id="tNew">${ic('plus')}New token</button>` : '') + table(['Token', 'Separated', 'Expires', 'Created', 'Comment', ''], list.map((t) => `<tr><td class="addr">${esc(t.id)}</td><td>${t.privsep ? 'yes' : 'no'}</td><td>${day(t.expire)}</td><td>${day(t.created)}</td><td>${esc(t.comment)}</td><td>${canManage() ? rowBtn(`data-tdel="${esc(t.id)}"`, 'trash', 'Delete', 'stop') : ''}</td></tr>`), 'No API tokens.');
  H.hydrateIcons($('#setBody'));
  onClick({
    '#tNew': () => tokenModal(us),
    '[data-tdel]': async (el) => { if (await H.confirmBox('Delete token', `Scripts using ${el.dataset.tdel} stop working.`, 'Delete')) api(`/v1/access/tokens/${encodeURIComponent(el.dataset.tdel)}`, { method: 'DELETE' }).then(again, fail); },
  });
}

// ---------------------------------------------------------------- audit log
let auditQ = '';
async function audit() {
  const list = await api(`/v1/access/audit?limit=500&q=${encodeURIComponent(auditQ)}`);
  $('#setBody').innerHTML = `<div class="toolbar"><form class="q" id="auForm"><i data-icon="search"></i><input id="auQ" placeholder="Filter: user, action, address" value="${esc(auditQ)}"></form><span class="sp"></span><span class="muted">${list.length} entries (newest first)</span></div>` +
    table(['When', 'Who', 'From', 'Action', 'Target', ''], list.map((a) => `<tr><td>${when(a.ts)}</td><td>${esc(a.user)}</td><td class="addr">${esc(a.ip)}</td><td class="addr">${esc(a.action)}</td><td class="addr">${esc(a.target || '')}</td><td>${a.ok ? '<span class="chip good">ok</span>' : '<span class="chip crit">failed</span>'}</td></tr>`), 'Nothing logged yet.');
  H.hydrateIcons($('#setBody'));
  $('#auForm').onsubmit = (e) => { e.preventDefault(); auditQ = $('#auQ').value.trim(); audit().catch(fail); };
  onClick({});
}

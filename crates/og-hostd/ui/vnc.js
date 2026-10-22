// A small VNC (RFB 3.8) client for VM consoles: Raw + CopyRect + DesktopSize, QEMU's
// extended key events (scancodes, so the guest's keyboard layout applies), absolute pointer.
// The socket is og-hostd's /v1/vms/<name>/vnc WebSocket (auth is the session cookie).

// KeyboardEvent.code -> XT scancode (set 1; 0xe0-prefixed keys as 0xe0xx)
const XT = {
  Escape: 0x01, Digit1: 0x02, Digit2: 0x03, Digit3: 0x04, Digit4: 0x05, Digit5: 0x06, Digit6: 0x07, Digit7: 0x08, Digit8: 0x09, Digit9: 0x0a, Digit0: 0x0b,
  Minus: 0x0c, Equal: 0x0d, Backspace: 0x0e, Tab: 0x0f, KeyQ: 0x10, KeyW: 0x11, KeyE: 0x12, KeyR: 0x13, KeyT: 0x14, KeyY: 0x15, KeyU: 0x16, KeyI: 0x17, KeyO: 0x18, KeyP: 0x19,
  BracketLeft: 0x1a, BracketRight: 0x1b, Enter: 0x1c, ControlLeft: 0x1d, KeyA: 0x1e, KeyS: 0x1f, KeyD: 0x20, KeyF: 0x21, KeyG: 0x22, KeyH: 0x23, KeyJ: 0x24, KeyK: 0x25, KeyL: 0x26,
  Semicolon: 0x27, Quote: 0x28, Backquote: 0x29, ShiftLeft: 0x2a, Backslash: 0x2b, KeyZ: 0x2c, KeyX: 0x2d, KeyC: 0x2e, KeyV: 0x2f, KeyB: 0x30, KeyN: 0x31, KeyM: 0x32,
  Comma: 0x33, Period: 0x34, Slash: 0x35, ShiftRight: 0x36, NumpadMultiply: 0x37, AltLeft: 0x38, Space: 0x39, CapsLock: 0x3a,
  F1: 0x3b, F2: 0x3c, F3: 0x3d, F4: 0x3e, F5: 0x3f, F6: 0x40, F7: 0x41, F8: 0x42, F9: 0x43, F10: 0x44, NumLock: 0x45, ScrollLock: 0x46,
  Numpad7: 0x47, Numpad8: 0x48, Numpad9: 0x49, NumpadSubtract: 0x4a, Numpad4: 0x4b, Numpad5: 0x4c, Numpad6: 0x4d, NumpadAdd: 0x4e, Numpad1: 0x4f, Numpad2: 0x50, Numpad3: 0x51, Numpad0: 0x52, NumpadDecimal: 0x53,
  IntlBackslash: 0x56, F11: 0x57, F12: 0x58,
  NumpadEnter: 0xe01c, ControlRight: 0xe01d, NumpadDivide: 0xe035, PrintScreen: 0xe037, AltRight: 0xe038, Home: 0xe047, ArrowUp: 0xe048, PageUp: 0xe049, ArrowLeft: 0xe04b, ArrowRight: 0xe04d,
  End: 0xe04f, ArrowDown: 0xe050, PageDown: 0xe051, Insert: 0xe052, Delete: 0xe053, MetaLeft: 0xe05b, MetaRight: 0xe05c, ContextMenu: 0xe05d,
};
// X keysyms for the same keys (the RFB key event still carries one)
const SYM = {
  Escape: 0xff1b, Backspace: 0xff08, Tab: 0xff09, Enter: 0xff0d, NumpadEnter: 0xff8d, ShiftLeft: 0xffe1, ShiftRight: 0xffe2, ControlLeft: 0xffe3, ControlRight: 0xffe4,
  AltLeft: 0xffe9, AltRight: 0xfe03, MetaLeft: 0xffeb, MetaRight: 0xffec, CapsLock: 0xffe5, NumLock: 0xff7f, ScrollLock: 0xff14, ContextMenu: 0xff67, PrintScreen: 0xff61,
  Home: 0xff50, ArrowLeft: 0xff51, ArrowUp: 0xff52, ArrowRight: 0xff53, ArrowDown: 0xff54, PageUp: 0xff55, PageDown: 0xff56, End: 0xff57, Insert: 0xff63, Delete: 0xffff,
};
for (let i = 1; i <= 12; i++) SYM[`F${i}`] = 0xffbd + i;
// US layout, for typing text in: char -> [code, shift]
const TYPE = {};
'abcdefghijklmnopqrstuvwxyz'.split('').forEach((c) => { TYPE[c] = [`Key${c.toUpperCase()}`, false]; TYPE[c.toUpperCase()] = [`Key${c.toUpperCase()}`, true]; });
'1234567890'.split('').forEach((c) => { TYPE[c] = [`Digit${c}`, false]; });
Object.entries({ '!': 'Digit1', '@': 'Digit2', '#': 'Digit3', $: 'Digit4', '%': 'Digit5', '^': 'Digit6', '&': 'Digit7', '*': 'Digit8', '(': 'Digit9', ')': 'Digit0', _: 'Minus', '+': 'Equal', '{': 'BracketLeft', '}': 'BracketRight', ':': 'Semicolon', '"': 'Quote', '~': 'Backquote', '|': 'Backslash', '<': 'Comma', '>': 'Period', '?': 'Slash' }).forEach(([c, k]) => { TYPE[c] = [k, true]; });
Object.entries({ '-': 'Minus', '=': 'Equal', '[': 'BracketLeft', ']': 'BracketRight', ';': 'Semicolon', "'": 'Quote', '`': 'Backquote', '\\': 'Backslash', ',': 'Comma', '.': 'Period', '/': 'Slash', ' ': 'Space', '\n': 'Enter', '\t': 'Tab' }).forEach(([c, k]) => { TYPE[c] = [k, false]; });

export class Vnc {
  constructor(canvas, url, { onstate = () => {}, onname = () => {} } = {}) {
    this.c = canvas;
    this.ctx = canvas.getContext('2d');
    this.onstate = onstate;
    this.onname = onname;
    this.chunks = [];
    this.len = 0;
    this.waiting = null;
    this.extKeys = false;
    this.buttons = 0;
    this.down = new Map();
    this.closed = false;
    this.ws = new WebSocket(url, ['binary']);
    this.ws.binaryType = 'arraybuffer';
    this.ws.onopen = () => { this.onstate('connecting'); this.run().catch((e) => { if (!this.closed) this.onstate(`error: ${e.message}`); this.close(); }); };
    this.ws.onmessage = (e) => this.feed(new Uint8Array(e.data));
    this.ws.onclose = () => { if (!this.closed) this.onstate('disconnected'); this.closed = true; if (this.waiting) { const w = this.waiting; this.waiting = null; w.rej(new Error('closed')); } };
    this.ws.onerror = () => this.onstate('error');
    this.bind();
  }

  feed(b) {
    this.chunks.push(b);
    this.len += b.length;
    if (this.waiting && this.len >= this.waiting.n) { const w = this.waiting; this.waiting = null; w.res(); }
  }

  async read(n) {
    if (this.len < n) await new Promise((res, rej) => { this.waiting = { n, res, rej }; });
    this.len -= n;
    const first = this.chunks[0];
    if (first.length >= n) {
      // common case: no copy
      const out = first.subarray(0, n);
      if (first.length === n) this.chunks.shift(); else this.chunks[0] = first.subarray(n);
      return out;
    }
    const out = new Uint8Array(n);
    let off = 0;
    while (off < n) {
      const c = this.chunks[0];
      const take = Math.min(c.length, n - off);
      out.set(c.subarray(0, take), off);
      off += take;
      if (take === c.length) this.chunks.shift(); else this.chunks[0] = c.subarray(take);
    }
    return out;
  }

  async u8() { return (await this.read(1))[0]; }
  async u16() { const b = await this.read(2); return (b[0] << 8) | b[1]; }
  async u32() { const b = await this.read(4); return ((b[0] << 24) >>> 0) + (b[1] << 16) + (b[2] << 8) + b[3]; }
  async s32() { return (await this.u32()) | 0; }

  send(bytes) { if (this.ws.readyState === 1) this.ws.send(new Uint8Array(bytes)); }

  async run() {
    const ver = new TextDecoder().decode(await this.read(12));
    if (!ver.startsWith('RFB ')) throw new Error('not a VNC server');
    this.send([...new TextEncoder().encode('RFB 003.008\n')]);
    const n = await this.u8();
    if (n === 0) { const len = await this.u32(); throw new Error(new TextDecoder().decode(await this.read(len))); }
    const types = [...(await this.read(n))];
    if (!types.includes(1)) throw new Error('the VM console wants a password (not supported)');
    this.send([1]);
    if ((await this.u32()) !== 0) throw new Error('VNC security handshake failed');
    this.send([1]); // shared
    this.w = await this.u16(); this.h = await this.u16();
    await this.read(16); // server pixel format
    const nl = await this.u32();
    this.onname(new TextDecoder().decode(await this.read(nl)));
    this.resize(this.w, this.h);
    // 32 bpp, little endian, R G B X in memory = canvas RGBA order
    this.send([0, 0, 0, 0, 32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 0, 8, 16, 0, 0, 0]);
    const enc = [0, 1, -223, -258]; // raw, copyrect, desktop size, qemu extended key events
    const m = [2, 0, 0, enc.length];
    for (const e of enc) m.push((e >>> 24) & 255, (e >>> 16) & 255, (e >>> 8) & 255, e & 255);
    this.send(m);
    this.request(false);
    this.onstate('connected');
    for (;;) {
      const t = await this.u8();
      if (t === 0) await this.update();
      else if (t === 1) { await this.read(3); const k = await this.u16(); await this.read(k * 6); }
      else if (t === 2) { /* bell */ }
      else if (t === 3) { await this.read(3); const len = await this.u32(); await this.read(len); }
      else throw new Error(`unknown VNC message ${t}`);
    }
  }

  request(incremental) {
    this.send([3, incremental ? 1 : 0, 0, 0, 0, 0, this.w >> 8, this.w & 255, this.h >> 8, this.h & 255]);
  }

  resize(w, h) {
    this.w = w; this.h = h;
    this.c.width = w; this.c.height = h;
    this.ctx.fillStyle = '#000'; this.ctx.fillRect(0, 0, w, h);
  }

  async update() {
    await this.read(1);
    const n = await this.u16();
    for (let i = 0; i < n; i++) {
      const x = await this.u16(), y = await this.u16(), w = await this.u16(), h = await this.u16();
      const e = await this.s32();
      if (e === 0) {
        if (!w || !h) continue;
        const px = await this.read(w * h * 4);
        const img = new ImageData(w, h);
        img.data.set(px);
        for (let p = 3; p < img.data.length; p += 4) img.data[p] = 255;
        this.ctx.putImageData(img, x, y);
      } else if (e === 1) {
        const sx = await this.u16(), sy = await this.u16();
        this.ctx.drawImage(this.c, sx, sy, w, h, x, y, w, h);
      } else if (e === -223) {
        this.resize(w, h);
      } else if (e === -258) {
        this.extKeys = true;
      } else {
        throw new Error(`unsupported VNC encoding ${e}`);
      }
    }
    this.request(true);
  }

  key(code, down) {
    const sym = SYM[code] || 0;
    const xt = XT[code];
    if (this.extKeys && xt) {
      // QEMU extended key event: msg 255, sub 0, down, keysym, keycode (0xe0xx -> 0x80 | xx)
      const kc = xt > 0xff ? 0x80 | (xt & 0x7f) : xt;
      this.send([255, 0, 0, down ? 1 : 0, (sym >>> 24) & 255, (sym >>> 16) & 255, (sym >>> 8) & 255, sym & 255, 0, 0, 0, kc]);
    } else if (sym) {
      this.send([4, down ? 1 : 0, 0, 0, (sym >>> 24) & 255, (sym >>> 16) & 255, (sym >>> 8) & 255, sym & 255]);
    }
  }

  keysym(sym, down) { this.send([4, down ? 1 : 0, 0, 0, (sym >>> 24) & 255, (sym >>> 16) & 255, (sym >>> 8) & 255, sym & 255]); }

  pointer(ev) {
    const r = this.c.getBoundingClientRect();
    const x = Math.max(0, Math.min(this.w - 1, Math.round(((ev.clientX - r.left) / r.width) * this.w)));
    const y = Math.max(0, Math.min(this.h - 1, Math.round(((ev.clientY - r.top) / r.height) * this.h)));
    this.send([5, this.buttons, x >> 8, x & 255, y >> 8, y & 255]);
    return [x, y];
  }

  bind() {
    const c = this.c;
    c.tabIndex = 0;
    const kd = (e) => {
      if (this.closed) return;
      e.preventDefault();
      if (!XT[e.code] && !SYM[e.code]) {
        // a key we have no scancode for: send its character
        if (e.key.length === 1) { const s = e.key.codePointAt(0); this.down.set(e.code, s); this.keysym(s, true); }
        return;
      }
      this.key(e.code, true);
    };
    const ku = (e) => {
      if (this.closed) return;
      e.preventDefault();
      if (this.down.has(e.code)) { this.keysym(this.down.get(e.code), false); this.down.delete(e.code); return; }
      this.key(e.code, false);
    };
    const btn = (b) => (b === 0 ? 1 : b === 1 ? 2 : b === 2 ? 4 : 0);
    const md = (e) => { c.focus(); this.buttons |= btn(e.button); this.pointer(e); e.preventDefault(); };
    const mu = (e) => { this.buttons &= ~btn(e.button); this.pointer(e); e.preventDefault(); };
    const mm = (e) => this.pointer(e);
    const wh = (e) => {
      e.preventDefault();
      const b = e.deltaY < 0 ? 8 : 16;
      this.buttons |= b; this.pointer(e); this.buttons &= ~b; this.pointer(e);
    };
    const blur = () => { for (const code of Object.keys(XT)) if (code.startsWith('Shift') || code.startsWith('Control') || code.startsWith('Alt') || code.startsWith('Meta')) this.key(code, false); };
    c.addEventListener('keydown', kd); c.addEventListener('keyup', ku);
    c.addEventListener('mousedown', md); c.addEventListener('mouseup', mu); c.addEventListener('mousemove', mm);
    c.addEventListener('wheel', wh, { passive: false }); c.addEventListener('contextmenu', (e) => e.preventDefault());
    c.addEventListener('blur', blur);
    this.unbind = () => { c.removeEventListener('keydown', kd); c.removeEventListener('keyup', ku); c.removeEventListener('mousedown', md); c.removeEventListener('mouseup', mu); c.removeEventListener('mousemove', mm); c.removeEventListener('wheel', wh); c.removeEventListener('blur', blur); };
  }

  ctrlAltDel() {
    for (const k of ['ControlLeft', 'AltLeft', 'Delete']) this.key(k, true);
    for (const k of ['Delete', 'AltLeft', 'ControlLeft']) this.key(k, false);
  }

  /** Type text as key presses (US layout), e.g. a password. */
  async type(text) {
    for (const ch of text) {
      const t = TYPE[ch];
      if (!t) { const s = ch.codePointAt(0); this.keysym(s, true); this.keysym(s, false); continue; }
      if (t[1]) this.key('ShiftLeft', true);
      this.key(t[0], true); this.key(t[0], false);
      if (t[1]) this.key('ShiftLeft', false);
      await new Promise((r) => setTimeout(r, 8));
    }
  }

  close() {
    this.closed = true;
    if (this.unbind) this.unbind();
    try { this.ws.close(); } catch { /* gone */ }
  }
}

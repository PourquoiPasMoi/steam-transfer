'use strict';
/* Steam Transfer 2 - UI. Vanilla JS, no build step.
   Talks to the Rust side through window.__TAURI__ (or ui/mock.js when opened in a normal browser). */

const $ = (sel, root = document) => root.querySelector(sel);

/** Tiny DOM builder: h('div', {class: 'x', onclick}, child, 'text'). Text is always set safely. */
function h(tag, attrs = {}, ...children) {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs || {})) {
    if (v == null || v === false) continue;
    if (k.startsWith('on')) el.addEventListener(k.slice(2), v);
    else if (k === 'style' && typeof v === 'object') Object.assign(el.style, v);
    else if (k === 'dataset') Object.assign(el.dataset, v);
    else el.setAttribute(k, v === true ? '' : v);
  }
  for (const c of children.flat()) {
    if (c == null || c === false) continue;
    el.append(c instanceof Node ? c : document.createTextNode(String(c)));
  }
  return el;
}

const fmtBytes = (n) => {
  if (!n) return '0 B';
  const u = ['B', 'KB', 'MB', 'GB', 'TB'];
  let i = 0;
  while (n >= 1024 && i < u.length - 1) { n /= 1024; i++; }
  return i === 0 ? `${n} B` : `${n.toFixed(n >= 100 ? 0 : 1)} ${u[i]}`;
};
const fmtEta = (s) => {
  if (s == null || !isFinite(s)) return '';
  s = Math.round(s);
  const hh = Math.floor(s / 3600), mm = Math.floor((s % 3600) / 60), ss = s % 60;
  return hh ? `${hh}:${String(mm).padStart(2, '0')}:${String(ss).padStart(2, '0')}` : `${mm}:${String(ss).padStart(2, '0')}`;
};
const num = (n) => Number(n || 0).toLocaleString();
const cdn = (id, file) => `https://shared.cloudflare.steamstatic.com/store_item_assets/steam/apps/${id}/${file}`;
const buildNum = (b) => (b && /^\d+$/.test(b) ? Number(b) : null);

// ------------------------------------------------------------------------------------------ state

const S = {
  api: null,
  mode: 'backup',
  version: '',
  settings: {},
  games: [],
  binder: null,          // backup destination scan
  pkgByApp: {},
  restore: { binder: null, library: null, installed: {}, libraries: [], binders: [], loaded: false },
  move: { from: null, to: null, installed: {}, libraries: [] },
  dl: { list: [], dir: null, link: '', signature: '' },
  steamRunning: false,
  queue: { current: null, pending: [] },
  progress: null,
  lastResult: null,
  filter: 'all',
  search: '',
  spread: 0,
  layout: { cols: 3, rows: 2, perPage: 6, single: false },
  update: null,
  updating: null,        // { version, done, total, phase: 'download' | 'install' } while self-updating
  loading: true,
};

// ------------------------------------------------------------------------------------------ api

function tauriApi() {
  const T = window.__TAURI__;
  return {
    invoke: T.core.invoke,
    listen: T.event.listen,
    win: T.window.getCurrentWindow(),
  };
}

function loadScript(src) {
  return new Promise((resolve, reject) => {
    const s = document.createElement('script');
    s.src = src; s.onload = resolve; s.onerror = reject;
    document.head.append(s);
  });
}

const call = (cmd, args) => S.api.invoke(cmd, args);

// ------------------------------------------------------------------------------------------ items

function jobState(match) {
  const cur = S.queue.current;
  if (cur && match(cur.spec)) return { busy: true, kind: cur.spec.kind, id: cur.id };
  const idx = S.queue.pending.findIndex((j) => match(j.spec));
  if (idx >= 0) return { queued: idx + 1, id: S.queue.pending[idx].id };
  return null;
}

function backupItems() {
  return S.games.map((g) => {
    const pkg = S.pkgByApp[g.app_id] || null;
    const job = jobState((s) => (s.kind === 'backup' || s.kind === 'verify') && s.app_id === g.app_id);
    let state;
    if (job?.busy) state = 'busy';
    else if (job?.queued) state = 'queued';
    else if (!pkg) state = 'new';
    else if (pkg.status !== 'complete') state = 'resume';
    else if (g.build_id && pkg.build_id && g.build_id !== pkg.build_id) state = 'update';
    else state = 'done';
    return { key: `b-${g.app_id}`, app_id: g.app_id, name: g.name, size: g.size_on_disk, art: g.art_url, state, job, game: g, pkg };
  });
}

function restoreItems() {
  const packages = S.restore.binder?.packages || [];
  return packages.map((p) => {
    const inst = S.restore.installed[p.app_id];
    const job = jobState((s) => s.kind === 'restore' && s.package === p.path);
    let state;
    if (job?.busy) state = 'busy';
    else if (job?.queued) state = 'queued';
    else if (p.status !== 'complete') state = 'broken';
    else if (inst?.partial) state = 'resume';
    else if (!inst) state = 'new';
    else {
      const a = buildNum(p.build_id), b = buildNum(inst.build_id);
      if (a != null && b != null && a !== b) state = a > b ? 'update' : 'newer';
      else state = 'done';
    }
    return { key: `r-${p.app_id}`, app_id: p.app_id, name: p.name, size: p.bytes, art: p.art_url, state, job, pkg: p, inst };
  });
}

function moveItems() {
  const m = S.move;
  if (!m.from) return [];
  return S.games.filter((g) => samePath(g.library_root, m.from)).map((g) => {
    const job = jobState((s) => s.kind === 'move' && s.app_id === g.app_id);
    let state;
    if (job?.busy) state = 'busy';
    else if (job?.queued) state = 'queued';
    else if (m.installed[g.app_id]?.moving) state = 'resume';
    else state = 'new';
    return { key: `m-${g.app_id}`, app_id: g.app_id, name: g.name, size: g.size_on_disk, art: g.art_url, state, job, game: g };
  });
}

const TORRENT_ACTIVE = ['fetching', 'checking', 'downloading'];

function downloadItems() {
  return S.dl.list.map((t) => ({
    key: `d-${t.key}`, app_id: null, name: t.name, size: t.total_bytes, art: null, state: t.state, torrent: t,
  }));
}

const FILTERS = {
  backup: [
    ['all', 'All', () => true],
    ['new', 'Not backed up', (i) => i.state === 'new'],
    ['done', 'Backed up', (i) => i.state === 'done'],
    ['attention', 'Needs update', (i) => i.state === 'update' || i.state === 'resume'],
  ],
  download: [
    ['all', 'All', () => true],
    ['active', 'Downloading', (i) => TORRENT_ACTIVE.includes(i.state) || i.state === 'paused'],
    ['done', 'Done', (i) => i.state === 'seeding'],
  ],
  move: [
    ['all', 'All', () => true],
    ['attention', 'Unfinished', (i) => i.state === 'resume'],
  ],
  restore: [
    ['all', 'All', () => true],
    ['new', 'Not installed', (i) => i.state === 'new'],
    ['done', 'Installed', (i) => i.state === 'done' || i.state === 'newer'],
    ['attention', 'Needs update', (i) => i.state === 'update' || i.state === 'resume'],
  ],
};

function allItems() {
  return { backup: backupItems, restore: restoreItems, move: moveItems, download: downloadItems }[S.mode]();
}

function visibleItems() {
  const filter = FILTERS[S.mode].find((f) => f[0] === S.filter) || FILTERS[S.mode][0];
  const q = S.search.trim().toLowerCase();
  return allItems().filter((i) => filter[2](i) && (!q || i.name.toLowerCase().includes(q)));
}

const STICKERS = {
  backup: { done: ['BACKED UP ✓', 'done'], update: ['UPDATE', 'update'], resume: ['UNFINISHED', 'resume'] },
  move: { resume: ['UNFINISHED', 'resume'] },
  download: {
    fetching: ['FETCHING INFO', 'queued'], checking: ['CHECKING', 'resume'], downloading: ['DOWNLOADING', 'busy'],
    paused: ['PAUSED', 'muted'], seeding: ['DONE ✓', 'done'], error: ['ERROR', 'error'],
  },
  restore: { done: ['INSTALLED ✓', 'done'], update: ['UPDATE', 'update'], resume: ['UNFINISHED', 'resume'], newer: ['PC NEWER', 'muted'], broken: ['INCOMPLETE', 'muted'] },
};
const ACTIONS = {
  backup: { new: 'Back up →', resume: 'Resume →', update: 'Update →', done: 'Details' },
  move: { new: 'Move →', resume: 'Resume →' },
  download: { fetching: 'Pause', checking: 'Pause', downloading: 'Pause', paused: 'Resume', seeding: 'Open', error: 'Remove' },
  restore: { new: 'Restore →', resume: 'Resume →', update: 'Update →', done: 'Details', newer: 'Details', broken: 'Unfinished' },
};

function stickerFor(item) {
  if (item.torrent) return STICKERS.download[item.state] || null;
  if (item.state === 'busy') {
    const verb = { verify: 'CHECKING', restore: 'RESTORING', move: 'MOVING' }[item.job.kind] || 'COPYING';
    return [verb, 'busy'];
  }
  if (item.state === 'queued') return [`QUEUED #${item.job.queued}`, 'queued'];
  return STICKERS[S.mode][item.state] || null;
}

// ------------------------------------------------------------------------------------------ disc art

function discEl(item, cls = 'disc') {
  if (item.torrent) {
    const blank = () => h('div', { class: `${cls} blank` }, h('div', { class: 'initials dark' }, initials(item.name) || '♪'));
    if (!item.torrent.cover) return blank();
    const disc = h('div', { class: cls });
    const img = h('img', { alt: '', draggable: 'false', decoding: 'async', src: item.torrent.cover });
    img.onerror = () => disc.replaceWith(blank());
    disc.append(img);
    return disc;
  }
  const disc = h('div', { class: cls });
  const sources = [item.art, cdn(item.app_id, 'library_600x900.jpg'), cdn(item.app_id, 'header.jpg')].filter(Boolean);
  const img = h('img', { alt: '', draggable: 'false', loading: 'lazy', decoding: 'async' });
  let n = 0;
  img.onerror = () => {
    n++;
    if (n < sources.length) img.src = sources[n];
    else {
      img.remove();
      disc.classList.add('placeholder');
      disc.append(h('div', { class: 'initials' }, initials(item.name)));
    }
  };
  img.src = sources[0];
  disc.append(img);
  return disc;
}
const initials = (name) => name.replace(/[^\p{L}\p{N} ]/gu, ' ').split(/\s+/).filter(Boolean).slice(0, 2).map((w) => w[0]).join('').toUpperCase();
const artSources = (item) => [item.art, cdn(item.app_id, 'library_600x900.jpg')].filter(Boolean);

// ------------------------------------------------------------------------------------------ layout & pages

const STRIP_H = 58, COL_GAP = 14, ROW_GAP = 16, MIN_W = 148, MAX_W = 210;

/** Fit as many rows as possible while keeping sleeves at least MIN_W wide, then fill columns. */
function computeLayout() {
  const binder = $('#binder');
  const single = binder.clientWidth < 980;
  binder.classList.toggle('single-page', single);
  $('#pageLeft').classList.toggle('single', single);
  const page = $('#pageLeft');
  const w = page.clientWidth - 36;
  const hgt = page.clientHeight - 18 - 30;
  let rows = 1;
  for (let r = 2; r <= 6; r++) {
    if ((hgt - (r - 1) * ROW_GAP) / r - STRIP_H >= MIN_W) rows = r; else break;
  }
  const size = Math.max(120, Math.min(MAX_W, Math.floor((hgt - (rows - 1) * ROW_GAP) / rows - STRIP_H)));
  const cols = Math.max(1, Math.floor((w + COL_GAP) / (size + COL_GAP)));
  const changed = cols !== S.layout.cols || rows !== S.layout.rows || single !== S.layout.single || size !== S.layout.size;
  S.layout = { cols, rows, perPage: cols * rows, single, size };
  for (const p of [$('#pageLeft'), $('#pageRight')]) {
    p.style.setProperty('--cols', cols);
    p.style.setProperty('--sleeve-w', `${size}px`);
  }
  return changed;
}

function perSpread() {
  return S.layout.perPage * (S.layout.single ? 1 : 2);
}

let turning = null;      // running page-turn animation
let queuedFlip = 0;      // flips requested while a turn is running

function flip(dir) {
  if (turning) { queuedFlip = dir; turning.speedUp(); return; }
  const items = visibleItems();
  const spreads = Math.max(1, Math.ceil(items.length / perSpread()));
  const next = Math.min(spreads - 1, Math.max(0, S.spread + dir));
  if (next === S.spread) return;

  const reduced = window.matchMedia('(prefers-reduced-motion: reduce)').matches;
  // Snapshot the pages as they look now, then render the new spread underneath.
  const oldLeft = snapshot($('#pageLeft')), oldRight = snapshot($('#pageRight'));
  S.spread = next;
  renderPages(true);
  if (reduced) return;

  const left = $('#pageLeft'), right = $('#pageRight');
  let plan;
  if (S.layout.single) {
    // One page: the sheet lifts off the spine on the left and turns away (or comes back for "previous").
    plan = dir > 0
      ? { anchor: 'left', page: left, front: oldLeft, back: null, cover: null, reverse: false }
      : { anchor: 'left', page: left, front: snapshot(left), back: null, cover: { page: left, el: oldLeft }, reverse: true };
  } else if (dir > 0) {
    // Right-hand sheet turns over the spine onto the left side.
    plan = { anchor: 'left', page: right, front: oldRight, back: snapshot(left), cover: { page: left, el: oldLeft }, reverse: false };
  } else {
    // Left-hand sheet turns back onto the right side.
    plan = { anchor: 'right', page: left, front: oldLeft, back: snapshot(right), cover: { page: right, el: oldRight }, reverse: false };
  }
  turning = turnPage(plan);
  turning.done.then(() => {
    turning = null;
    if (queuedFlip) { const d = queuedFlip; queuedFlip = 0; flip(d); }
  });
}

/** Static copy of a page (no ids, no entry animations) to paint onto the turning sheet. */
function snapshot(page) {
  const copy = page.cloneNode(true);
  copy.removeAttribute('id');
  copy.classList.add('snap');
  copy.querySelectorAll('[id]').forEach((n) => n.removeAttribute('id'));
  return copy;
}

const clamp = (v, lo, hi) => Math.min(hi, Math.max(lo, v));
const easeInOut = (t) => (t < 0.5 ? 4 * t * t * t : 1 - Math.pow(-2 * t + 2, 3) / 2);

/**
 * Curling page turn. The sheet is cut into vertical strips chained in 3D, each rotated a little
 * more than the one before it, so the sheet bends like paper: the free edge leads, the part near
 * the spine follows. Front strips show the old page, back strips show the page being revealed.
 */
function turnPage({ anchor, page, front, back, cover, reverse }) {
  const spread = $('#spread');
  const base = spread.getBoundingClientRect();
  const r = page.getBoundingClientRect();
  const W = r.width, H = r.height;
  const N = 12;
  const w = W / N;
  const sign = anchor === 'left' ? -1 : 1;

  const layer = h('div', { class: 'turn-layer' });
  const place = (el, rect) => Object.assign(el.style, { left: `${rect.left - base.left}px`, top: `${rect.top - base.top}px`, width: `${rect.width}px`, height: `${rect.height}px` });

  // The old page that stays put under the turning sheet until it lands on it.
  if (cover) {
    const holder = h('div', { class: 'turn-cover' });
    place(holder, cover.page.getBoundingClientRect());
    cover.el.style.width = '100%';
    cover.el.style.height = '100%';
    holder.append(cover.el);
    layer.append(holder);
  }
  // Shadow the sheet casts on the page it is lifting away from.
  const shadow = h('div', { class: `turn-shadow from-${anchor}` });
  place(shadow, r);
  layer.append(shadow);

  const leaf = h('div', { class: 'leaf' });
  place(leaf, r);
  layer.append(leaf);

  const strips = [];
  let parent = leaf;
  for (let k = 0; k < N; k++) {
    const strip = h('div', { class: 'strip-3d' });
    strip.style.width = `${w}px`;
    strip.style.height = `${H}px`;
    if (anchor === 'left') {
      strip.style.left = k === 0 ? '0px' : `${w}px`;
      strip.style.transformOrigin = 'left center';
    } else {
      strip.style.left = k === 0 ? `${W - w}px` : `${-w}px`;
      strip.style.transformOrigin = 'right center';
    }
    // which horizontal slice of the page each face shows (see derivation in the comment above)
    const frontX = anchor === 'left' ? k * w : W - (k + 1) * w;
    const backX = anchor === 'left' ? W - (k + 1) * w : k * w;
    const face = (src, x, cls) => {
      const f = h('div', { class: `face ${cls}` });
      f.style.width = `${w + 1}px`;
      if (src) {
        const c = k === 0 ? src : src.cloneNode(true);
        Object.assign(c.style, { position: 'absolute', left: `${-x}px`, top: '0px', width: `${W}px`, height: `${H}px`, margin: '0' });
        f.append(c);
      } else {
        f.classList.add('blank');
      }
      const shade = h('div', { class: 'shade' });
      f.append(shade);
      return { f, shade };
    };
    const fr = face(front, frontX, 'front');
    const bk = face(back, backX, 'back');
    strip.append(fr.f, bk.f);
    parent.append(strip);
    strips.push({ el: strip, frontShade: fr.shade, backShade: bk.shade });
    parent = strip;
  }
  // The first strip took the original snapshot nodes; later strips cloned them.
  spread.append(layer);

  let duration = 820;
  let start = null;
  let resolve;
  const done = new Promise((res) => (resolve = res));

  function frame(now) {
    if (start == null) start = now;
    let t = clamp((now - start) / duration, 0, 1);
    const p = reverse ? 1 - t : t;
    const e = easeInOut(p);
    const bend = Math.sin(Math.PI * p) * 62;              // how much the paper curls mid-turn
    const edge = clamp(180 * e + bend * 0.75, 0, 180);     // free edge leads...
    const spine = clamp(180 * e - bend * 0.55, 0, 180);    // ...the part at the rings lags
    let prev = 0;
    for (let k = 0; k < N; k++) {
      const A = spine + (edge - spine) * Math.pow(k / (N - 1), 1.35);
      const s = strips[k];
      s.el.style.transform = `rotateY(${sign * (A - prev)}deg)`;
      prev = A;
      const lit = Math.sin((A * Math.PI) / 180);
      // light comes from the top-left: the rising front darkens, the back is bright then settles
      s.frontShade.style.opacity = (lit * 0.55).toFixed(3);
      s.backShade.style.opacity = (lit * 0.35 + (A > 90 ? 0 : 0.2)).toFixed(3);
    }
    shadow.style.opacity = (Math.sin(Math.PI * p) * 0.75).toFixed(3);
    if (t < 1) requestAnimationFrame(frame);
    else { layer.remove(); resolve(); }
  }
  requestAnimationFrame(frame);

  return {
    done,
    // a second click while turning finishes this turn quickly, then plays the next one
    speedUp() {
      if (start == null) return;
      const now = performance.now();
      const t = (now - start) / duration;
      duration = Math.max(260, duration * 0.45);
      start = now - t * duration;
    },
  };
}

// ------------------------------------------------------------------------------------------ render: binder

function renderPages(quiet) {
  const items = visibleItems();
  const ps = perSpread();
  const spreads = Math.max(1, Math.ceil(items.length / ps));
  if (S.spread >= spreads) S.spread = spreads - 1;
  const start = S.spread * ps;
  const left = $('#pageLeft'), right = $('#pageRight');
  left.replaceChildren();
  right.replaceChildren();

  const empty = emptyState(items);
  if (empty) {
    left.append(empty);
  } else {
    items.slice(start, start + S.layout.perPage).forEach((it, i) => left.append(sleeveEl(it, i)));
    if (!S.layout.single) items.slice(start + S.layout.perPage, start + ps).forEach((it, i) => right.append(sleeveEl(it, i + S.layout.perPage)));
  }

  // During a page turn the sleeves are already "in place", so skip their drop-in animation.
  for (const p of [left, right]) p.classList.toggle('quiet', !!quiet);
  $('#prevPage').disabled = S.spread <= 0;
  $('#nextPage').disabled = S.spread >= spreads - 1;
  const pages = Math.max(1, Math.ceil(items.length / S.layout.perPage));
  const first = S.spread * (S.layout.single ? 1 : 2) + 1;
  const last = Math.min(pages, first + (S.layout.single ? 0 : 1));
  $('#pageCount').textContent = items.length ? (first === last ? `page ${first} of ${pages}` : `pages ${first}–${last} of ${pages}`) : '';
}

function emptyState(items) {
  if (S.loading) return h('div', { class: 'page-empty' }, h('div', { class: 'note' }, h('h3', {}, 'Flipping through your library…'), h('p', {}, 'Looking for Steam and your games.')));
  if (S.mode === 'backup') {
    if (!S.games.length) return note('No Steam games found', 'Steam Transfer couldn\'t find any installed Steam games on this PC. If Steam is installed somewhere unusual, start Steam once and press Refresh.', [h('button', { class: 'btn primary', onclick: refreshAll }, 'Refresh')]);
  } else if (S.mode === 'download') {
    if (!S.dl.list.length) return note('Paste a magnet link', 'Paste a magnet link in the box above (or open a .torrent file), pick where to save it and press Download. Progress, speed and peers show up here, and unfinished downloads carry on when you reopen the app.', []);
  } else if (S.mode === 'move') {
    if (S.move.libraries.length < 2) return note('You need a second Steam library', 'Moving works between Steam libraries on this PC. Pick the other drive and the app creates a Steam library there and adds it to Steam for you (Steam needs to be closed).', [h('button', { class: 'btn', onclick: () => newLibrary('move') }, 'New Steam library…'), h('button', { class: 'btn ghost', onclick: refreshMove }, 'Refresh')]);
    if (!items.length && !S.search && S.filter === 'all') return note('No games in this library', 'Pick another library in the From list.', []);
  } else {
    if (!S.restore.binder) return note('Where are your discs?', 'Plug in the drive that has your backups and pick it. The app looks for its "Steam Transfer" folder.', [h('button', { class: 'btn blue', onclick: chooseRestoreSource }, 'Choose backup drive')]);
    if (!S.restore.binder.packages.length) return note('This binder is empty', `There are no backups in ${S.restore.binder.root}. Back up some games on the other PC first, or pick a different drive.`, [h('button', { class: 'btn blue', onclick: chooseRestoreSource }, 'Choose another drive')]);
  }
  if (!items.length) return note('Nothing here', S.search ? `No games match "${S.search}".` : 'No games match this filter.', [h('button', { class: 'btn', onclick: () => { S.search = ''; $('#search').value = ''; S.filter = 'all'; renderAll(); } }, 'Show everything')]);
  return null;
}

function note(title, text, actions = []) {
  return h('div', { class: 'page-empty' }, h('div', { class: 'note' }, h('h3', {}, title), h('p', {}, text), h('div', { class: 'actions' }, actions)));
}

function torrentPct(t) {
  return t.total_bytes ? Math.min(100, (t.progress_bytes / t.total_bytes) * 100) : 0;
}

function torrentMeta(t) {
  if (t.state === 'downloading') return `${torrentPct(t).toFixed(0)}% · ${fmtBytes(t.down_bps)}/s`;
  if (t.state === 'fetching') return 'finding peers…';
  if (t.state === 'checking') return `checking ${torrentPct(t).toFixed(0)}%`;
  if (t.state === 'paused') return `${torrentPct(t).toFixed(0)}% · paused`;
  if (t.state === 'error') return 'failed';
  return fmtBytes(t.total_bytes);
}

function torrentSleeveEl(item, i) {
  const t = item.torrent;
  const sticker = stickerFor(item);
  const showRing = t.state !== 'seeding' && t.state !== 'fetching';
  const pocket = h('div', { class: 'pocket' },
    showRing ? h('div', { class: 'ring', style: `--p:${torrentPct(t)}; --ring-color: ${t.state === 'paused' ? 'var(--grey)' : 'var(--violet)'}` }) : null,
    discEl(item));
  const action = h('button', {
    class: 'strip-action download',
    onclick: (e) => { e.stopPropagation(); torrentPrimary(t); },
  }, ACTIONS.download[t.state] || 'Details');
  return h('div', {
    class: `sleeve torrent ${t.state === 'downloading' || t.state === 'fetching' ? 'busy' : ''} ${t.state === 'error' ? 'dim' : ''}`,
    role: 'button', tabindex: '0', style: `--i:${i}`, dataset: { key: item.key }, title: t.name,
    onclick: () => openTorrentCase(t),
    onkeydown: (e) => { if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); openTorrentCase(t); } },
  },
    pocket,
    sticker ? h('div', { class: `sticker ${sticker[1]}` }, sticker[0]) : null,
    h('div', { class: 'strip' },
      h('div', { class: 'strip-title' }, t.name),
      h('div', { class: 'strip-meta' }, h('span', { class: 'meta-text' }, torrentMeta(t)), action)));
}

function sleeveEl(item, i) {
  if (item.torrent) return torrentSleeveEl(item, i);
  const pct = item.state === 'busy' && S.progress && S.progress.id === item.job.id ? progressPct(S.progress) : 0;
  const sticker = stickerFor(item);
  const pocket = h('div', { class: 'pocket' },
    item.state === 'busy' ? h('div', { class: 'ring', style: `--p:${pct}` }) : null,
    discEl(item),
  );
  const actionLabel = item.state === 'busy' ? `${pct.toFixed(0)}%` : item.state === 'queued' ? 'Queued' : ACTIONS[S.mode][item.state];
  const action = h('button', {
    class: `strip-action ${S.mode === 'restore' ? 'restore' : S.mode === 'move' ? 'move' : ''}`,
    disabled: item.state === 'busy' || item.state === 'queued' || item.state === 'broken',
    onclick: (e) => {
      e.stopPropagation();
      if (item.state === 'done' || item.state === 'newer') openCase(item);
      else primaryAction(item);
    },
  }, actionLabel);
  const el = h('div', {
    class: `sleeve ${item.state === 'busy' ? 'busy' : ''} ${item.state === 'broken' ? 'dim' : ''}`,
    role: 'button',
    tabindex: '0',
    style: `--i:${i}`,
    dataset: { key: item.key },
    title: item.name,
    onclick: () => openCase(item),
    onkeydown: (e) => { if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); openCase(item); } },
  },
    pocket,
    sticker ? h('div', { class: `sticker ${sticker[1]}` }, sticker[0]) : null,
    h('div', { class: 'strip' },
      h('div', { class: 'strip-title' }, item.name),
      h('div', { class: 'strip-meta' }, h('span', {}, fmtBytes(item.size)), action),
    ),
  );
  return el;
}

// ------------------------------------------------------------------------------------------ render: chrome

function renderFilters() {
  const items = allItems();
  const bar = $('#filters');
  bar.replaceChildren(...FILTERS[S.mode].map(([key, label, fn]) => h('button', {
    class: `chip ${S.filter === key ? 'active' : ''}`,
    onclick: () => { S.filter = key; S.spread = 0; renderAll(); },
  }, label, h('span', { class: 'count' }, items.filter(fn).length))));
}

function driveEl(path, free, missing) {
  return h('div', { class: `drive ${missing ? 'missing' : ''}`, title: path },
    h('span', { class: 'drive-icon' }),
    h('span', { class: 'drive-path' }, path),
    free != null ? h('span', { class: 'drive-free' }, `${fmtBytes(free)} free`) : null,
  );
}

function renderLocation() {
  const loc = $('#location');
  loc.replaceChildren();
  if (S.mode === 'backup') {
    if (!S.binder) {
      appendAll(loc,
        h('div', { class: 'loc-group' }, h('span', { class: 'loc-label' }, 'Back up to'),
          h('button', { class: 'btn primary', onclick: chooseDestination }, 'Choose a backup drive…')),
        h('div', { class: 'loc-stats' }, h('span', {}, h('b', {}, S.games.length), ' games on this PC')),
      );
      return;
    }
    const items = backupItems();
    const count = (st) => items.filter((i) => i.state === st).length;
    appendAll(loc,
      h('div', { class: 'loc-group' }, h('span', { class: 'loc-label' }, 'Back up to'),
        driveEl(S.binder.root, S.binder.free_bytes, S.binder.free_bytes == null),
        h('button', { class: 'btn', onclick: chooseDestination }, 'Change'),
        h('button', { class: 'btn', onclick: () => call('open_folder', { path: S.binder.root }).catch(showError) }, 'Open'),
        h('button', { class: 'btn ghost', onclick: refreshAll, title: 'Rescan Steam and the drive' }, '↻ Refresh')),
      h('div', { class: 'loc-stats' },
        h('span', {}, h('b', {}, count('done')), ` of ${items.length} backed up`),
        count('update') ? h('span', {}, h('b', {}, count('update')), ' need update') : null,
        count('resume') ? h('span', {}, h('b', {}, count('resume')), ' unfinished') : null),
    );
  } else if (S.mode === 'restore') {
    const r = S.restore;
    const sourceOptions = [...new Set([...(r.binder ? [r.binder.root] : []), ...r.binders.map((b) => b.path)])];
    const sourceSel = h('select', { class: 'select', onchange: (e) => e.target.value === '__browse' ? chooseRestoreSource() : loadRestoreSource(e.target.value) },
      ...sourceOptions.map((p) => h('option', { value: p, selected: r.binder && p === r.binder.root }, p)),
      h('option', { value: '__browse' }, 'Browse…'));
    const libOptions = uniquePaths([...(r.library ? [r.library] : []), ...r.libraries.map((l) => l.path)]);
    const libSel = h('select', {
      class: 'select',
      onchange: (e) => e.target.value === '__browse' ? chooseRestoreTarget() : e.target.value === '__new' ? newLibrary('restore') : setRestoreTarget(e.target.value),
    },
      ...libOptions.map((p) => h('option', { value: p, selected: samePath(p, r.library) }, libraryLabel(p, r.libraries))),
      h('option', { value: '__new' }, '＋ New Steam library on another drive…'),
      h('option', { value: '__browse' }, 'Browse…'));
    const items = restoreItems();
    appendAll(loc,
      h('div', { class: 'loc-group' }, h('span', { class: 'loc-label' }, 'From'),
        sourceOptions.length ? sourceSel : h('button', { class: 'btn blue', onclick: chooseRestoreSource }, 'Choose backup drive…')),
      arrowEl(),
      h('div', { class: 'loc-group' }, h('span', { class: 'loc-label' }, 'Into'),
        libOptions.length ? libSel : h('button', { class: 'btn', onclick: chooseRestoreTarget }, 'Choose Steam library…'),
        h('button', { class: 'btn ghost', onclick: refreshRestore, title: 'Rescan' }, '↻')),
      S.steamRunning ? h('div', { class: 'warn' }, '⚠ Steam is running. Exit it (tray icon too) before restoring.') : null,
      h('div', { class: 'loc-stats' },
        h('span', {}, h('b', {}, items.length), ' discs'),
        h('span', {}, h('b', {}, items.filter((i) => i.state === 'done' || i.state === 'newer').length), ' installed')),
    );
  } else if (S.mode === 'download') {
    const input = h('input', {
      class: 'magnet', type: 'text', placeholder: 'Paste a magnet link…', spellcheck: 'false', value: S.dl.link,
      oninput: (e) => { S.dl.link = e.target.value; },
      onkeydown: (e) => { if (e.key === 'Enter') startDownload(); },
    });
    const totals = S.dl.list.reduce((a, t) => ({ down: a.down + t.down_bps, up: a.up + t.up_bps, active: a.active + (TORRENT_ACTIVE.includes(t.state) ? 1 : 0) }), { down: 0, up: 0, active: 0 });
    appendAll(loc,
      h('label', { class: 'magnet-wrap' }, h('span', { class: 'magnet-icon', 'aria-hidden': 'true' }, '🧲'), input),
      h('div', { class: 'loc-group' }, h('span', { class: 'loc-label' }, 'Save to'),
        driveEl(S.dl.dir || '…', null, false),
        h('button', { class: 'btn', onclick: chooseDownloadDir }, 'Change')),
      h('button', { class: 'btn violet', onclick: startDownload }, 'Download'),
      h('button', { class: 'btn ghost', onclick: openTorrentFile }, 'Open .torrent'),
      h('button', { class: 'btn ghost', onclick: openSpeedSettings, title: 'Download and upload speed limits' }, '⚙ Speed'),
      h('div', { class: 'loc-stats' },
        h('span', {}, '↓ ', h('b', {}, `${fmtBytes(totals.down)}/s`), limitText(S.settings.torrent_down_kbps)),
        h('span', {}, '↑ ', h('b', {}, `${fmtBytes(totals.up)}/s`), limitText(S.settings.torrent_up_kbps)),
        h('span', {}, h('b', {}, totals.active), ' active')),
    );
  } else {
    const m = S.move;
    const libs = m.libraries;
    if (libs.length < 2) {
      appendAll(loc,
        h('div', { class: 'loc-group' }, h('span', { class: 'loc-label' }, 'Move between'), h('span', { class: 'drive-free' }, `${libs.length} Steam library found`)),
        h('button', { class: 'btn', onclick: () => newLibrary('move') }, '＋ New Steam library…'),
        h('button', { class: 'btn ghost', onclick: refreshMove, title: 'Rescan' }, '↻ Refresh'));
      return;
    }
    const countIn = (lib) => S.games.filter((g) => samePath(g.library_root, lib)).length;
    const fromSel = h('select', { class: 'select', onchange: (e) => setMoveFrom(e.target.value) },
      ...libs.map((l) => h('option', { value: l.path, selected: samePath(l.path, m.from) }, `${l.path}  (${countIn(l.path)} games)`)));
    const toSel = h('select', { class: 'select', onchange: (e) => e.target.value === '__new' ? newLibrary('move') : setMoveTo(e.target.value) },
      ...libs.filter((l) => !samePath(l.path, m.from)).map((l) => h('option', { value: l.path, selected: samePath(l.path, m.to) }, libraryLabel(l.path, libs))),
      h('option', { value: '__new' }, '＋ New Steam library on another drive…'));
    const items = moveItems();
    appendAll(loc,
      h('div', { class: 'loc-group' }, h('span', { class: 'loc-label' }, 'From'), fromSel),
      arrowEl(),
      h('div', { class: 'loc-group' }, h('span', { class: 'loc-label' }, 'Into'), toSel,
        h('button', { class: 'btn ghost', onclick: refreshMove, title: 'Rescan' }, '↻')),
      S.steamRunning ? h('div', { class: 'warn' }, '⚠ Steam is running. Exit it (tray icon too) before moving.') : null,
      h('div', { class: 'loc-stats' }, h('span', {}, h('b', {}, items.length), ' games in this library')),
    );
  }
}

/** append() turns null into the text "null", so drop empty slots first. */
function appendAll(parent, ...children) {
  parent.append(...children.filter((c) => c != null && c !== false));
}

function arrowEl() {
  const span = h('span', { class: 'arrow', 'aria-hidden': 'true' });
  span.innerHTML = '<svg viewBox="0 0 26 12"><path d="M1 6h22M18 1.5 23 6l-5 4.5"/></svg>';
  return span;
}

const samePath = (a, b) => !!a && !!b && a.replace(/[\\/]+$/, '').toLowerCase() === b.replace(/[\\/]+$/, '').toLowerCase();
function uniquePaths(paths) {
  const out = [];
  for (const p of paths) if (p && !out.some((q) => samePath(p, q))) out.push(p);
  return out;
}

function progressPct(p) {
  return p && p.bytes_total ? Math.min(100, (p.bytes_done / p.bytes_total) * 100) : 0;
}

function renderDeck() {
  if (S.updating) return renderUpdateDeck();
  const cur = S.queue.current;
  const tray = $('#trayDisc');
  tray.classList.remove('update-disc', 'cdr-disc');
  const p = S.progress && cur && S.progress.id === cur.id ? S.progress : null;
  $('#stopBtn').disabled = !cur;
  if (cur) {
    const gameItem = { app_id: cur.app_id, art: artFor(cur.app_id) };
    tray.style.backgroundImage = artSources(gameItem).map((s) => `url("${s}")`).join(', ');
    tray.classList.add('loaded', 'spinning');
    const verb = { backup: 'BACKING UP', restore: 'RESTORING', verify: 'CHECKING', move: 'MOVING' }[cur.spec.kind];
    $('#lcdTitle').textContent = `${verb}  ${cur.title}`;
    const pct = progressPct(p);
    $('#lcdPct').textContent = p ? `${pct.toFixed(1)}%` : '';
    $('#lcdFill').style.width = `${pct}%`;
    if (p) {
      const stage = (p.stage || '').toUpperCase();
      const speed = p.speed > 0 ? `  ${fmtBytes(p.speed)}/S` : '';
      $('#lcdStats').textContent = `${stage}  ${num(p.files_done)}/${num(p.files_total)} FILES  ${fmtBytes(p.bytes_done)} / ${fmtBytes(p.bytes_total)}${speed}`;
      $('#lcdEta').textContent = p.eta_secs != null && p.eta_secs > 0 ? `ETA ${fmtEta(p.eta_secs)}` : '';
      $('#lcdFile').textContent = p.file || '';
    } else {
      $('#lcdStats').textContent = 'READING THE FILE LIST…';
      $('#lcdEta').textContent = '';
      $('#lcdFile').textContent = '';
    }
  } else {
    tray.classList.remove('spinning');
    const last = S.lastResult;
    const activeTorrents = S.dl.list.filter((t) => t.state === 'downloading');
    if (activeTorrents.length && !(last && Date.now() - last.at < 8000)) {
      const top = activeTorrents.slice().sort((a, b) => b.down_bps - a.down_bps)[0];
      const down = activeTorrents.reduce((a, t) => a + t.down_bps, 0);
      const up = S.dl.list.reduce((a, t) => a + t.up_bps, 0);
      tray.style.backgroundImage = top.cover ? `url("${top.cover}")` : '';
      tray.classList.add('loaded', 'spinning');
      tray.classList.toggle('cdr-disc', !top.cover);
      $('#lcdTitle').textContent = activeTorrents.length > 1 ? `DOWNLOADING ${activeTorrents.length} TORRENTS` : `DOWNLOADING  ${top.name}`;
      $('#lcdPct').textContent = `${torrentPct(top).toFixed(1)}%`;
      $('#lcdFill').style.width = `${torrentPct(top)}%`;
      $('#lcdStats').textContent = `↓ ${fmtBytes(down)}/S  ↑ ${fmtBytes(up)}/S  ${top.peers} PEERS  ${fmtBytes(top.progress_bytes)} / ${fmtBytes(top.total_bytes)}`;
      $('#lcdEta').textContent = top.eta_secs ? `ETA ${fmtEta(top.eta_secs)}` : '';
      $('#lcdFile').textContent = activeTorrents.length > 1 ? top.name : '';
      return renderQueueList();
    }
    tray.classList.remove('cdr-disc');
    if (last && Date.now() - last.at < 60000) {
      tray.style.backgroundImage = artSources({ app_id: last.app_id, art: artFor(last.app_id) }).map((s) => `url("${s}")`).join(', ');
      tray.classList.add('loaded');
      $('#lcdTitle').textContent = `${last.outcome === 'success' ? 'DONE ✓' : last.outcome === 'cancelled' ? 'STOPPED' : 'ERROR'}  ${last.title}`;
      $('#lcdPct').textContent = last.outcome === 'success' ? '100%' : '';
      $('#lcdFill').style.width = last.outcome === 'success' ? '100%' : '0%';
      $('#lcdStats').textContent = last.message;
    } else {
      tray.style.backgroundImage = '';
      tray.classList.remove('loaded');
      $('#lcdTitle').textContent = 'READY';
      $('#lcdPct').textContent = '';
      $('#lcdFill').style.width = '0%';
      $('#lcdStats').textContent = { backup: 'PICK A GAME TO BACK IT UP', restore: 'PICK A DISC TO RESTORE IT', move: 'PICK A GAME TO MOVE IT', download: 'PASTE A MAGNET LINK TO START' }[S.mode];
    }
    $('#lcdEta').textContent = '';
    $('#lcdFile').textContent = '';
  }

  renderQueueList();
}

function renderQueueList() {
  const list = $('#queueList');
  list.replaceChildren();
  if (!S.queue.pending.length) list.append(h('span', { class: 'queue-empty' }, 'nothing queued'));
  S.queue.pending.forEach((j, idx) => {
    const bg = artSources({ app_id: j.app_id, art: artFor(j.app_id) }).map((s) => `url("${s}")`).join(', ');
    list.append(h('div', { class: 'q-wrap', title: `${j.title} (${j.spec.kind})` },
      h('span', { class: 'q-num' }, idx + 1),
      h('div', { class: 'q-item', style: `background-image:${bg}` }),
      h('button', { class: 'q-x', title: 'Remove from queue', onclick: () => call('cancel_job', { id: j.id }) }, '×')));
  });
}

function artFor(appId) {
  return S.games.find((g) => g.app_id === appId)?.art_url
    || S.restore.binder?.packages.find((p) => p.app_id === appId)?.art_url
    || null;
}

function renderAll() {
  document.querySelectorAll('.tab').forEach((t) => t.classList.toggle('active', t.dataset.mode === S.mode));
  renderFilters();
  renderLocation();
  renderPages();
  renderDeck();
}

/** Cheap progress update: touch only the busy sleeve and the LCD. */
function renderProgress() {
  renderDeck();
  const cur = S.queue.current;
  if (!cur) return;
  const pct = progressPct(S.progress);
  const key = ({ restore: 'r-', move: 'm-' }[cur.spec.kind] || 'b-') + cur.app_id;
  const sleeve = document.querySelector(`.sleeve[data-key="${CSS.escape(key)}"]`);
  if (sleeve) {
    sleeve.querySelector('.ring')?.style.setProperty('--p', pct);
    const a = sleeve.querySelector('.strip-action');
    if (a) a.textContent = `${pct.toFixed(0)}%`;
  }
}

// ------------------------------------------------------------------------------------------ case (details modal)

function openCase(item) {
  const isBackup = S.mode === 'backup';
  const facts = [];
  const add = (k, v, cls) => facts.push(h('dt', {}, k), h('dd', { class: cls || null }, v));
  let explain = '';
  const actions = [];
  const primary = primaryLabel(item);

  if (isBackup) {
    const g = item.game, p = item.pkg;
    add('Size on PC', fmtBytes(g.size_on_disk));
    add('Steam build on PC', g.build_id || 'unknown');
    add('Build on drive', p?.build_id || '—', item.state === 'update' ? 'warn-text' : item.state === 'done' ? 'good' : null);
    if (p?.completed_at) add('Last backup', p.completed_at);
    add('Game folder', g.game_path);
    if (p) add('Backup folder', p.path);
    if (!g.fully_installed) add('Steam says', 'An update is pending for this game', 'warn-text');
    explain = {
      new: 'Copies the game files, its Steam manifest and any save folders found onto the drive. The original stays where it is.',
      resume: 'This backup was interrupted. Resuming keeps every file that already finished and copies the rest.',
      update: 'Steam updated this game since the last backup. Updating copies only the files that changed and removes files the update deleted.',
      done: 'This backup matches the game on this PC. Use "Check every byte" if you want to be completely sure before wiping the PC.',
      busy: 'This game is being copied right now.',
      queued: 'Waiting for the current transfer to finish.',
    }[item.state];
    if (primary) actions.push(h('button', { class: 'btn primary', onclick: () => { closeModal(); primaryAction(item); } }, primary));
    if (p && p.status === 'complete' && item.state !== 'busy') actions.push(h('button', { class: 'btn', onclick: () => { closeModal(); enqueueVerify(item); } }, 'Check every byte'));
    if (item.state === 'busy') actions.push(h('button', { class: 'btn danger', onclick: () => { closeModal(); call('cancel_job', { id: item.job.id }); } }, 'Stop'));
    if (item.state === 'queued') actions.push(h('button', { class: 'btn', onclick: () => { closeModal(); call('cancel_job', { id: item.job.id }); } }, 'Remove from queue'));
    if (p) actions.push(h('button', { class: 'btn', onclick: () => call('open_folder', { path: p.path }).catch(showError) }, 'Open backup folder'));
    actions.push(h('button', { class: 'btn', onclick: () => call('open_folder', { path: g.game_path }).catch(showError) }, 'Open game folder'));
  } else if (S.mode === 'move') {
    const g = item.game;
    const target = S.move.libraries.find((l) => samePath(l.path, S.move.to));
    add('Size', fmtBytes(g.size_on_disk));
    add('Now in', g.library_root);
    add('Move to', S.move.to || 'choose a library');
    if (target) add('Free there', fmtBytes(target.free_bytes), target.free_bytes < g.size_on_disk ? 'warn-text' : 'good');
    if (!g.fully_installed) add('Steam says', 'An update is pending: let Steam finish it first', 'warn-text');
    explain = {
      new: 'Copies the game, its Workshop items and its shader cache to the other library and checks every file. Then it hands the game over to Steam there and removes the old copy. Steam must be closed. On the same drive it is an instant rename.',
      resume: 'A move was interrupted. The game still lives in its original library; resuming keeps the files already copied.',
      busy: 'Moving right now.',
      queued: 'Waiting for the current transfer to finish.',
    }[item.state];
    if (primary) actions.push(h('button', { class: 'btn move', onclick: () => { closeModal(); primaryAction(item); } }, primary));
    if (item.state === 'busy') actions.push(h('button', { class: 'btn danger', onclick: () => { closeModal(); call('cancel_job', { id: item.job.id }); } }, 'Stop'));
    actions.push(h('button', { class: 'btn', onclick: () => call('open_folder', { path: g.game_path }).catch(showError) }, 'Open game folder'));
  } else {
    const p = item.pkg, inst = item.inst;
    add('Backup size', fmtBytes(p.bytes));
    add('Build on drive', p.build_id || 'unknown');
    add('Installed build', inst ? (inst.partial ? 'unfinished restore' : inst.build_id || 'unknown') : 'not installed', item.state === 'done' ? 'good' : item.state === 'update' ? 'warn-text' : null);
    if (p.completed_at) add('Backed up', p.completed_at);
    add('Restore into', S.restore.library || 'choose a Steam library');
    add('Backup folder', p.path);
    explain = {
      new: 'Copies the game into the Steam library and installs its manifest, so Steam sees it as installed. Steam must be closed while this runs.',
      resume: 'A previous restore was interrupted. Resuming keeps the finished files and copies the rest.',
      update: 'The drive has a newer build than this PC. Updating copies only the changed files, so you skip the Steam download.',
      done: 'This PC already has the same build as the drive.',
      newer: 'This PC already has a newer build than the drive. Restoring would make it older, so it is not offered.',
      broken: 'This backup never finished. Resume it on the PC it came from, then restore it here.',
      busy: 'Restoring right now.',
      queued: 'Waiting for the current transfer to finish.',
    }[item.state];
    if (primary) actions.push(h('button', { class: 'btn blue', onclick: () => { closeModal(); primaryAction(item); } }, primary));
    if (item.state === 'done') actions.push(h('button', { class: 'btn', onclick: () => { closeModal(); primaryAction(item, true); } }, 'Restore again (repair)'));
    if (item.state === 'busy') actions.push(h('button', { class: 'btn danger', onclick: () => { closeModal(); call('cancel_job', { id: item.job.id }); } }, 'Stop'));
    actions.push(h('button', { class: 'btn', onclick: () => call('open_folder', { path: p.path }).catch(showError) }, 'Open backup folder'));
  }

  const front = h('div', { class: 'case-front' });
  const sources = [...artSources(item), cdn(item.app_id, 'header.jpg')];
  const img = h('img', { alt: '' });
  let n = 0;
  img.onerror = () => { n++; if (n < sources.length) img.src = sources[n]; else img.remove(); };
  img.src = sources[0];
  front.append(img);

  showModal(h('div', { class: 'case' },
    front,
    h('div', { class: 'liner' },
      h('h2', {}, item.name),
      h('div', { class: 'sub' }, `Steam app ${item.app_id}`),
      h('dl', { class: 'facts' }, facts),
      explain ? h('p', { class: 'explain' }, explain) : null,
      h('div', { class: 'actions' }, actions),
    )), true);
}

function primaryLabel(item) {
  if (S.mode === 'backup') {
    if (!S.binder && ['new', 'resume', 'update'].includes(item.state)) return 'Choose drive & back up';
    return { new: 'Back up', resume: 'Resume backup', update: 'Update backup' }[item.state] || null;
  }
  if (S.mode === 'move') return { new: S.move.to ? `Move to ${S.move.to}` : 'Move', resume: 'Resume move' }[item.state] || null;
  return { new: 'Restore to PC', resume: 'Resume restore', update: 'Update on PC' }[item.state] || null;
}

// ------------------------------------------------------------------------------------------ actions

async function primaryAction(item, force) {
  if (S.mode === 'move') return moveAction(item);
  if (S.mode === 'backup') {
    if (!S.binder) {
      const ok = await chooseDestination();
      if (!ok) return;
    }
    return enqueue({ kind: 'backup', app_id: item.app_id, binder: S.binder.root }, item.name, item.app_id);
  }
  if (!force && !['new', 'resume', 'update'].includes(item.state)) return openCase(item);
  if (!S.restore.library) {
    toast('Pick a Steam library', 'Choose which Steam library the game should go into.', '');
    return chooseRestoreTarget();
  }
  S.steamRunning = await call('steam_running').catch(() => false);
  if (S.steamRunning) {
    renderLocation();
    return dialog('Close Steam first', 'Steam is running. Exit it completely, including the icon in the system tray (right-click it and choose Exit Steam), then try again.\n\nThis stops Steam from overwriting the game\'s install record while it is being restored.', [['OK', 'primary']]);
  }
  return enqueue({ kind: 'restore', package: item.pkg.path, library: S.restore.library }, item.name, item.app_id);
}

async function moveAction(item) {
  const m = S.move;
  if (!['new', 'resume'].includes(item.state)) return openCase(item);
  if (!m.to) return toast('Pick a library', 'Choose which Steam library to move the game into.', '');
  S.steamRunning = await call('steam_running').catch(() => false);
  if (S.steamRunning) {
    renderLocation();
    return dialog('Close Steam first', 'Steam is running. Exit it completely, including the icon in the system tray (right-click it and choose Exit Steam), then try again.\n\nSteam has to be closed so it finds the game in its new library when it starts.', [['OK', 'primary']]);
  }
  if (!item.game.fully_installed) {
    return dialog('Let Steam finish first', `Steam has an update or download waiting for ${item.name}. Start Steam, let it finish, exit Steam, then move the game.`, [['OK', 'primary']]);
  }
  const target = m.libraries.find((l) => samePath(l.path, m.to));
  const sameDrive = item.game.library_root.slice(0, 2).toLowerCase() === m.to.slice(0, 2).toLowerCase();
  const choice = await dialog(`Move ${item.name}?`,
    `From:  ${item.game.library_root}\nTo:  ${m.to}${target ? `  (${fmtBytes(target.free_bytes)} free)` : ''}\n\n` +
    (sameDrive
      ? 'Both libraries are on the same drive, so this is instant.'
      : `About ${fmtBytes(item.size)} will be copied and checked. The old copy is deleted only after everything is verified.`) +
    '\n\nKeep Steam closed until it finishes.',
    [['Cancel', ''], ['Move', 'primary', 'move']]);
  if (choice !== 'move') return;
  return enqueue({ kind: 'move', app_id: item.app_id, library: m.to }, item.name, item.app_id);
}

function enqueueVerify(item) {
  return enqueue({ kind: 'verify', app_id: item.app_id, binder: S.binder.root }, item.name, item.app_id);
}

async function enqueue(spec, title, appId) {
  try {
    await call('enqueue', { spec, title, appId });
    if (S.queue.current) toast('Added to the queue', `${title} will start when the current transfer finishes.`, '');
  } catch (e) {
    showError(e);
  }
}

async function chooseDestination() {
  const picked = await call('pick_folder', { title: 'Choose the drive or folder to back up to', start: S.settings.destination || null }).catch(showError);
  if (!picked) return false;
  S.settings.destination = picked;
  await saveSettings();
  await loadBinder();
  renderAll();
  return true;
}

async function chooseRestoreSource() {
  const picked = await call('pick_folder', { title: 'Choose the drive that has your backups', start: S.settings.restore_source || null }).catch(showError);
  if (picked) await loadRestoreSource(picked);
  else renderLocation();
}

async function loadRestoreSource(path) {
  try {
    S.restore.binder = await call('scan_binder', { path });
    S.settings.restore_source = path;
    saveSettings();
  } catch (e) { showError(e); }
  S.spread = 0;
  renderAll();
}

async function chooseRestoreTarget() {
  const picked = await call('pick_folder', { title: 'Choose a Steam library (or the drive it is on)', start: S.restore.library || null }).catch(showError);
  if (!picked) { renderLocation(); return; }
  const lib = await call('detect_library', { path: picked });
  if (!lib) {
    const choice = await dialog('Create a Steam library there?', `There's no Steam library at ${picked} yet.\n\nSteam Transfer can create one and add it to Steam for you, so restored games show up as installed. Steam needs to be closed for this.`, [['Cancel', ''], ['Create library', 'primary', 'create']]);
    if (choice === 'create') return createLibraryAt(picked, 'restore');
    renderLocation();
    return;
  }
  setRestoreTarget(lib);
}

function libraryLabel(path, libraries) {
  const info = libraries.find((l) => samePath(l.path, path));
  if (!info) return path;
  const free = info.free_bytes != null ? `  ·  ${fmtBytes(info.free_bytes)} free` : '';
  return `${path}${free}${info.registered === false ? '  ·  not in Steam yet (added on first use)' : ''}`;
}

/** Pick a drive or folder and create a Steam library there. */
async function newLibrary(forTab) {
  const picked = await call('pick_folder', { title: 'Pick the drive (or folder) for the new Steam library', start: null }).catch(showError);
  if (!picked) { renderLocation(); return; }
  return createLibraryAt(picked, forTab);
}

async function createLibraryAt(picked, forTab) {
  try {
    const created = await call('create_library', { path: picked });
    toast('Steam library ready', `${created.path}\n${created.note}`, 'ok');
    if (forTab === 'move') {
      S.move.to = created.path;
      S.settings.move_to = created.path;
      saveSettings();
      await refreshMove();
    } else {
      S.restore.libraries = await call('list_libraries').catch(() => S.restore.libraries);
      await setRestoreTarget(created.path);
    }
  } catch (e) {
    showError(e);
    renderLocation();
  }
}

async function setRestoreTarget(lib) {
  S.restore.library = lib;
  S.settings.restore_target = lib;
  saveSettings();
  await loadInstalled();
  renderAll();
}

async function saveSettings() {
  try { await call('save_settings', { settings: S.settings }); } catch (e) { console.warn(e); }
}

// ------------------------------------------------------------------------------------------ data loading

async function loadGames() {
  try { S.games = await call('scan_library'); } catch (e) { showError(e); S.games = []; }
}

async function loadBinder() {
  if (!S.settings.destination) { S.binder = null; S.pkgByApp = {}; return; }
  try {
    S.binder = await call('scan_binder', { path: S.settings.destination });
    S.pkgByApp = Object.fromEntries(S.binder.packages.map((p) => [p.app_id, p]));
  } catch (e) { showError(e); }
}

async function loadInstalled() {
  if (!S.restore.library) { S.restore.installed = {}; return; }
  try { S.restore.installed = await call('library_state', { library: S.restore.library }); } catch (e) { showError(e); }
}

async function refreshAll() {
  await Promise.all([loadGames(), loadBinder()]);
  renderAll();
}

async function refreshRestore() {
  const r = S.restore;
  const [libs, binders] = await Promise.all([call('list_libraries').catch(() => []), call('find_binders').catch(() => [])]);
  r.libraries = libs;
  r.binders = binders;
  if (!r.library) r.library = S.settings.restore_target || libs[0]?.path || null;
  r.library = libs.find((l) => samePath(l.path, r.library))?.path || r.library;
  const source = r.binder?.root || S.settings.restore_source || binders[0]?.path || S.settings.destination;
  if (source) {
    try { r.binder = await call('scan_binder', { path: source }); } catch (e) { showError(e); }
  }
  await loadInstalled();
  S.steamRunning = await call('steam_running').catch(() => false);
  r.loaded = true;
  renderAll();
}

// ---- torrents

async function startDownload() {
  const link = (S.dl.link || '').trim();
  if (!link) return toast('Paste a magnet link first', 'Copy a magnet link (it starts with magnet:?) and paste it into the box.', '');
  if (!S.dl.dir) return chooseDownloadDir();
  try {
    await call('torrent_add', { link, folder: S.dl.dir });
    S.dl.link = '';
    S.filter = 'all';
    await pollTorrents(true);
  } catch (e) { showError(e); }
}

async function openTorrentFile() {
  const path = await call('pick_torrent_file').catch(showError);
  if (!path) return;
  if (!S.dl.dir) await chooseDownloadDir();
  if (!S.dl.dir) return;
  try {
    await call('torrent_add_file', { path, folder: S.dl.dir });
    await pollTorrents(true);
  } catch (e) { showError(e); }
}

async function chooseDownloadDir() {
  const picked = await call('pick_folder', { title: 'Where should downloads be saved?', start: S.dl.dir }).catch(showError);
  if (!picked) return;
  S.dl.dir = picked;
  S.settings.download_dir = picked;
  saveSettings();
  renderLocation();
}

async function torrentPrimary(t) {
  try {
    if (['downloading', 'checking', 'fetching'].includes(t.state)) await call('torrent_pause', { key: t.key });
    else if (t.state === 'paused') await call('torrent_resume', { key: t.key });
    else if (t.state === 'seeding') await call('open_folder', { path: t.folder });
    else if (t.state === 'error') return removeTorrent(t);
    await pollTorrents(true);
  } catch (e) { showError(e); }
}

async function removeTorrent(t) {
  const hasFiles = t.id != null && t.progress_bytes > 0;
  const choice = hasFiles
    ? await dialog(`Remove ${t.name}?`, 'Keep the downloaded files, or delete them from your disk too?', [['Cancel', ''], ['Delete files too', 'danger', 'delete'], ['Remove, keep files', 'primary', 'keep']])
    : 'keep';
  if (choice !== 'keep' && choice !== 'delete') return;
  try {
    await call('torrent_remove', { key: t.key, deleteFiles: choice === 'delete' });
    await pollTorrents(true);
  } catch (e) { showError(e); }
}

/** The rows of a download's details: [id, label, value, cssClass]. Which rows appear depends only on
 *  the download's stage, so the open details can refresh their values in place every second. */
function torrentFacts(t) {
  const rows = [];
  const add = (id, label, value, cls) => rows.push([id, label, value, cls || null]);
  add('status', 'Status', { fetching: 'Fetching torrent info from peers (you can pause this)', checking: 'Checking existing files', downloading: 'Downloading', paused: 'Paused', seeding: 'Finished (sharing with others while the app is open)', error: 'Error' }[t.state], t.state === 'seeding' ? 'good' : t.state === 'error' ? 'warn-text' : null);
  if (t.error) add('error', 'Problem', t.error, 'warn-text');
  if (t.total_bytes) add('size', 'Size', fmtBytes(t.total_bytes));
  if (t.total_bytes) add('done', 'Downloaded', `${fmtBytes(t.progress_bytes)} (${torrentPct(t).toFixed(1)}%)`);
  if (t.state === 'downloading' || t.state === 'seeding') add('speed', 'Speed', `↓ ${fmtBytes(t.down_bps)}/s   ↑ ${fmtBytes(t.up_bps)}/s`);
  if (t.state === 'downloading') add('eta', 'Time left', t.eta_secs ? fmtEta(t.eta_secs) : 'working it out…');
  if (t.id != null) add('peers', 'Peers', t.peers ? `${t.peers} connected (${t.seeders} seeders)` : 'looking for peers…');
  if (t.id != null) add('up', 'Uploaded', fmtBytes(t.uploaded_bytes));
  add('folder', 'Saved to', t.folder);
  return rows;
}

/** The download whose details are open, so polling can keep them current. */
let openTorrent = null;

function refreshOpenTorrent() {
  if (!openTorrent || !modalOpen()) return;
  const t = S.dl.list.find((x) => x.key === openTorrent.key);
  if (!t) return;
  const rows = torrentFacts(t);
  const layout = `${t.state}|${t.cover || ''}|${rows.map((r) => r[0]).join(',')}`;
  if (layout !== openTorrent.layout) return openTorrentCase(t);   // stage changed: rebuild
  for (const [id, , value, cls] of rows) {
    const dd = openTorrent.cells.get(id);
    if (!dd) continue;
    if (dd.textContent !== value) dd.textContent = value;
    dd.className = cls || '';
  }
}

function openTorrentCase(t) {
  const rows = torrentFacts(t);
  const cells = new Map();
  const facts = rows.flatMap(([id, label, value, cls]) => {
    const dd = h('dd', { class: cls }, value);
    cells.set(id, dd);
    return [h('dt', {}, label), dd];
  });
  const actions = [];
  if (['downloading', 'checking', 'fetching'].includes(t.state)) actions.push(h('button', { class: 'btn', onclick: () => { closeModal(); torrentPrimary(t); } }, 'Pause'));
  if (t.state === 'paused') actions.push(h('button', { class: 'btn violet', onclick: () => { closeModal(); torrentPrimary(t); } }, 'Resume'));
  if (t.state === 'seeding') actions.push(h('button', { class: 'btn', onclick: () => { closeModal(); call('torrent_pause', { key: t.key }).then(() => pollTorrents(true)).catch(showError); } }, 'Stop sharing'));
  actions.push(h('button', { class: 'btn', onclick: () => call('open_folder', { path: t.folder }).catch(showError) }, 'Open folder'));
  actions.push(h('button', { class: 'btn', onclick: () => changeCover(t) }, 'Change cover…'));
  if (t.cover) actions.push(h('button', { class: 'btn', onclick: () => resetCover(t) }, 'Reset cover'));
  actions.push(h('button', { class: 'btn danger', onclick: () => { closeModal(); removeTorrent(t); } }, 'Remove'));
  const cdr = () => h('div', { class: 'case-front cdr-case' }, h('div', { class: 'cdr-big' }, h('div', { class: 'initials dark' }, initials(t.name) || '♪')));
  let front = cdr();
  if (t.cover) {
    front = h('div', { class: 'case-front' });
    const img = h('img', { alt: '', src: t.cover });
    img.onerror = () => front.replaceWith(cdr());
    front.append(img);
  }
  const rebuilding = openTorrent && openTorrent.key === t.key && modalOpen();
  showModal(h('div', { class: `case ${rebuilding ? 'no-anim' : ''}` }, front,
    h('div', { class: 'liner' },
      h('h2', {}, t.name),
      h('div', { class: 'sub' }, 'Torrent download'),
      h('dl', { class: 'facts' }, facts),
      h('div', { class: 'actions' }, actions))), true, () => { openTorrent = null; });
  openTorrent = { key: t.key, layout: `${t.state}|${t.cover || ''}|${rows.map((r) => r[0]).join(',')}`, cells };
}

const limitText = (kbps) => (kbps ? h('span', { class: 'limit' }, ` / max ${fmtBytes(kbps * 1024)}/s`) : null);

async function openSpeedSettings() {
  const field = (label, kbps) => {
    const unlimited = h('input', { type: 'checkbox', checked: !kbps });
    const amount = h('input', { type: 'number', min: '0.1', step: '0.1', value: kbps ? +(kbps / 1024).toFixed(1) : '', placeholder: 'MB/s', disabled: !kbps });
    unlimited.addEventListener('change', () => { amount.disabled = unlimited.checked; if (!unlimited.checked) amount.focus(); });
    const row = h('div', { class: 'speed-row' },
      h('span', { class: 'speed-label' }, label),
      h('label', { class: 'speed-unlimited' }, unlimited, ' No limit'),
      h('span', { class: 'speed-amount' }, amount, ' MB/s'));
    return { row, read: () => (unlimited.checked ? 0 : Math.max(1, Math.round((parseFloat(amount.value) || 0) * 1024))) };
  };
  const down = field('Download', S.settings.torrent_down_kbps || 0);
  const up = field('Upload', S.settings.torrent_up_kbps || 0);
  const extra = h('div', { class: 'speed-settings' }, down.row, up.row,
    h('p', { class: 'speed-hint' }, 'Limits apply to all downloads together, straight away. Keeping some upload helps downloads stay fast.'));
  const choice = await dialog('Download speed', '', [['Cancel', ''], ['Save', 'primary', 'save']], extra);
  if (choice !== 'save') return;
  const downKbps = down.read(), upKbps = up.read();
  try {
    await call('torrent_set_limits', { downKbps, upKbps });
    S.settings.torrent_down_kbps = downKbps;
    S.settings.torrent_up_kbps = upKbps;
    renderLocation();
    toast('Speed limits saved', `Download: ${downKbps ? fmtBytes(downKbps * 1024) + '/s' : 'no limit'} · Upload: ${upKbps ? fmtBytes(upKbps * 1024) + '/s' : 'no limit'}`, 'ok');
  } catch (e) { showError(e); }
}

async function changeCover(t) {
  const path = await call('pick_cover_image').catch(showError);
  if (!path) return;
  try {
    await call('torrent_set_cover', { name: t.name, path });
    closeModal();
    await pollTorrents(true);
    const updated = S.dl.list.find((x) => x.key === t.key);
    if (updated) openTorrentCase(updated);
  } catch (e) { showError(e); }
}

async function resetCover(t) {
  await call('torrent_reset_cover', { name: t.name }).catch(showError);
  closeModal();
  toast('Cover reset', 'Looking for a new cover…', '');
  await pollTorrents(true);
}

let torrentTimer = null;
async function pollTorrents(force) {
  let list;
  try { list = await call('torrent_list'); } catch { return; }
  const signature = list.map((t) => `${t.key}:${t.state}:${t.name}:${t.cover || ''}`).join('|');
  const changed = signature !== S.dl.signature;
  S.dl.list = list;
  S.dl.signature = signature;
  if (S.mode === 'download') {
    if (changed || force) renderAll();
    else updateTorrentSleeves();
  }
  refreshOpenTorrent();
  if (!S.queue.current && !S.updating) renderDeck();
}

/** Update rings and labels in place every second instead of rebuilding the pages. */
function updateTorrentSleeves() {
  for (const t of S.dl.list) {
    const el = document.querySelector(`.sleeve[data-key="${CSS.escape(`d-${t.key}`)}"]`);
    if (!el) continue;
    el.querySelector('.ring')?.style.setProperty('--p', torrentPct(t));
    const meta = el.querySelector('.meta-text');
    if (meta) meta.textContent = torrentMeta(t);
  }
  renderLocation();
}

function startTorrentPolling() {
  clearInterval(torrentTimer);
  torrentTimer = setInterval(() => {
    // poll every second while the tab is open or something is moving; otherwise every 5 s
    const busy = S.dl.list.some((t) => TORRENT_ACTIVE.includes(t.state));
    if (S.mode === 'download' || busy || Date.now() % 5000 < 1000) pollTorrents(false);
  }, 1000);
}

async function refreshMove() {
  const m = S.move;
  const libs = await call('list_libraries').catch(() => []);
  m.libraries = libs;
  const fix = (p) => libs.find((l) => samePath(l.path, p))?.path || null;
  const gamesIn = (lib) => S.games.filter((g) => samePath(g.library_root, lib)).length;
  m.from = fix(m.from) || fix(S.settings.move_from) || libs.slice().sort((a, b) => gamesIn(b.path) - gamesIn(a.path))[0]?.path || null;
  m.to = fix(m.to) || fix(S.settings.move_to);
  if (!m.to || samePath(m.to, m.from)) m.to = libs.find((l) => !samePath(l.path, m.from))?.path || null;
  await loadMoveState();
  S.steamRunning = await call('steam_running').catch(() => false);
  renderAll();
}

async function loadMoveState() {
  S.move.installed = S.move.to ? await call('library_state', { library: S.move.to }).catch(() => ({})) : {};
}

async function setMoveFrom(path) {
  S.move.from = path;
  S.settings.move_from = path;
  if (samePath(S.move.to, path)) S.move.to = S.move.libraries.find((l) => !samePath(l.path, path))?.path || null;
  saveSettings();
  S.spread = 0;
  await loadMoveState();
  renderAll();
}

async function setMoveTo(path) {
  S.move.to = path;
  S.settings.move_to = path;
  saveSettings();
  await loadMoveState();
  renderAll();
}

async function setMode(mode) {
  if (S.mode === mode) return;
  S.mode = mode;
  S.filter = 'all';
  S.spread = 0;
  renderAll();
  if (mode === 'restore') await refreshRestore();
  if (mode === 'move') await refreshMove();
  if (mode === 'download') {
    if (!S.dl.dir) S.dl.dir = S.settings.download_dir || await call('torrent_default_dir').catch(() => null);
    await pollTorrents(true);
  }
}

// ------------------------------------------------------------------------------------------ modal, dialog, toast

let modalOnClose = null;
function showModal(content, closable = true, onClose = null) {
  const root = $('#modalRoot');
  root.replaceChildren(content);
  if (closable) root.append(h('button', { class: 'case-close', 'aria-label': 'Close', onclick: closeModal }, '✕'));
  root.classList.remove('hidden');
  root.onclick = (e) => { if (closable && e.target === root) closeModal(); };
  modalOnClose = onClose;
}
function closeModal() {
  $('#modalRoot').classList.add('hidden');
  $('#modalRoot').replaceChildren();
  const cb = modalOnClose; modalOnClose = null;
  if (cb) cb();
}
const modalOpen = () => !$('#modalRoot').classList.contains('hidden');

/** buttons: [[label, style, value]] - resolves with the clicked button's value (or label). */
function dialog(title, text, buttons, extra) {
  return new Promise((resolve) => {
    const btns = buttons.map(([label, style, value]) => h('button', { class: `btn ${style || ''}`, onclick: () => { resolve(value ?? label); closeModal(); } }, label));
    // resolve before closing: closing a dialog any other way (✕, Esc, backdrop) resolves it with null
    showModal(h('div', { class: 'dialog' }, h('h2', {}, title), h('p', {}, text), extra || null, h('div', { class: 'actions' }, btns)), true, () => resolve(null));
  });
}

function toast(title, text, kind = '') {
  const el = h('div', { class: `toast ${kind}`, onclick: () => dismiss() }, h('b', {}, title), h('span', {}, text));
  const dismiss = () => { el.classList.add('out'); setTimeout(() => el.remove(), 260); };
  $('#toasts').append(el);
  setTimeout(dismiss, kind === 'err' ? 10000 : 6000);
}

function showError(e) {
  const msg = typeof e === 'string' ? e : e?.message || JSON.stringify(e);
  toast('Something went wrong', msg, 'err');
  return null;
}

// ------------------------------------------------------------------------------------------ updates

async function checkForUpdate(manual) {
  try {
    const u = await call('check_update');
    S.update = u;
    const pill = $('#updatePill');
    if (u) {
      pill.textContent = `Update ${u.version} available`;
      pill.classList.remove('hidden');
      if (manual) showUpdateDialog();
    } else {
      pill.classList.add('hidden');
      if (manual) toast('You\'re up to date', `Steam Transfer ${S.version} is the latest version.`, 'ok');
    }
  } catch (e) {
    if (manual) toast('Couldn\'t check for updates', String(e), 'err');
    else console.warn('update check failed', e);
  }
}

async function showUpdateDialog() {
  const u = S.update;
  if (!u || S.updating) return;
  if (S.queue.current || S.queue.pending.length) {
    return dialog('Finish your transfers first', 'A backup or restore is still running. Update once the queue is empty so nothing is interrupted.', [['OK', 'primary']]);
  }
  const choice = await dialog(`Version ${u.version} is ready`,
    `${u.notes || 'A new version of Steam Transfer is available.'}\n\nThe app will download it, install it and restart. Your backups and settings are not touched.`,
    [['Later', ''], ['Install & restart', 'primary', 'install']]);
  if (choice !== 'install') return;

  // Progress shows on the CD-player display at the bottom.
  S.updating = { version: u.version, done: 0, total: 0, phase: 'download' };
  const pill = $('#updatePill');
  pill.textContent = 'Updating…';
  pill.disabled = true;
  renderDeck();
  const offs = [
    await S.api.listen('update-progress', (e) => {
      const [done, total] = e.payload;
      Object.assign(S.updating, { done, total: total || 0 });
      renderDeck();
    }),
    await S.api.listen('update-installing', () => {
      S.updating.phase = 'install';
      renderDeck();
    }),
  ];
  try {
    await call('install_update');   // on success the installer takes over and the app restarts
  } catch (e) {
    offs.forEach((off) => off());
    S.updating = null;
    pill.textContent = `Update ${u.version} available`;
    pill.disabled = false;
    renderDeck();
    showError(e);
  }
}

function renderUpdateDeck() {
  const u = S.updating;
  const tray = $('#trayDisc');
  tray.style.backgroundImage = '';
  tray.classList.add('loaded', 'spinning', 'update-disc');
  $('#stopBtn').disabled = true;
  const pct = u.phase === 'install' ? 100 : u.total ? (u.done / u.total) * 100 : 0;
  $('#lcdTitle').textContent = `UPDATING TO V${u.version}`;
  $('#lcdPct').textContent = u.total || u.phase === 'install' ? `${pct.toFixed(0)}%` : '';
  $('#lcdFill').style.width = `${pct}%`;
  if (u.phase === 'install') {
    $('#lcdStats').textContent = 'INSTALLING  STEAM TRANSFER WILL RESTART BY ITSELF';
  } else {
    $('#lcdStats').textContent = u.total
      ? `DOWNLOADING FROM GITHUB  ${fmtBytes(u.done)} / ${fmtBytes(u.total)}`
      : 'CONNECTING TO GITHUB…';
  }
  $('#lcdEta').textContent = '';
  $('#lcdFile').textContent = u.phase === 'install' ? 'Your backups and settings are not touched.' : '';
}

// ------------------------------------------------------------------------------------------ events & boot

function bindUi() {
  document.querySelectorAll('.tab').forEach((t) => t.addEventListener('click', () => setMode(t.dataset.mode)));
  $('#search').addEventListener('input', (e) => { S.search = e.target.value; S.spread = 0; renderPages(); renderFilters(); });
  $('#prevPage').addEventListener('click', () => flip(-1));
  $('#nextPage').addEventListener('click', () => flip(1));
  $('#stopBtn').addEventListener('click', () => { if (S.queue.current) call('cancel_job', { id: S.queue.current.id }); });
  $('#version').addEventListener('click', () => checkForUpdate(true));
  $('#updatePill').addEventListener('click', showUpdateDialog);

  let wheelLock = 0;
  $('#binder').addEventListener('wheel', (e) => {
    if (modalOpen() || Math.abs(e.deltaY) < 10) return;
    const now = Date.now();
    if (now - wheelLock < 380) return;
    wheelLock = now;
    flip(e.deltaY > 0 ? 1 : -1);
  }, { passive: true });

  document.addEventListener('keydown', (e) => {
    if (e.key === 'Escape' && modalOpen()) return closeModal();
    if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === 'f') { e.preventDefault(); $('#search').focus(); return; }
    if (modalOpen() || e.target.tagName === 'INPUT' || e.target.tagName === 'SELECT') return;
    if (e.key === 'ArrowRight' || e.key === 'PageDown') flip(1);
    if (e.key === 'ArrowLeft' || e.key === 'PageUp') flip(-1);
  });

  let resizeTimer;
  new ResizeObserver(() => {
    clearTimeout(resizeTimer);
    resizeTimer = setTimeout(() => { if (computeLayout()) renderPages(); }, 60);
  }).observe($('#binder'));

  const win = S.api.win;
  if (win) {
    $('#winMin').addEventListener('click', () => win.minimize());
    $('#winMax').addEventListener('click', () => win.toggleMaximize());
    $('#winClose').addEventListener('click', () => win.close());
    win.onCloseRequested?.(async (event) => {
      const downloading = S.dl.list.some((t) => TORRENT_ACTIVE.includes(t.state));
      if (!S.queue.current && !S.queue.pending.length && !downloading) return;
      event.preventDefault();
      if (!S.queue.current && !S.queue.pending.length) {
        const c = await dialog('Downloads are still running', 'Torrent downloads pause while Steam Transfer is closed and carry on the next time you open it.', [['Keep open', ''], ['Close anyway', 'danger', 'quit']]);
        if (c === 'quit') win.destroy();
        return;
      }
      const choice = await dialog('Stop the transfer and quit?', 'A transfer is still running. Everything copied so far is kept and you can resume it next time.', [['Keep running', ''], ['Stop & quit', 'danger', 'quit']]);
      if (choice !== 'quit') return;
      for (const j of S.queue.pending) await call('cancel_job', { id: j.id });
      if (S.queue.current) await call('cancel_job', { id: S.queue.current.id });
      setTimeout(() => win.destroy(), 400);
    });
  }
}

async function bindEvents() {
  await S.api.listen('queue', (e) => {
    const hadCurrent = S.queue.current?.id;
    S.queue = e.payload;
    if (S.queue.current?.id !== hadCurrent) S.progress = null;
    renderAll();
  });
  await S.api.listen('progress', (e) => {
    S.progress = e.payload;
    renderProgress();
  });
  await S.api.listen('job-finished', async (e) => {
    const r = e.payload;
    S.lastResult = { ...r, at: Date.now() };
    const kindWord = { backup: 'Backup', restore: 'Restore', verify: 'Check', move: 'Move' }[r.kind];
    if (r.outcome === 'success') toast(`${r.title} ✓`, r.message, 'ok');
    else if (r.outcome === 'cancelled') toast(`${kindWord} stopped`, `${r.title}: ${r.message}`, '');
    else toast(`${kindWord} failed: ${r.title}`, r.message, 'err');
    if (r.kind === 'restore') await loadInstalled();
    else if (r.kind === 'move') {
      await loadGames();
      S.move.libraries = await call('list_libraries').catch(() => S.move.libraries);
      await loadMoveState();
    } else await loadBinder();
    renderAll();
  });
}

async function main() {
  if (!window.__TAURI__) await loadScript('mock.js');
  S.api = window.__TAURI__ ? tauriApi() : window.MockApi;
  bindUi();
  await bindEvents();
  computeLayout();
  renderAll();

  try { S.version = await call('app_version'); $('#version').textContent = `v${S.version}`; } catch { /* ignore */ }
  S.settings = await call('get_settings').catch(() => ({}));
  S.queue = await call('queue_state').catch(() => S.queue);
  await loadGames();
  if (!S.settings.destination) {
    const found = await call('find_binders').catch(() => []);
    if (found.length) {
      S.settings.destination = found[0].path;
      saveSettings();
      toast('Found your backup drive', `Using ${found[0].path}. Click Change to pick another.`, 'ok');
    }
  }
  await loadBinder();
  S.loading = false;
  computeLayout();
  renderAll();

  setInterval(async () => {
    if (S.mode === 'backup') return;
    const running = await call('steam_running').catch(() => false);
    if (running !== S.steamRunning) { S.steamRunning = running; renderLocation(); }
  }, 4000);
  setInterval(() => { if (!S.queue.current && S.lastResult) renderDeck(); }, 15000);
  startTorrentPolling();

  if (S.settings.auto_update_check !== false) setTimeout(() => checkForUpdate(false), 2500);
}

main();

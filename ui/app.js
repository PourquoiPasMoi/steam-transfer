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

const FILTERS = {
  backup: [
    ['all', 'All', () => true],
    ['new', 'Not backed up', (i) => i.state === 'new'],
    ['done', 'Backed up', (i) => i.state === 'done'],
    ['attention', 'Needs update', (i) => i.state === 'update' || i.state === 'resume'],
  ],
  restore: [
    ['all', 'All', () => true],
    ['new', 'Not installed', (i) => i.state === 'new'],
    ['done', 'Installed', (i) => i.state === 'done' || i.state === 'newer'],
    ['attention', 'Needs update', (i) => i.state === 'update' || i.state === 'resume'],
  ],
};

function allItems() {
  return S.mode === 'backup' ? backupItems() : restoreItems();
}

function visibleItems() {
  const filter = FILTERS[S.mode].find((f) => f[0] === S.filter) || FILTERS[S.mode][0];
  const q = S.search.trim().toLowerCase();
  return allItems().filter((i) => filter[2](i) && (!q || i.name.toLowerCase().includes(q)));
}

const STICKERS = {
  backup: { done: ['BACKED UP ✓', 'done'], update: ['UPDATE', 'update'], resume: ['UNFINISHED', 'resume'] },
  restore: { done: ['INSTALLED ✓', 'done'], update: ['UPDATE', 'update'], resume: ['UNFINISHED', 'resume'], newer: ['PC NEWER', 'muted'], broken: ['INCOMPLETE', 'muted'] },
};
const ACTIONS = {
  backup: { new: 'Back up →', resume: 'Resume →', update: 'Update →', done: 'Details' },
  restore: { new: 'Restore →', resume: 'Resume →', update: 'Update →', done: 'Details', newer: 'Details', broken: 'Unfinished' },
};

function stickerFor(item) {
  if (item.state === 'busy') {
    const verb = item.job.kind === 'verify' ? 'CHECKING' : item.job.kind === 'restore' ? 'RESTORING' : 'COPYING';
    return [verb, 'busy'];
  }
  if (item.state === 'queued') return [`QUEUED #${item.job.queued}`, 'queued'];
  return STICKERS[S.mode][item.state] || null;
}

// ------------------------------------------------------------------------------------------ disc art

function discEl(item, cls = 'disc') {
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
    // One page: the sheet lifts off the rings on the left and turns away (or comes back for "previous").
    plan = dir > 0
      ? { anchor: 'left', page: left, front: oldLeft, back: null, cover: null, reverse: false }
      : { anchor: 'left', page: left, front: snapshot(left), back: null, cover: { page: left, el: oldLeft }, reverse: true };
  } else if (dir > 0) {
    // Right-hand sheet turns over the rings onto the left side.
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
 * the rings follows. Front strips show the old page, back strips show the page being revealed.
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

function sleeveEl(item, i) {
  const pct = item.state === 'busy' && S.progress && S.progress.id === item.job.id ? progressPct(S.progress) : 0;
  const sticker = stickerFor(item);
  const pocket = h('div', { class: 'pocket' },
    item.state === 'busy' ? h('div', { class: 'ring', style: `--p:${pct}` }) : null,
    discEl(item),
  );
  const actionLabel = item.state === 'busy' ? `${pct.toFixed(0)}%` : item.state === 'queued' ? 'Queued' : ACTIONS[S.mode][item.state];
  const action = h('button', {
    class: `strip-action ${S.mode === 'restore' ? 'restore' : ''}`,
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
      loc.append(
        h('div', { class: 'loc-group' }, h('span', { class: 'loc-label' }, 'Back up to'),
          h('button', { class: 'btn primary', onclick: chooseDestination }, 'Choose a backup drive…')),
        h('div', { class: 'loc-stats' }, h('span', {}, h('b', {}, S.games.length), ' games on this PC')),
      );
      return;
    }
    const items = backupItems();
    const count = (st) => items.filter((i) => i.state === st).length;
    loc.append(
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
  } else {
    const r = S.restore;
    const sourceOptions = [...new Set([...(r.binder ? [r.binder.root] : []), ...r.binders.map((b) => b.path)])];
    const sourceSel = h('select', { class: 'select', onchange: (e) => e.target.value === '__browse' ? chooseRestoreSource() : loadRestoreSource(e.target.value) },
      ...sourceOptions.map((p) => h('option', { value: p, selected: r.binder && p === r.binder.root }, p)),
      h('option', { value: '__browse' }, 'Browse…'));
    const libOptions = [...new Set([...(r.library ? [r.library] : []), ...r.libraries.map((l) => l.path)])];
    const libSel = h('select', { class: 'select', onchange: (e) => e.target.value === '__browse' ? chooseRestoreTarget() : setRestoreTarget(e.target.value) },
      ...libOptions.map((p) => h('option', { value: p, selected: p === r.library }, p)),
      h('option', { value: '__browse' }, 'Browse…'));
    const items = restoreItems();
    loc.append(
      h('div', { class: 'loc-group' }, h('span', { class: 'loc-label' }, 'From'),
        sourceOptions.length ? sourceSel : h('button', { class: 'btn blue', onclick: chooseRestoreSource }, 'Choose backup drive…')),
      h('span', { class: 'arrow' }, '→'),
      h('div', { class: 'loc-group' }, h('span', { class: 'loc-label' }, 'Into'),
        libOptions.length ? libSel : h('button', { class: 'btn', onclick: chooseRestoreTarget }, 'Choose Steam library…'),
        h('button', { class: 'btn ghost', onclick: refreshRestore, title: 'Rescan' }, '↻')),
      S.steamRunning ? h('div', { class: 'warn' }, '⚠ Steam is running. Exit it (tray icon too) before restoring.') : null,
      h('div', { class: 'loc-stats' },
        h('span', {}, h('b', {}, items.length), ' discs'),
        h('span', {}, h('b', {}, items.filter((i) => i.state === 'done' || i.state === 'newer').length), ' installed')),
    );
  }
}

function progressPct(p) {
  return p && p.bytes_total ? Math.min(100, (p.bytes_done / p.bytes_total) * 100) : 0;
}

function renderDeck() {
  if (S.updating) return renderUpdateDeck();
  const cur = S.queue.current;
  const tray = $('#trayDisc');
  tray.classList.remove('update-disc');
  const p = S.progress && cur && S.progress.id === cur.id ? S.progress : null;
  $('#stopBtn').disabled = !cur;
  if (cur) {
    const gameItem = { app_id: cur.app_id, art: artFor(cur.app_id) };
    tray.style.backgroundImage = artSources(gameItem).map((s) => `url("${s}")`).join(', ');
    tray.classList.add('loaded', 'spinning');
    const verb = { backup: 'BACKING UP', restore: 'RESTORING', verify: 'CHECKING' }[cur.spec.kind];
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
      $('#lcdStats').textContent = S.mode === 'backup' ? 'PICK A GAME TO BACK IT UP' : 'PICK A DISC TO RESTORE IT';
    }
    $('#lcdEta').textContent = '';
    $('#lcdFile').textContent = '';
  }

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
  const key = (cur.spec.kind === 'restore' ? 'r-' : 'b-') + cur.app_id;
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
  return { new: 'Restore to PC', resume: 'Resume restore', update: 'Update on PC' }[item.state] || null;
}

// ------------------------------------------------------------------------------------------ actions

async function primaryAction(item, force) {
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
    dialog('That isn\'t a Steam library', `No Steam library was found at ${picked}.\n\nOpen Steam > Settings > Storage, add a library on that drive, exit Steam, then choose it here.`, [['OK', 'primary']]);
    renderLocation();
    return;
  }
  setRestoreTarget(lib);
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
  const source = r.binder?.root || S.settings.restore_source || binders[0]?.path || S.settings.destination;
  if (source) {
    try { r.binder = await call('scan_binder', { path: source }); } catch (e) { showError(e); }
  }
  await loadInstalled();
  S.steamRunning = await call('steam_running').catch(() => false);
  r.loaded = true;
  renderAll();
}

async function setMode(mode) {
  if (S.mode === mode) return;
  S.mode = mode;
  S.filter = 'all';
  S.spread = 0;
  renderAll();
  if (mode === 'restore') await refreshRestore();
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
      if (!S.queue.current && !S.queue.pending.length) return;
      event.preventDefault();
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
    const kindWord = { backup: 'Backup', restore: 'Restore', verify: 'Check' }[r.kind];
    if (r.outcome === 'success') toast(`${r.title} ✓`, r.message, 'ok');
    else if (r.outcome === 'cancelled') toast(`${kindWord} stopped`, `${r.title}: ${r.message}`, '');
    else toast(`${kindWord} failed: ${r.title}`, r.message, 'err');
    if (r.kind === 'restore') await loadInstalled();
    else await loadBinder();
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
    if (S.mode !== 'restore') return;
    const running = await call('steam_running').catch(() => false);
    if (running !== S.steamRunning) { S.steamRunning = running; renderLocation(); }
  }, 4000);
  setInterval(() => { if (!S.queue.current && S.lastResult) renderDeck(); }, 15000);

  if (S.settings.auto_update_check !== false) setTimeout(() => checkForUpdate(false), 2500);
}

main();

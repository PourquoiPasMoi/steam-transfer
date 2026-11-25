'use strict';
/* Design-preview stand-in for the Rust backend. Only loaded when ui/index.html is opened
   in a normal browser (no window.__TAURI__). Never used by the real app. */
(function () {
  const G = (app_id, name, size, build) => ({
    app_id, name, install_dir: name, size_on_disk: size, build_id: build, state_flags: 4, fully_installed: true,
    game_path: `C:\\Program Files (x86)\\Steam\\steamapps\\common\\${name}`, manifest_path: '', library_root: 'C:\\Program Files (x86)\\Steam',
    art_url: null, last_updated: 0,
  });
  const games = [
    G('2073620', 'Arena Breakout: Infinite', 98287476826, '26473477'),
    G('2406770', 'Bodycam', 74623415803, '26707677'),
    G('202970', 'Call of Duty: Black Ops II', 3255287346, '24784288'),
    G('212910', 'Call of Duty: Black Ops II - Zombies', 6012705432, '24784239'),
    G('1621690', 'Core Keeper', 871472227, '26332184'),
    G('221100', 'DayZ', 33337688237, '26403755'),
    G('548430', 'Deep Rock Galactic', 3963988601, '26107973'),
    G('2507950', 'Delta Force', 112779518896, '26555303'),
    G('534380', 'Dying Light 2: Reloaded Edition', 79268386234, '26404378'),
    G('2231380', 'Ghost Recon Breakpoint', 71163310039, '25349072'),
    G('4962020', 'No Body No Problem! Playtest', 3902882144, '26780520'),
    G('2139460', 'Once Human', 129400593069, '26444881'),
    G('739630', 'Phasmophobia', 44706808096, '25811160'),
    G('578080', 'PUBG: BATTLEGROUNDS', 56085479389, '26395528'),
    G('2444750', 'Shape of Dreams', 12879471087, '26040602'),
    G('1577120', 'The Quarry', 49828775749, '11105655'),
    G('2997360', 'The Other Side Demo', 11350740826, '22472975'),
    G('3041230', 'Windrose', 22415016556, '24803703'),
  ];
  const pkg = (id, status, build) => {
    const g = games.find((x) => x.app_id === id);
    return { path: `D:\\Steam Transfer\\${g.name} (App ${id})`, app_id: id, name: g.name, install_dir: g.name, build_id: build ?? g.build_id, status, bytes: g.size_on_disk, files: 1234, completed_at: '2026-11-20 21:14:03', art_url: null };
  };
  const packages = [
    pkg('2406770', 'incomplete'), pkg('202970', 'complete'), pkg('212910', 'complete'), pkg('1621690', 'complete'),
    pkg('534380', 'complete'), pkg('739630', 'complete'), pkg('3041230', 'complete'), pkg('548430', 'complete', '25900001'),
  ];
  const listeners = {};
  const emit = (name, payload) => (listeners[name] || []).forEach((cb) => cb({ payload }));
  const queue = { current: null, pending: [] };
  let nextId = 1;
  let timer = null;

  function startNext() {
    if (queue.current || !queue.pending.length) return;
    queue.current = queue.pending.shift();
    emit('queue', JSON.parse(JSON.stringify(queue)));
    const total = 6.4e9, files = 2140;
    let done = 0;
    timer = setInterval(() => {
      done = Math.min(total, done + total / 60);
      emit('progress', { id: queue.current.id, app_id: queue.current.app_id, title: queue.current.title, stage: 'Copying', file: `Content\\Paks\\pakchunk${Math.floor(done / 1e8)}-WindowsNoEditor.pak`, bytes_done: done, bytes_total: total, files_done: Math.floor((done / total) * files), files_total: files, speed: 142e6, eta_secs: (total - done) / 142e6 });
      if (done >= total) finish('success');
    }, 250);
  }
  function finish(outcome) {
    clearInterval(timer);
    const j = queue.current;
    queue.current = null;
    if (outcome === 'success' && j.spec.kind === 'backup') {
      const g = games.find((x) => x.app_id === j.app_id);
      const i = packages.findIndex((p) => p.app_id === j.app_id);
      const fresh = pkg(j.app_id, 'complete', g.build_id);
      if (i >= 0) packages[i] = fresh; else packages.push(fresh);
    }
    emit('job-finished', { id: j.id, kind: j.spec.kind, app_id: j.app_id, title: j.title, outcome, message: outcome === 'success' ? 'Backup complete and verified.' : 'Stopped. Everything copied so far is kept.', files: 1, bytes: 1 });
    emit('queue', JSON.parse(JSON.stringify(queue)));
    setTimeout(startNext, 300);
  }

  const handlers = {
    app_version: () => '2.0.0',
    get_settings: () => ({ destination: 'D:\\' }),
    save_settings: () => null,
    queue_state: () => queue,
    scan_library: () => games,
    scan_binder: () => ({ root: 'D:\\Steam Transfer', exists: true, free_bytes: 412e9, packages: packages.slice().sort((a, b) => a.name.localeCompare(b.name)) }),
    find_binders: () => [{ path: 'D:\\Steam Transfer', free_bytes: 412e9 }],
    list_libraries: () => [{ path: 'C:\\Program Files (x86)\\Steam', free_bytes: 220e9 }, { path: 'E:\\SteamLibrary', free_bytes: 800e9 }],
    library_state: () => ({ '202970': { build_id: '24784288', partial: false }, '1621690': { build_id: '26000000', partial: false }, '739630': { build_id: '', partial: true } }),
    detect_library: ({ path }) => path,
    steam_running: () => false,
    pick_folder: () => 'D:\\',
    open_folder: () => null,
    check_update: () => null,
    enqueue: ({ spec, title, appId }) => {
      const id = nextId++;
      queue.pending.push({ id, spec, title, app_id: appId });
      emit('queue', JSON.parse(JSON.stringify(queue)));
      setTimeout(startNext, 100);
      return id;
    },
    cancel_job: ({ id }) => {
      if (queue.current?.id === id) { finish('cancelled'); return true; }
      queue.pending = queue.pending.filter((j) => j.id !== id);
      emit('queue', JSON.parse(JSON.stringify(queue)));
      return true;
    },
  };

  window.MockApi = {
    invoke: async (cmd, args) => {
      if (!handlers[cmd]) throw new Error(`mock: no handler for ${cmd}`);
      return handlers[cmd](args || {});
    },
    listen: async (name, cb) => {
      (listeners[name] = listeners[name] || []).push(cb);
      return () => { listeners[name] = listeners[name].filter((x) => x !== cb); };
    },
    win: null,
  };
})();

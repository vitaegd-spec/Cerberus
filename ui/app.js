(() => {
  'use strict';
  const T = window.__TAURI__;
  if (!T || !T.core) {
    document.body.innerHTML = '<p style="padding:2em;color:#ff5d6c">Tauri API not available.</p>';
    return;
  }
  const invoke = T.core.invoke;
  const listen = T.event.listen;
  const $ = (id) => document.getElementById(id);

  let logSeq = 0;
  let lastListKey = '';
  let localBusy = 0;
  let mounts = [];

  // ---------- helpers ----------
  const esc = (s) => String(s ?? '').replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));
  const base = (p) => String(p).split(/[\\/]/).pop();
  function fmtSize(n) {
    if (!n || n < 0) return '0 B';
    const u = ['B', 'KB', 'MB', 'GB', 'TB'];
    let i = 0; let v = n;
    while (v >= 1024 && i < u.length - 1) { v /= 1024; i++; }
    return (i === 0 ? v : v.toFixed(v < 10 ? 2 : 1)) + ' ' + u[i];
  }
  function toast(msg, kind = 'err', ms = 7000) {
    const el = document.createElement('div');
    el.className = 'toast ' + kind;
    el.textContent = msg;
    $('toasts').appendChild(el);
    setTimeout(() => el.remove(), ms);
  }
  const writable = (m) => !m.readonly && !m.iso && !m.optical && !m.archive;
  // Never offer Initialize on a disk that holds partitions Windows can't read (e.g. Linux ext4).
  const canInit = (m) => writable(m) && m.disk_number != null && (m.letters || []).length === 0 && (m.skipped || []).length === 0;
  const canFormat = (m) => writable(m) && (m.letters || []).length > 0;
  const canWsl = (m) => !m.optical && !m.archive && m.disk_number != null && (m.skipped || []).some((s) => s.includes('Linux ext'));

  // ---------- read-only preference ----------
  // Stored in CERBERUS's settings (also used for Explorer / double-click mounts).
  const chk = $('chk-ro');
  chk.checked = true;
  invoke('get_settings').then((v) => { chk.checked = !!v.settings.readonly_default; }).catch(() => {});
  chk.addEventListener('change', () => {
    invoke('set_readonly_default', { readonly: chk.checked }).catch((e) => toast(String(e)));
  });

  // ---------- rendering ----------
  function renderStatus(s) {
    $('version').textContent = 'v' + s.version + ' · runs in the tray';
    const pa = $('pill-admin');
    pa.className = 'pill ' + (s.admin ? 'ok' : 'neutral');
    pa.textContent = s.admin ? 'Admin helper ready' : 'Admin: on demand';
    pa.title = s.admin ? '' : 'Administrator permission (UAC) is requested the first time a disk or disc image is mounted';
    const pe = $('pill-engine');
    pe.className = 'pill ' + (s.target_ok ? 'ok' : 'bad');
    pe.textContent = s.target_ok ? 'iSCSI engine :' + s.port : 'iSCSI engine offline';

    const problems = [];
    if (!s.target_ok) problems.push('iSCSI engine: ' + s.target_error + ' — only ISO/VHD/VHDX and archives can be mounted until this is fixed.');
    const b = $('banner');
    b.innerHTML = problems.map(esc).join('<br>');
    b.classList.toggle('hidden', problems.length === 0);

    const busyText = s.busy || (localBusy > 0 ? 'Working…' : (!s.ready ? 'Starting…' : ''));
    $('busy').classList.toggle('hidden', !busyText);
    $('busy-text').textContent = busyText;
    $('btn-unmount-all').disabled = s.mounts.length === 0;
    if (s.closing) $('closing').classList.remove('hidden');

    mounts = s.mounts;
    const key = JSON.stringify(s.mounts);
    if (key !== lastListKey) {
      lastListKey = key;
      renderList(s.mounts);
    }
  }

  function renderList(list) {
    const el = $('list');
    if (list.length === 0) {
      el.innerHTML = '<div class="empty"><b>No images mounted</b>Drag &amp; drop images onto this window, or click <i>Mount image…</i>' +
        '<small>Disks: ISO · VHD · VHDX · IMG · IMA · FLP · RAW · VDI · VMDK · OVA · CHD<br>' +
        'CD/DVD: BIN/CUE · MDS/MDF · CCD/IMG · CDI · NRG · CHD<br>' +
        'Archives: ZIP · 7Z · RAR · TAR · TAR.GZ · TGZ · GZ · CAB · split (.7z.001, .part1.rar)<br>' +
        'Tip: enable “Mount with CERBERUS” in the Explorer right-click menu under ⚙ Settings</small></div>';
      return;
    }
    el.innerHTML = list.map((m) => {
      const letters = m.letters || [];
      const skipped = m.skipped || [];
      let lclass = letters.length === 0 ? 'letter none' : (letters.length > 1 ? 'letter multi' : 'letter');
      if (m.optical && letters.length) lclass += ' cd';
      if (m.archive && letters.length) lclass += ' arc';
      const ltext = letters.length === 0 ? '—' : letters.map((l) => l + ':').join(' ');
      const open = letters.map((l) => `<button class="btn small" type="button" data-act="open" data-letter="${esc(l)}">Open ${esc(l)}:</button>`).join('');
      const init = canInit(m) ? `<button class="btn small warn" type="button" data-act="init" data-id="${esc(m.id)}">Initialize &amp; Format…</button>` : '';
      const fmt = canFormat(m) ? `<button class="btn small warn" type="button" data-act="format" data-id="${esc(m.id)}">Format…</button>` : '';
      const wslList = m.wsl || [];
      const wsl = wslList.length
        ? wslList.map((p) => `<button class="btn small" type="button" data-act="folder" data-path="${esc(p)}" title="${esc(p)}">WSL ${esc(p.slice(p.lastIndexOf('p')))}</button>`).join('')
        : (canWsl(m) ? `<button class="btn small" type="button" data-act="wsl" data-id="${esc(m.id)}" title="Attach the Linux partition(s) to WSL 2 and browse them in Explorer">Open in WSL</button>` : '');
      const folder = m.folder ? `<button class="btn small ghost" type="button" data-act="folder" data-path="${esc(m.folder)}" title="Open the projection folder behind the drive letter">Folder</button>` : '';
      let mode;
      if (m.overlay && !m.readonly) mode = '<span class="chip ovl" title="The original image is never modified">Read-write · changes file</span>';
      else mode = `<span class="chip ${m.readonly ? 'ro' : 'rw'}">${m.readonly ? 'Read-only' : 'Read-write'}</span>`;
      const saved = m.saved_changes > 0 ? `<span class="chip ovl">${esc(fmtSize(m.saved_changes))} saved changes applied</span>` : '';
      const foreign = skipped.length
        ? `<span class="chip ro" title="${esc(skipped.join('\n'))}">${skipped.length} unreadable partition${skipped.length > 1 ? 's' : ''}</span>`
        : '';
      return `<div class="card">
        <div class="${lclass}">${esc(ltext)}</div>
        <div class="info">
          <div class="name" title="${esc(m.name)}">${esc(m.name)}</div>
          <div class="path" title="${esc(m.path)}">${esc(m.path)}</div>
          ${m.contents ? `<div class="detail" title="${esc(m.contents)}">${esc(m.contents)}</div>` : ''}
          <div class="chips">
            <span class="chip ${m.optical ? 'cd' : (m.archive ? 'arc' : '')}">${esc(m.kind)}</span>
            <span class="chip route">${esc(m.route)}</span>
            ${mode}
            ${saved}
            ${foreign}
            ${wslList.length ? '<span class="chip wsl">Attached to WSL</span>' : ''}
            <span class="chip">${esc(fmtSize(m.size))}</span>
            ${m.disk_number != null ? `<span class="chip">Disk ${esc(m.disk_number)}</span>` : ''}
            ${m.style && m.style !== 'RAW' ? `<span class="chip">${esc(m.style)}</span>` : ''}
          </div>
          ${m.note ? `<div class="note">⚠ ${esc(m.note)}</div>` : ''}
        </div>
        <div class="actions">
          ${open}
          ${init}
          ${fmt}
          ${wsl}
          ${folder}
          <button class="btn small ghost" type="button" data-act="reveal" data-path="${esc(m.path)}" title="Show image file in Explorer">File</button>
          <button class="btn small danger" type="button" data-act="unmount" data-id="${esc(m.id)}">Unmount</button>
        </div>
      </div>`;
    }).join('');
  }

  async function refresh() {
    try { renderStatus(await invoke('get_status')); } catch (e) { /* ignore transient */ }
  }

  async function pollLog() {
    try {
      const lines = await invoke('get_log', { since: logSeq });
      if (!lines.length) return;
      const box = $('log');
      const atBottom = box.scrollTop + box.clientHeight >= box.scrollHeight - 8;
      for (const l of lines) {
        logSeq = Math.max(logSeq, l.seq);
        const div = document.createElement('div');
        div.className = 'l-' + l.level;
        div.innerHTML = `<span class="t">${l.t.toFixed(1).padStart(7, ' ')}</span>${esc(l.msg)}`;
        box.appendChild(div);
      }
      while (box.childElementCount > 500) box.firstElementChild.remove();
      if (atBottom) box.scrollTop = box.scrollHeight;
    } catch (e) { /* ignore */ }
  }

  // ---------- format dialog ----------
  const fmtDlg = $('fmt');
  let fmtCtx = null; // { mode: 'init' | 'format', m }

  function labelMax(fs) {
    return fs === 'NTFS' ? 32 : (fs === 'exFAT' ? 15 : 11);
  }
  function syncLabelMax() {
    const inp = $('fmt-label');
    const max = labelMax($('fmt-fs').value);
    inp.maxLength = max;
    if (inp.value.length > max) inp.value = inp.value.slice(0, max);
  }

  function openFormat(mode, m) {
    fmtCtx = { mode, m };
    const isInit = mode === 'init';
    $('fmt-title').textContent = isInit ? 'Initialize & Format' : 'Format drive';
    $('fmt-sub').textContent = `${m.name} · ${fmtSize(m.size)}${m.disk_number != null ? ' · Disk ' + m.disk_number : ''}`;
    $('fmt-row-style').classList.toggle('hidden', !isInit);
    $('fmt-row-letter').classList.toggle('hidden', isInit);
    $('fmt-warn').textContent = m.overlay
      ? 'ℹ The original image is not touched - the formatted contents go to its changes file.'
      : '⚠ This erases everything on the selected disk/drive inside the image file.';
    const sel = $('fmt-letter');
    sel.innerHTML = (m.letters || []).map((l) => `<option value="${esc(l)}">${esc(l)}:</option>`).join('');
    const tiny = m.size <= 4 * 1024 * 1024;
    $('fmt-fs').value = tiny ? 'FAT' : 'NTFS';
    $('fmt-style').value = m.removable ? 'MBR' : 'GPT';
    $('fmt-label').value = 'CERBERUS';
    $('fmt-full').checked = false;
    syncLabelMax();
    $('fmt-ok').textContent = isInit ? 'Initialize & Format' : 'Format';
    fmtDlg.classList.remove('hidden');
    setTimeout(() => $('fmt-fs').focus(), 0);
  }
  function closeFormat() {
    fmtDlg.classList.add('hidden');
    fmtCtx = null;
  }

  $('fmt-fs').addEventListener('change', syncLabelMax);
  $('fmt-cancel').addEventListener('click', closeFormat);
  fmtDlg.addEventListener('click', (e) => { if (e.target === fmtDlg) closeFormat(); });

  $('fmt-form').addEventListener('submit', async (e) => {
    e.preventDefault();
    if (!fmtCtx) return;
    const { mode, m } = fmtCtx;
    const fs = $('fmt-fs').value;
    const label = $('fmt-label').value.trim() || 'CERBERUS';
    const full = $('fmt-full').checked;

    if (mode === 'format') {
      const letter = $('fmt-letter').value;
      if (!letter) { toast('No drive letter selected'); return; }
      if (!confirm(`Format ${letter}: (${m.name}) as ${fs}?\n\nEVERYTHING on ${letter}: will be erased.`)) return;
      closeFormat();
      localBusy++;
      refresh();
      try {
        await invoke('format_drive', { id: m.id, letter, fs, label, full });
        toast(`${letter}: formatted as ${fs}`, 'ok', 4000);
      } catch (err) {
        toast(`Format failed: ${err}`);
      } finally { localBusy--; refresh(); }
    } else {
      const style = $('fmt-style').value;
      closeFormat();
      localBusy++;
      refresh();
      try {
        const r = await invoke('initialize_disk', { id: m.id, style, fs, label, full });
        const where = (r.letters || []).length ? r.letters.map((l) => l + ':').join(', ') : '(no drive letter)';
        toast(`${m.name} initialized (${style}) and formatted as ${fs} → ${where}`, 'ok', 5000);
      } catch (err) {
        toast(`Initialize & Format failed: ${err}`);
      } finally { localBusy--; refresh(); }
    }
  });

  // ---------- unmount (with keep/discard for changes files) ----------
  const umDlg = $('um');
  let umCtx = null;

  async function doUnmount(id, keep) {
    localBusy++;
    refresh();
    try {
      await invoke('unmount_image', { id, keep });
    } catch (e) {
      toast(String(e));
    } finally { localBusy--; refresh(); }
  }
  function closeUm() { umDlg.classList.add('hidden'); umCtx = null; }
  $('um-cancel').addEventListener('click', closeUm);
  umDlg.addEventListener('click', (e) => { if (e.target === umDlg) closeUm(); });
  $('um-keep').addEventListener('click', () => { const id = umCtx; closeUm(); if (id) doUnmount(id, true); });
  $('um-discard').addEventListener('click', () => {
    const id = umCtx;
    if (!id) return;
    if (!confirm('Discard all changes made to this image? This cannot be undone.')) return;
    closeUm();
    doUnmount(id, false);
  });

  document.addEventListener('keydown', (e) => {
    if (e.key !== 'Escape') return;
    if (fmtCtx) closeFormat();
    if (umCtx) closeUm();
    if (setOpen) closeSettings();
  });

  // ---------- settings (Phase 6) ----------
  const setDlg = $('set');
  let setOpen = false;
  const COMMON = ['chd', 'vdi', 'vmdk', 'cue', 'mds', 'nrg', 'cdi'];
  (() => {
    const sel = $('set-letter');
    for (let c = 90; c >= 68; c--) {
      const l = String.fromCharCode(c);
      sel.insertAdjacentHTML('beforeend', `<option value="${l}">${l}:</option>`);
    }
  })();

  async function loadSaved() {
    const box = $('set-saved');
    try {
      const list = await invoke('list_saved_changes');
      if (!list.length) {
        box.innerHTML = '<div class="saved-empty">No saved changes.</div>';
        return;
      }
      box.innerHTML = list.map((s) => {
        const when = s.modified ? new Date(s.modified * 1000).toLocaleString() : '';
        const state = s.mounted ? 'mounted now' : (s.path && !s.exists ? 'image file no longer exists' : when);
        return `<div class="saved-row">
          <div><div class="saved-name" title="${esc(s.path || s.name)}">${esc(s.name)}</div>
          <div class="saved-sub" title="${esc(s.path)}">${esc(state)}</div></div>
          <span class="saved-size">${esc(fmtSize(s.size))}</span>
          <button class="btn small danger" type="button" data-key="${esc(s.key)}" data-name="${esc(s.name)}" ${s.mounted ? 'disabled title="Unmount the image first"' : ''}>Delete</button>
        </div>`;
      }).join('');
    } catch (e) {
      box.innerHTML = `<div class="saved-empty">${esc(String(e))}</div>`;
    }
  }

  $('set-saved').addEventListener('click', async (e) => {
    const btn = e.target.closest('button[data-key]');
    if (!btn) return;
    if (!confirm(`Delete the saved changes for ${btn.dataset.name}?\n\nThe image goes back to exactly how it was. This cannot be undone.`)) return;
    btn.disabled = true;
    try {
      await invoke('delete_saved_changes', { key: btn.dataset.key });
      toast('Saved changes deleted', 'ok', 3000);
    } catch (err) { toast(String(err)); }
    loadSaved();
  });

  function setAssoc(list) {
    $('set-assoc').querySelectorAll('input').forEach((i) => { i.checked = list.includes(i.value); });
  }

  async function openSettings() {
    try {
      const v = await invoke('get_settings');
      const s = v.settings;
      const exts = v.extensions || [];
      $('set-assoc').innerHTML = exts.map((e) =>
        `<label class="check"><input type="checkbox" value="${esc(e)}"> .${esc(e)}</label>`).join('');
      $('set-autostart').checked = !!s.start_with_windows;
      $('set-restore').checked = !!s.restore_mounts;
      $('set-context').checked = !!s.context_menu;
      $('set-openafter').checked = !!s.open_after_external_mount;
      $('set-ro').checked = !!s.readonly_default;
      $('set-letter').value = s.archive_letter || 'Z';
      $('set-log').value = String(s.log_level || 0);
      setAssoc(s.associations || []);
      loadSaved();
      setDlg.classList.remove('hidden');
      setOpen = true;
    } catch (e) { toast(String(e)); }
  }
  function closeSettings() { setDlg.classList.add('hidden'); setOpen = false; }

  $('btn-settings').addEventListener('click', openSettings);
  $('set-cancel').addEventListener('click', closeSettings);
  setDlg.addEventListener('click', (e) => { if (e.target === setDlg) closeSettings(); });
  $('set-assoc-none').addEventListener('click', () => setAssoc([]));
  $('set-assoc-common').addEventListener('click', () => setAssoc(COMMON));
  $('set-defapps').addEventListener('click', () => invoke('open_default_apps').catch((e) => toast(String(e))));
  $('set-form').addEventListener('submit', async (e) => {
    e.preventDefault();
    const settings = {
      start_with_windows: $('set-autostart').checked,
      restore_mounts: $('set-restore').checked,
      context_menu: $('set-context').checked,
      associations: [...$('set-assoc').querySelectorAll('input:checked')].map((i) => i.value),
      readonly_default: $('set-ro').checked,
      open_after_external_mount: $('set-openafter').checked,
      archive_letter: $('set-letter').value,
      log_level: parseInt($('set-log').value, 10) || 0,
    };
    try {
      const s = await invoke('set_settings', { settings });
      chk.checked = !!s.readonly_default;
      closeSettings();
      toast('Settings saved', 'ok', 3000);
    } catch (err) { toast('Settings: ' + err); }
  });

  // ---------- actions ----------
  async function mountPaths(paths) {
    for (const p of paths) {
      localBusy++;
      refresh();
      try {
        const m = await invoke('mount_image', { path: p, readonly: chk.checked });
        const where = (m.letters || []).length ? 'as ' + m.letters.map((l) => l + ':').join(', ') : '(no drive letter)';
        const sk = (m.skipped || []).length;
        toast(`${m.name} mounted ${where}` + (sk ? ` - ${sk} partition(s) Windows can't read were skipped` : ''), sk ? 'err' : 'ok', sk ? 7000 : 4000);
      } catch (e) {
        toast(`${base(p)}: ${e}`);
      } finally {
        localBusy--;
        refresh();
      }
    }
  }

  $('btn-mount').addEventListener('click', async () => {
    try {
      const files = await invoke('pick_files');
      if (files && files.length) mountPaths(files);
    } catch (e) { toast(String(e)); }
  });

  $('btn-unmount-all').addEventListener('click', async () => {
    if (!mounts.length) return;
    const ovl = mounts.some((m) => m.overlay && !m.readonly);
    const msg = `Unmount all ${mounts.length} image(s)?` + (ovl ? '\n\nChanges files are kept and re-applied next time.' : '');
    if (!confirm(msg)) return;
    localBusy++;
    refresh();
    try {
      const errs = await invoke('unmount_all');
      if (errs.length) errs.forEach((e) => toast(e)); else toast('All images unmounted', 'ok', 3000);
    } catch (e) { toast(String(e)); }
    finally { localBusy--; refresh(); }
  });

  $('list').addEventListener('click', async (ev) => {
    const btn = ev.target.closest('button[data-act]');
    if (!btn) return;
    const act = btn.dataset.act;
    try {
      if (act === 'open') await invoke('open_drive', { letter: btn.dataset.letter });
      else if (act === 'reveal') await invoke('reveal_image', { path: btn.dataset.path });
      else if (act === 'folder') await invoke('open_folder', { path: btn.dataset.path });
      else if (act === 'wsl') {
        btn.disabled = true;
        localBusy++;
        refresh();
        try {
          const r = await invoke('open_in_wsl', { id: btn.dataset.id });
          toast(`${r.name}: Linux partition(s) available at ${(r.wsl || []).join(', ')}`, 'ok', 6000);
        } finally { localBusy--; refresh(); }
      }
      else if (act === 'init' || act === 'format') {
        const m = mounts.find((x) => x.id === btn.dataset.id);
        if (m) openFormat(act, m);
      } else if (act === 'unmount') {
        const m = mounts.find((x) => x.id === btn.dataset.id);
        if (m && m.overlay && !m.readonly) {
          umCtx = m.id;
          $('um-sub').textContent = m.name;
          umDlg.classList.remove('hidden');
          setTimeout(() => $('um-keep').focus(), 0);
        } else {
          btn.disabled = true;
          await doUnmount(btn.dataset.id, true);
        }
      }
    } catch (e) {
      btn.disabled = false;
      toast(String(e));
    }
  });

  $('btn-log').addEventListener('click', () => {
    const p = $('logpanel');
    p.classList.toggle('collapsed');
    $('btn-log').textContent = p.classList.contains('collapsed') ? 'Show' : 'Hide';
  });
  $('btn-log-clear').addEventListener('click', () => { $('log').innerHTML = ''; });

  // ---------- events from Rust ----------
  listen('files-dropped', (e) => { if (Array.isArray(e.payload) && e.payload.length) mountPaths(e.payload); });
  listen('drag-state', (e) => $('drop').classList.toggle('hidden', !e.payload));
  listen('closing', () => { closeFormat(); closeUm(); closeSettings(); $('closing').classList.remove('hidden'); });
  listen('toast', (e) => {
    const p = e.payload || {};
    if (p.msg) toast(p.msg, p.kind || 'ok', p.kind === 'err' ? 7000 : 4000);
    refresh();
  });
  listen('open-settings', () => openSettings());

  // Disable the WebView context menu / reload shortcuts for an app-like feel.
  document.addEventListener('contextmenu', (e) => { if (!e.target.closest('.log, .toast, input')) e.preventDefault(); });
  document.addEventListener('keydown', (e) => { if (e.key === 'F5' || (e.ctrlKey && e.key.toLowerCase() === 'r')) e.preventDefault(); });

  refresh();
  pollLog();
  setInterval(refresh, 1200);
  setInterval(pollLog, 1200);
})();

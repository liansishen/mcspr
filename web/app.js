'use strict';
/* MCS Panel 前端 */
const $ = (s, el = document) => el.querySelector(s);
const $$ = (s, el = document) => [...el.querySelectorAll(s)];
const esc = s => String(s ?? '').replace(/[&<>"']/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));
const fmtSize = n => {
  n = Number(n) || 0;
  if (n >= 1073741824) return (n / 1073741824).toFixed(2) + ' GB';
  if (n >= 1048576) return (n / 1048576).toFixed(1) + ' MB';
  if (n >= 1024) return (n / 1024).toFixed(1) + ' KB';
  return n + ' B';
};
const fmtUptime = s => {
  s = Number(s) || 0;
  const d = Math.floor(s / 86400), h = Math.floor(s % 86400 / 3600), m = Math.floor(s % 3600 / 60);
  if (d) return `${d}天${h}小时`;
  if (h) return `${h}小时${m}分`;
  return `${m}分${s % 60}秒`;
};

let TOKEN = localStorage.getItem('mcspr.token') || '';
let currentInstanceInfo = null;
let routeToken = 0;
let timers = [];
let activeWS = null;
let curPath = '';
let importTab = 'upload';
let propEntries = [];

function every(ms, fn) { timers.push(setInterval(fn, ms)); }
function clearTimers() {
  timers.forEach(t => clearInterval(t));
  timers = [];
  if (activeWS) { try { activeWS.onclose = null; activeWS.close(); } catch {} activeWS = null; }
}

function headers(extra = {}) {
  const h = { ...extra };
  if (TOKEN) h['Authorization'] = 'Bearer ' + TOKEN;
  return h;
}

async function api(path, opts = {}) {
  const h = headers(opts.headers || {});
  let body = opts.body;
  if (body !== undefined && !(body instanceof FormData) && typeof body !== 'string') {
    h['Content-Type'] = 'application/json';
    body = JSON.stringify(body);
  }
  const r = await fetch('/api' + path, { method: opts.method || 'GET', headers: h, body });
  if (r.status === 401) { showTokenModal(); throw new Error('需要访问令牌'); }
  if (!r.ok) {
    let msg = r.statusText;
    try { const j = await r.json(); msg = j.error || msg; } catch {}
    throw new Error(msg);
  }
  const ct = r.headers.get('content-type') || '';
  return ct.includes('application/json') ? r.json() : r.text();
}

function toast(msg, ok = true) {
  const el = document.createElement('div');
  el.className = 'toast' + (ok ? '' : ' err');
  el.textContent = msg;
  $('#toast-root').appendChild(el);
  setTimeout(() => el.remove(), 4000);
}

function showModal(html, cls = '') {
  $('#modal-root').innerHTML = `<div class="modal-backdrop" onclick="if(event.target===this)closeModal()"><div class="modal ${cls}">${html}</div></div>`;
}
let modsTabReload = null;
function closeModal() {
  const wasModDownload = !!$('#md-results');
  $('#modal-root').innerHTML = '';
  // 下载模组弹窗关闭后，刷新背后的模组列表
  if (wasModDownload && modsTabReload) modsTabReload();
}

function showTokenModal() {
  showModal(`<h2>需要访问令牌</h2>
    <p class="muted small" style="margin-bottom:12px">此面板启用了鉴权，请输入 config.toml 中配置的 token。</p>
    <input id="tok-input" placeholder="访问令牌" style="margin-bottom:14px">
    <div class="row right"><button class="btn primary" onclick="saveToken()">确定</button></div>`);
}
function saveToken() {
  TOKEN = $('#tok-input').value.trim();
  localStorage.setItem('mcspr.token', TOKEN);
  closeModal();
  route();
}

const STATUS_TEXT = { stopped: '已停止', starting: '启动中', running: '运行中', stopping: '停止中' };
function statusPill(s) { return `<span class="pill st-${esc(s)}">${STATUS_TEXT[s] || esc(s)}</span>`; }

/* ---------------- 主题切换（深色 / 亮色 / MC 像素） ---------------- */
const THEME_ORDER = ['dark', 'light', 'mc'];
const THEME_LABEL = { dark: '深色', light: '亮色', mc: 'MC 像素' };

function currentTheme() {
  const de = document.documentElement;
  return de.classList.contains('light') ? 'light' : de.classList.contains('mc') ? 'mc' : 'dark';
}
function applyTheme(t) {
  const de = document.documentElement;
  de.classList.toggle('light', t === 'light');
  de.classList.toggle('mc', t === 'mc');
  try { localStorage.setItem('mcspr.theme', t); } catch {}
  const btn = $('#theme-toggle');
  if (btn) btn.textContent = '🎨 ' + (THEME_LABEL[t] || t);
}
function toggleTheme() {
  applyTheme(THEME_ORDER[(THEME_ORDER.indexOf(currentTheme()) + 1) % THEME_ORDER.length]);
}
applyTheme(currentTheme());

function refresh() { route(); }

/* ---------------- 路由 ---------------- */
window.addEventListener('hashchange', route);

async function route() {
  clearTimers();
  const hash = location.hash.replace(/^#/, '') || '/dashboard';
  const parts = hash.split('/').filter(Boolean);
  $$('#nav a').forEach(a => {
    const nav = a.dataset.nav;
    const active = (nav === 'instances' && parts[0] === 'instance') || nav === parts[0];
    a.classList.toggle('active', active);
  });
  try {
    if (parts[0] === 'dashboard') await renderDashboard();
    else if (parts[0] === 'instances') await renderInstances();
    else if (parts[0] === 'instance' && parts[1]) await renderInstance(parts[1], parts[2] || 'console');
    else if (parts[0] === 'settings') await renderPanelSettings();
    else location.hash = '#/dashboard';
  } catch (e) {
    $('#main').innerHTML = `<div class="empty">加载失败: ${esc(e.message)}</div>`;
  }
}

/* ---------------- 仪表盘 ---------------- */
async function renderDashboard() {
  const t = ++routeToken;
  $('#main').innerHTML = `<h1>仪表盘</h1><div id="dash"><div class="empty">加载中…</div></div>`;
  const load = async () => {
    try {
      const s = await api('/stats');
      if (t !== routeToken) return;
      const cpu = (s.cpu_usage || 0);
      const memPct = s.mem_total ? (s.mem_used / s.mem_total * 100) : 0;
      const running = s.instances.filter(i => i.status === 'running').length;
      $('#dash').innerHTML = `
        <div class="grid stats-grid">
          <div class="card"><h3>CPU 使用率</h3><div class="big">${cpu.toFixed(1)}%</div><div class="bar"><i style="width:${Math.min(cpu, 100)}%"></i></div></div>
          <div class="card"><h3>系统内存</h3><div class="big">${fmtSize(s.mem_used)} <span class="muted small">/ ${fmtSize(s.mem_total)}</span></div><div class="bar"><i style="width:${memPct.toFixed(1)}%"></i></div></div>
          <div class="card"><h3>实例</h3><div class="big">${running} <span class="muted small">/ ${s.instances.length} 运行中</span></div></div>
        </div>
        <h2>实例概览</h2>
        <div class="grid cards-grid">${s.instances.map(i => {
          const p = (s.per_instance && s.per_instance[i.id]) || {};
          return `<div class="card inst-card">
            <div class="inst-head"><a href="#/instance/${i.id}/console">${esc(i.name)}</a>${statusPill(i.status)}</div>
            <div class="muted">${i.player_names.length ? '在线: ' + esc(i.player_names.join(', ')) : '无玩家在线'}</div>
            <div class="muted">${i.status === 'running'
              ? `运行 ${fmtUptime(i.uptime_secs)} · CPU ${(p.cpu || 0).toFixed(1)}% · 内存 ${fmtSize((p.mem_mb || 0) * 1048576)}`
              : (i.jar || i.jvm_args) ? '未运行' : '⚠ 未配置主程序，请先到实例设置中配置'}</div>
            <div class="row actions">
              ${i.status === 'stopped'
                ? `<button class="btn small primary" onclick="instStart('${i.id}')">▶ 启动</button>`
                : `<button class="btn small warn" onclick="instStop('${i.id}')">■ 停止</button>`}
              <a class="btn small ghost" href="#/instance/${i.id}/console">控制台</a>
              ${i.eula_accepted ? '' : `<span class="pill st-warn">需同意 EULA</span>`}
            </div>
          </div>`;
        }).join('') || '<div class="empty">还没有实例，去 <a href="#/instances">实例管理</a> 创建或导入</div>'}</div>`;
    } catch (e) {
      if (t === routeToken) $('#dash').innerHTML = `<div class="empty">加载失败: ${esc(e.message)}</div>`;
    }
  };
  await load();
  every(3000, load);
}

/* ---------------- 实例列表 ---------------- */
async function renderInstances() {
  const t = ++routeToken;
  $('#main').innerHTML = `
    <div class="page-head"><h1>实例管理</h1>
      <div class="row">
        <button class="btn primary" onclick="showImportModal()">📦 导入整合包</button>
        <button class="btn" onclick="showCreateModal()">＋ 新建空白实例</button>
      </div></div>
    <div id="inst-list"><div class="empty">加载中…</div></div>`;
  const load = async () => {
    try {
      const { instances } = await api('/instances');
      if (t !== routeToken) return;
      $('#inst-list').innerHTML = instances.length ? `<table class="table">
        <thead><tr><th>名称</th><th>状态</th><th>玩家</th><th>内存</th><th>操作</th></tr></thead>
        <tbody>${instances.map(i => `<tr>
          <td><a href="#/instance/${i.id}/console">${esc(i.name)}</a><div class="muted small">${i.jar ? esc(i.jar) : (i.jvm_args ? '启动参数模式' : '未配置主程序')}</div></td>
          <td>${statusPill(i.status)}</td>
          <td>${i.players ? esc(i.player_names.join(', ')) : '<span class="muted">-</span>'}</td>
          <td class="muted">${i.min_ram_mb}M ~ ${i.max_ram_mb}M</td>
          <td>
            ${i.status === 'stopped'
              ? `<button class="btn small primary" onclick="instStart('${i.id}')">启动</button>`
              : `<button class="btn small warn" onclick="instStop('${i.id}')">停止</button>
                 <button class="btn small" onclick="instRestart('${i.id}')">重启</button>`}
            <button class="btn small ghost" onclick="openFolder('${i.id}')">目录</button>
            <button class="btn small danger" onclick="delInstance('${i.id}','${esc(i.name)}')">删除</button>
          </td></tr>`).join('')}</tbody></table>`
        : '<div class="empty">暂无实例。点击右上角「导入整合包」或「新建空白实例」开始。</div>';
    } catch (e) {
      $('#inst-list').innerHTML = `<div class="empty">加载失败: ${esc(e.message)}</div>`;
    }
  };
  await load();
  every(3000, load);
}

let usersData = null;
let usersTab = 'online';
let usersInstanceId = null;

let createMode = 'vanilla';
function showCreateModal() {
  showModal(`<h2>新建实例</h2>
    <div class="tabs small" style="margin-bottom:14px">
      <button class="tab active" onclick="switchCreateTab('vanilla')">官方原版服务端</button>
      <button class="tab" onclick="switchCreateTab('modded')">模组服务端</button>
    </div>
    <label>实例名称<input id="ci-name" placeholder="留空则自动命名"></label>
    <div id="create-vanilla">
      <label>服务端版本（自动下载）
        <select id="ci-version"><option value="">不下载，稍后手动导入或配置</option></select>
        <div class="muted small">从官方源下载对应服务端 jar 并配置为主程序（国内网络自动切换 BMCLAPI 镜像）。模组整合包请使用「导入整合包」。</div>
      </label>
    </div>
    <div id="create-modded" style="display:none">
      <label>模组加载器
        <select id="ci-loader" onchange="loadLoaderVersions()">
          <option value="fabric">Fabric</option>
          <option value="quilt">Quilt</option>
          <option value="forge">Forge</option>
          <option value="neoforge">NeoForge</option>
        </select>
        <div class="muted small">Fabric / Quilt：下载官方一键启动器，首次启动自动补全依赖。Forge / NeoForge：运行官方安装器完整安装（需要几分钟，使用实例设置中的 Java）。安装完成后建议检查实例设置的 Java 是否满足该版本要求。</div>
      </label>
      <div class="row">
        <label style="flex:1">MC 版本<select id="ci-loader-game" onchange="loadLoaderVerList()"><option value="">加载中…</option></select></label>
        <label style="flex:1">加载器版本<select id="ci-loader-ver"><option value="">加载中…</option></select></label>
      </div>
    </div>
    <div id="ci-progress" style="display:none">
      <h3>安装进度</h3>
      <div class="bar" id="ci-barwrap"><i id="ci-bar" style="width:0%"></i></div>
      <pre id="ci-log" class="job-log"></pre>
    </div>
    <div class="row right"><button class="btn ghost" onclick="closeModal()">取消</button><button id="ci-go" class="btn primary" onclick="doCreate()">创建</button></div>`);
  createMode = 'vanilla';
  loadVersionOptions();
  loadLoaderVersions();
}
function switchCreateTab(m) {
  createMode = m;
  $$('#modal-root .tab').forEach(b => b.classList.remove('active'));
  const idx = m === 'vanilla' ? 0 : 1;
  $$('#modal-root .tab')[idx]?.classList.add('active');
  $('#create-vanilla').style.display = m === 'vanilla' ? '' : 'none';
  $('#create-modded').style.display = m === 'modded' ? '' : 'none';
}
async function loadLoaderVersions() {
  const loader = $('#ci-loader')?.value;
  const gameSel = $('#ci-loader-game');
  if (!loader || !gameSel) return;
  gameSel.innerHTML = '<option value="">加载中…</option>';
  const verSel = $('#ci-loader-ver');
  if (verSel) verSel.innerHTML = '<option value="">—</option>';
  try {
    const g = await api(`/loaders/${loader}/game-versions`);
    gameSel.innerHTML = g.versions.map(v => `<option value="${esc(v.id)}">${esc(v.id)}${v.stable ? '' : '（快照）'}</option>`).join('');
    await loadLoaderVerList();
  } catch (e) {
    gameSel.innerHTML = `<option value="">加载失败：${esc(e.message)}</option>`;
  }
}
async function loadLoaderVerList() {
  const loader = $('#ci-loader')?.value;
  const game = $('#ci-loader-game')?.value;
  const verSel = $('#ci-loader-ver');
  if (!loader || !verSel) return;
  verSel.innerHTML = '<option value="">加载中…</option>';
  try {
    const d = await api(`/loaders/${loader}/loader-versions?game=${encodeURIComponent(game)}`);
    verSel.innerHTML = d.versions.map(v => `<option value="${esc(v)}">${esc(v)}</option>`).join('');
  } catch (e) {
    verSel.innerHTML = `<option value="">加载失败：${esc(e.message)}</option>`;
  }
}
async function loadVersionOptions() {
  try {
    const d = await api('/versions');
    const sel = $('#ci-version');
    if (!sel) return;
    const groups = { release: ['正式版', []], snapshot: ['快照', []], old_beta: ['旧版 Beta', []], old_alpha: ['旧版 Alpha', []] };
    for (const v of d.versions) if (groups[v.type]) groups[v.type][1].push(v);
    let html = '<option value="">不下载，稍后手动导入或配置</option>';
    const rel = groups.release[1];
    if (rel.length) html += `<optgroup label="正式版（最新 ${esc(rel[0].id)}）">${rel.slice(0, 120).map((v, i) => `<option value="${esc(v.id)}">${i === 0 ? '⭐ 最新正式版 · ' : ''}${esc(v.id)}</option>`).join('')}</optgroup>`;
    for (const k of ['snapshot', 'old_beta', 'old_alpha']) {
      if (groups[k][1].length) html += `<optgroup label="${groups[k][0]}">${groups[k][1].slice(0, 40).map(v => `<option value="${esc(v.id)}">${esc(v.id)}</option>`).join('')}</optgroup>`;
    }
    sel.innerHTML = html;
  } catch (e) {
    const sel = $('#ci-version');
    if (sel) sel.innerHTML = `<option value="">不下载（版本清单获取失败：${esc(e.message)}）</option>`;
  }
}
function pollJob(jobId, onUpdate) {
  return new Promise((resolve, reject) => {
    const timer = setInterval(async () => {
      try {
        const j = await api(`/jobs/${jobId}`);
        if (onUpdate) onUpdate(j);
        if (j.status === 'done') { clearInterval(timer); resolve(j); }
        if (j.status === 'error') { clearInterval(timer); reject(new Error(j.logs.slice(-1)[0] || '操作失败')); }
      } catch (e) { clearInterval(timer); reject(e); }
    }, 700);
  });
}
async function doCreate() {
  const nameInput = $('#ci-name').value.trim();
  let name = nameInput;
  const body = {};
  if (createMode === 'modded') {
    const loader = $('#ci-loader').value;
    const game = $('#ci-loader-game').value;
    const lver = $('#ci-loader-ver').value;
    if (!game || !lver) return toast('版本列表尚未加载完成，请稍候', false);
    body.mc_version = game;
    body.mod_loader = loader;
    body.loader_version = lver;
    if (!name) name = `${$('#ci-loader').selectedOptions[0].text} ${game}`;
  } else {
    const ver = $('#ci-version') ? $('#ci-version').value : '';
    if (ver) body.mc_version = ver;
    if (!name && ver) name = `MC ${ver}`;
  }
  if (!name) return toast('请输入实例名称', false);
  body.name = name;
  try {
    const r = await api('/instances', { method: 'POST', body });
    if (!r.job_id) {
      closeModal();
      toast('已创建');
      location.hash = `#/instance/${r.id}/settings`;
      return;
    }
    $('#ci-go').disabled = true;
    $('#ci-progress').style.display = '';
    const j = await pollJob(r.job_id, j => {
      if (j.progress > 0) {
        $('#ci-barwrap').style.display = '';
        $('#ci-bar').style.width = j.progress + '%';
      } else {
        $('#ci-barwrap').style.display = 'none';
      }
      $('#ci-log').textContent = j.logs.join('\n');
      $('#ci-log').scrollTop = 1e6;
    });
    toast('安装完成，实例已就绪');
    setTimeout(() => { closeModal(); location.hash = `#/instance/${j.instance_id}/console`; }, 500);
  } catch (e) {
    toast(e.message, false);
    $('#ci-log') && ($('#ci-log').textContent += `\n[错误] ${e.message}`);
    $('#ci-go') && ($('#ci-go').disabled = false);
  }
}

function showImportModal() {
  importTab = 'upload';
  showModal(`<h2>导入整合包 / 服务端</h2>
    <div class="tabs small">
      <button class="tab active" data-t="upload" onclick="switchImportTab('upload')">上传 ZIP</button>
      <button class="tab" data-t="path" onclick="switchImportTab('path')">本地目录</button>
    </div>
    <div id="imp-upload">
      <label>实例名称（可选）<input id="im-name" placeholder="默认使用压缩包名"></label>
      <label>整合包 ZIP 文件<input id="im-file" type="file" accept=".zip"></label>
      <p class="muted small">支持常见服务端整合包（ServerPack）：自动解压、应用 overrides、识别主程序 JAR / Forge·NeoForge 启动参数。</p>
    </div>
    <div id="imp-path" style="display:none">
      <label>实例名称（可选）<input id="ip-name"></label>
      <label>服务器目录绝对路径<input id="ip-path" placeholder="例如 D:\\Servers\\MyPack"></label>
    </div>
    <div id="imp-progress" style="display:none"><h3>导入进度</h3><pre id="imp-log" class="job-log"></pre></div>
    <div class="row right"><button class="btn ghost" onclick="closeModal()">关闭</button><button id="imp-go" class="btn primary" onclick="doImport()">开始导入</button></div>`);
}
function switchImportTab(t) {
  importTab = t;
  $$('#modal-root .tab').forEach(b => b.classList.toggle('active', b.dataset.t === t));
  $('#imp-upload').style.display = t === 'upload' ? '' : 'none';
  $('#imp-path').style.display = t === 'path' ? '' : 'none';
}
async function doImport() {
  $('#imp-progress').style.display = '';
  $('#imp-go').disabled = true;
  const poll = jobId => new Promise((resolve, reject) => {
    const timer = setInterval(async () => {
      try {
        const j = await api(`/jobs/${jobId}`);
        $('#imp-log').textContent = j.logs.join('\n');
        $('#imp-log').scrollTop = 1e6;
        if (j.status === 'done') { clearInterval(timer); resolve(j); }
        if (j.status === 'error') { clearInterval(timer); reject(new Error('导入失败，详见日志')); }
      } catch (e) { clearInterval(timer); reject(e); }
    }, 800);
  });
  try {
    let jobId;
    if (importTab === 'upload') {
      const fileEl = $('#im-file');
      if (!fileEl.files[0]) throw new Error('请选择 ZIP 文件');
      const fd = new FormData();
      if ($('#im-name').value.trim()) fd.append('name', $('#im-name').value.trim());
      fd.append('file', fileEl.files[0]);
      $('#imp-log').textContent = '上传中…';
      const r = await uploadWithProgress('/instances/import/upload', fd, p => {
        $('#imp-log').textContent = `上传中… ${p}%`;
      });
      jobId = r.job_id;
    } else {
      const body = { path: $('#ip-path').value.trim() };
      if ($('#ip-name').value.trim()) body.name = $('#ip-name').value.trim();
      const r = await api('/instances/import/path', { method: 'POST', body });
      jobId = r.job_id;
    }
    const j = await poll(jobId);
    toast('导入完成');
    setTimeout(() => { closeModal(); location.hash = `#/instance/${j.instance_id}/console`; }, 500);
  } catch (e) {
    $('#imp-log').textContent += `\n[错误] ${e.message}`;
    toast('导入失败: ' + e.message, false);
    $('#imp-go').disabled = false;
  }
}
function uploadWithProgress(path, fd, onProg) {
  return new Promise((resolve, reject) => {
    const xhr = new XMLHttpRequest();
    xhr.open('POST', '/api' + path);
    if (TOKEN) xhr.setRequestHeader('Authorization', 'Bearer ' + TOKEN);
    xhr.upload.onprogress = e => { if (e.lengthComputable) onProg(Math.round(e.loaded / e.total * 100)); };
    xhr.onload = () => {
      if (xhr.status === 401) { showTokenModal(); return reject(new Error('需要访问令牌')); }
      try {
        const j = JSON.parse(xhr.responseText);
        if (xhr.status >= 200 && xhr.status < 300) resolve(j); else reject(new Error(j.error || xhr.statusText));
      } catch { reject(new Error('响应解析失败')); }
    };
    xhr.onerror = () => reject(new Error('网络错误'));
    xhr.send(fd);
  });
}

/* ---------------- 实例操作 ---------------- */
async function instStart(id) {
  try { await api(`/instances/${id}/start`, { method: 'POST' }); toast('启动指令已发送'); }
  catch (e) { toast(e.message, false); }
  refresh();
}
async function instStop(id) {
  try { await api(`/instances/${id}/stop`, { method: 'POST' }); toast('停止指令已发送'); }
  catch (e) { toast(e.message, false); }
  refresh();
}
async function instRestart(id) {
  try { await api(`/instances/${id}/restart`, { method: 'POST' }); toast('正在重启'); }
  catch (e) { toast(e.message, false); }
  refresh();
}
async function delInstance(id, name) {
  if (!confirm(`确定删除实例「${name}」？\n实例目录将被彻底删除，不可恢复！`)) return;
  try { await api(`/instances/${id}`, { method: 'DELETE' }); toast('已删除'); refresh(); }
  catch (e) { toast(e.message, false); }
}
async function openFolder(id) {
  try { await api(`/instances/${id}/open`, { method: 'POST' }); }
  catch (e) { toast(e.message, false); }
}
async function acceptEula(id) {
  try { await api(`/instances/${id}/eula`, { method: 'POST' }); toast('已同意 EULA'); }
  catch (e) { toast(e.message, false); }
}

/* ---------------- 实例详情页 ---------------- */
const INST_TABS = [['console', '控制台'], ['mods', '模组'], ['files', '文件'], ['props', '服务器设置'], ['settings', '实例设置']];

async function renderInstance(id, tab) {
  const t = ++routeToken;
  let s;
  try { s = await api(`/instances/${id}`); }
  catch (e) { $('#main').innerHTML = `<div class="empty">${esc(e.message)}</div>`; return; }
  currentInstanceInfo = s;
  $('#main').innerHTML = `
    <div class="page-head">
      <div><h1>${esc(s.name)} ${statusPill(s.status)}</h1><div class="muted small" id="inst-sub"></div></div>
      <div class="row" id="inst-actions"></div>
    </div>
    <div id="eula-banner"></div>
    <div class="tabs">${INST_TABS.map(([k, label]) =>
      `<a class="tab ${k === tab ? 'active' : ''}" href="#/instance/${id}/${k}">${label}</a>`).join('')}</div>
    <div id="tab-body"></div>`;
  const body = $('#tab-body');
  if (tab === 'console') renderTabConsole(id, body, t);
  else if (tab === 'mods') renderTabMods(id, body, t);
  else if (tab === 'files') renderTabFiles(id, body, t);
  else if (tab === 'props') renderTabProps(id, body, t);
  else renderTabSettings(id, body);

  const upd = async () => {
    try {
      const s2 = await api(`/instances/${id}/status`);
      if (t !== routeToken) return;
      $('#inst-actions').innerHTML = actionButtons(id, s2.status);
      $('#eula-banner').innerHTML =
        (!s2.eula_accepted && (s2.status === 'stopped'))
          ? `<div class="banner warn"><span>该实例尚未同意 Minecraft EULA，直接启动会失败。</span><button class="btn small" onclick="acceptEula('${id}')">同意 EULA 并继续</button></div>`
          : '';
      $('#inst-sub').textContent = (s2.status === 'running' || s2.status === 'starting')
        ? `已运行 ${fmtUptime(s2.uptime_secs)} · PID ${s2.pid || '-'} · ${s2.players} 名玩家在线${s2.player_names.length ? '：' + s2.player_names.join(', ') : ''}`
        : '';
    } catch {}
  };
  await upd();
  every(2500, upd);
}

function actionButtons(id, status) {
  const parts = [];
  if (status === 'stopped') parts.push(`<button class="btn primary" onclick="instStart('${id}')">▶ 启动</button>`);
  else if (status === 'running' || status === 'starting') parts.push(
    `<button class="btn warn" onclick="instStop('${id}')">■ 停止</button>`,
    `<button class="btn" onclick="instRestart('${id}')">⟳ 重启</button>`);
  else parts.push(`<span class="muted small">停止中…</span>`);
  parts.push(`<button class="btn ghost" onclick="openFolder('${id}')">打开目录</button>`);
  return parts.join('');
}

/* ---------------- 控制台 + 用户管理 ---------------- */
function renderTabConsole(id, el, t) {
  el.innerHTML = `
    <div class="console-wrap">
      <div id="console-log" class="console"></div>
      <div class="row console-input">
        <input id="cmd-input" placeholder="输入命令（如 list、say hello）后回车发送…" autocomplete="off">
        <button class="btn" id="cmd-send">发送</button>
      </div>
    </div>
    <div class="card" style="margin-top:16px">
      <div class="row between"><h2 style="margin:0">用户管理</h2><span id="users-mode"></span></div>
      <div class="tabs small" id="users-tabs" style="margin-top:12px"></div>
      <div id="users-body"></div>
      <div class="row" id="user-addrow" style="margin-top:12px">
        <input id="user-target" placeholder="玩家名" style="max-width:240px;width:auto;flex:1">
        <input id="user-reason" placeholder="封禁原因（可选）" style="max-width:280px;width:auto;flex:1;display:none">
        <button class="btn primary" id="user-add" onclick="userAdd('${id}')">添加</button>
      </div>
      <div class="muted small" id="users-hint" style="margin-top:10px"></div>
    </div>`;
  const logEl = $('#console-log');
  const maxLines = 800;
  let retryTimer = null;

  const append = o => {
    const nearBottom = logEl.scrollTop + logEl.clientHeight >= logEl.scrollHeight - 60;
    const div = document.createElement('div');
    div.className = 'cline';
    if (/ERROR|FATAL|Exception|崩溃/.test(o.line)) div.classList.add('err');
    else if (/WARN|警告/.test(o.line)) div.classList.add('warn');
    div.innerHTML = `<span class="ts">${esc(o.ts)}</span>${esc(o.line)}`;
    logEl.appendChild(div);
    while (logEl.children.length > maxLines) logEl.removeChild(logEl.firstChild);
    if (nearBottom) logEl.scrollTop = logEl.scrollHeight;
  };

  const connect = () => {
    if (t !== routeToken) return;
    const proto = location.protocol === 'https:' ? 'wss' : 'ws';
    const ws = new WebSocket(`${proto}://${location.host}/api/instances/${id}/ws?token=${encodeURIComponent(TOKEN)}`);
    activeWS = ws;
    ws.onmessage = e => { try { append(JSON.parse(e.data)); } catch {} };
    ws.onopen = () => { logEl.innerHTML = ''; };
    ws.onclose = () => {
      if (t === routeToken && activeWS === ws) { retryTimer = setTimeout(connect, 3000); }
    };
  };
  connect();

  const send = async () => {
    const v = $('#cmd-input').value.trim();
    if (!v) return;
    try {
      await api(`/instances/${id}/command`, { method: 'POST', body: { command: v } });
      $('#cmd-input').value = '';
    } catch (e) { toast(e.message, false); }
  };
  $('#cmd-send').onclick = send;
  $('#cmd-input').onkeydown = e => { if (e.key === 'Enter') send(); };

  // 用户管理
  usersInstanceId = id;
  loadUsers(id);
  every(8000, () => loadUsers(id));
}

const USER_TABS = [['online', '在线玩家'], ['ops', 'OP 列表'], ['whitelist', '白名单'], ['banned', '封禁玩家'], ['banned_ips', '封禁 IP']];
const USER_CONF = {
  ops: { add: 'op', rm: 'deop', placeholder: '玩家名', reason: false, hint: 'OP 拥有管理员权限（等级默认 4，即最高）。', cols: ['玩家', 'UUID', '等级'] },
  whitelist: { add: 'whitelist_add', rm: 'whitelist_remove', placeholder: '玩家名', reason: false, hint: '白名单需在「服务器设置」中开启 white-list 后才会拦截未添加的玩家。', cols: ['玩家', 'UUID'] },
  banned: { add: 'ban', rm: 'pardon', placeholder: '玩家名', reason: true, hint: '被封禁的玩家将无法进入服务器。', cols: ['玩家', '原因', '来源', '过期时间'] },
  banned_ips: { add: 'ban_ip', rm: 'pardon_ip', placeholder: 'IP 地址（如 192.168.1.10）', reason: true, hint: '按 IP 封禁会阻止该地址的所有账号连接。', cols: ['IP', '原因', '来源', '过期时间'] },
};

async function loadUsers(id) {
  try {
    usersData = await api(`/instances/${id}/users`);
    if (usersInstanceId === id) renderUsers(id);
  } catch {}
}
function setUserTab(k) { usersTab = k; renderUsers(usersInstanceId); }

function renderUsers(id) {
  const d = usersData;
  if (!d || !$('#users-mode')) return;
  $('#users-mode').innerHTML = d.running
    ? '<span class="pill st-running">命令模式 · 实时生效</span>'
    : '<span class="pill st-warn">文件模式 · 启动后生效</span>';
  $('#users-tabs').innerHTML = USER_TABS.map(([k, label]) =>
    `<button class="tab ${usersTab === k ? 'active' : ''}" onclick="setUserTab('${k}')">${label}</button>`).join('');

  if (usersTab === 'online') {
    $('#user-addrow').style.display = 'none';
    $('#users-hint').textContent = d.running
      ? '在线玩家来自控制台的进出记录；快捷操作（OP / 踢出 / 封禁 / 白名单）通过控制台命令实时生效。'
      : '服务器未运行，暂无在线玩家。';
    const list = d.online || [];
    $('#users-body').innerHTML = list.length ? `<table class="table">
      <thead><tr><th>玩家</th><th>快捷操作</th></tr></thead>
      <tbody>${list.map(p => `<tr><td><b>${esc(p)}</b></td><td>
        <button class="btn small primary" onclick="userAction('${id}','op','${esc(p)}')">OP</button>
        <button class="btn small" onclick="userAction('${id}','whitelist_add','${esc(p)}')">白名单</button>
        <button class="btn small warn" onclick="userAction('${id}','kick','${esc(p)}')">踢出</button>
        <button class="btn small danger" onclick="userAction('${id}','ban','${esc(p)}')">封禁</button>
      </td></tr>`).join('')}</tbody></table>`
      : '<div class="empty" style="padding:16px">当前没有在线玩家</div>';
    return;
  }

  $('#user-addrow').style.display = '';
  const conf = USER_CONF[usersTab];
  const map = { ops: d.ops || [], whitelist: d.whitelist || [], banned: d.banned || [], banned_ips: d.bannedIps || [] };
  const list = map[usersTab];
  $('#user-target').placeholder = conf.placeholder;
  $('#user-reason').style.display = conf.reason ? '' : 'none';
  $('#users-hint').textContent = conf.hint + (d.running
    ? ' 当前服务器运行中，操作通过控制台命令实时生效。'
    : ' 当前服务器未运行，操作将直接写入 JSON 配置文件（启动后生效）。');

  const shortUuid = u => (u && u.length > 18) ? u.slice(0, 8) + '…' + u.slice(-4) : (u || '-');
  const rows = list.map(e => {
    const name = e.name || e.ip || '';
    let vals;
    if (usersTab === 'ops') vals = [e.name || '-', shortUuid(e.uuid), String(e.level ?? '-')];
    else if (usersTab === 'whitelist') vals = [e.name || '-', shortUuid(e.uuid)];
    else vals = [e.name || e.ip || '-', e.reason || '-', e.source || '-', e.expires || '-'];
    return `<tr>${vals.map(v => `<td>${esc(v)}</td>`).join('')}
      <td><button class="btn small danger" onclick="userAction('${id}','${conf.rm}','${esc(name)}')">${usersTab === 'ops' ? '移除 OP' : usersTab === 'whitelist' ? '移除' : '解封'}</button></td></tr>`;
  }).join('');
  $('#users-body').innerHTML = list.length
    ? `<table class="table"><thead><tr>${conf.cols.map(c => `<th>${c}</th>`).join('')}<th>操作</th></tr></thead><tbody>${rows}</tbody></table>`
    : '<div class="empty" style="padding:16px">暂无条目</div>';
}

async function userAdd(id) {
  const conf = USER_CONF[usersTab];
  const target = $('#user-target').value.trim();
  if (!target) return toast(conf.reason ? '请输入玩家名或 IP' : conf.placeholder, false);
  const reason = conf.reason ? $('#user-reason').value.trim() : undefined;
  await userAction(id, conf.add, target, reason);
  $('#user-target').value = '';
  $('#user-reason').value = '';
}

async function userAction(id, action, target, reason) {
  try {
    const r = await api(`/instances/${id}/users/action`, {
      method: 'POST',
      body: { action, target, reason: reason || undefined },
    });
    if (r.warning) toast(r.warning, false);
    else toast(r.mode === 'command' ? `已执行命令（实时生效）` : '已写入配置文件');
    loadUsers(id);
  } catch (e) { toast(e.message, false); }
}

/* ---------------- 模组 ---------------- */
async function renderTabMods(id, el, t) {
  el.innerHTML = `
    <div class="row between"><h2>模组</h2>
      <div class="row"><button class="btn primary" onclick="showModDownload('${id}')">⬇ 下载模组</button><button class="btn" onclick="uploadMod('${id}')">上传模组</button></div></div>
    <div id="mods-body"><div class="empty">加载中…</div></div>`;
  const load = async () => {
    try {
      const { mods } = await api(`/instances/${id}/mods`);
      if (t !== routeToken) return;
      $('#mods-body').innerHTML = mods.length ? `<table class="table">
        <thead><tr><th>状态</th><th>名称</th><th>版本</th><th>加载器</th><th>MC 版本</th><th>大小</th><th>操作</th></tr></thead>
        <tbody>${mods.map(m => `<tr>
          <td><span class="pill ${m.enabled ? 'st-running' : 'st-stopped'}">${m.enabled ? '启用' : '禁用'}</span></td>
          <td title="${esc(m.description)}">${esc(m.display_name)}${m.authors ? `<div class="muted small">by ${esc(m.authors)}</div>` : ''}<div class="muted small">${esc(m.file)}</div></td>
          <td>${esc(m.version)}</td><td>${esc(m.loader)}</td><td>${esc(m.mc_version || '-')}</td><td>${fmtSize(m.size)}</td>
          <td>
            <button class="btn small" onclick="toggleMod('${id}','${esc(m.file)}')">${m.enabled ? '禁用' : '启用'}</button>
            <button class="btn small danger" onclick="deleteMod('${id}','${esc(m.file)}')">删除</button>
          </td></tr>`).join('')}</tbody></table>`
        : '<div class="empty">mods 目录为空。模组应放在实例目录的 mods 文件夹中。</div>';
    } catch (e) {
      $('#mods-body').innerHTML = `<div class="empty">${esc(e.message)}</div>`;
    }
  };
  modsTabReload = load;
  await load();
}
async function toggleMod(id, file) {
  try { await api(`/instances/${id}/mods/toggle`, { method: 'POST', body: { file } }); toast('已切换'); refresh(); }
  catch (e) { toast(e.message, false); }
}
async function deleteMod(id, file) {
  if (!confirm(`确定删除 ${file}？`)) return;
  try { await api(`/instances/${id}/mods/delete`, { method: 'POST', body: { file } }); toast('已删除'); refresh(); }
  catch (e) { toast(e.message, false); }
}
function uploadMod(id) {
  showModal(`<h2>上传模组</h2>
    <input type="file" id="mod-file" accept=".jar" multiple style="margin:10px 0 16px">
    <div class="row right"><button class="btn ghost" onclick="closeModal()">取消</button><button class="btn primary" onclick="doUploadMod('${id}')">上传</button></div>`);
}

/* ---------------- 模组下载（Modrinth / CurseForge，队列式批量安装） ---------------- */
let modDL = null;

function showModDownload(id) {
  id = id || (currentInstanceInfo && currentInstanceInfo.id);
  const meta = currentInstanceInfo || {};
  modDL = {
    id,
    source: 'modrinth',
    q: '',
    game: meta.mc_version || '',
    loader: meta.mod_loader || '',
    results: null,
    expanded: null,
    versions: {},
    sides: {},
    installed: new Set(),
    installedFiles: [],
    queue: [],
    cfOk: false,
  };
  const loaderSel = ['', 'fabric', 'forge', 'neoforge', 'quilt'].map(l =>
    `<option value="${l}" ${l === modDL.loader ? 'selected' : ''}>${l === '' ? '任意加载器' : l[0].toUpperCase() + l.slice(1)}</option>`).join('');
  showModal(`<h2>下载模组</h2>
    <div class="tabs small" id="md-tabs" style="margin-bottom:10px">
      <button class="tab active" onclick="switchModSource('modrinth')">Modrinth</button>
      <button class="tab" onclick="switchModSource('curseforge')">CurseForge</button>
    </div>
    <div class="banner warn" id="md-cfhint" style="display:none;padding:8px 12px">
      CurseForge 搜索需要在「面板设置」中填写 CurseForge API Key（console.curseforge.com 免费创建）。
    </div>
    <div class="row" style="margin:10px 0">
      <input id="md-q" placeholder="搜索模组…" style="flex:2;min-width:150px" onkeydown="if(event.key==='Enter')searchMods()">
      <input id="md-game" placeholder="游戏版本" value="${esc(modDL.game)}" style="max-width:110px;width:auto;flex:1" title="按游戏版本过滤，留空不过滤" onchange="modDL.game=this.value.trim()">
      <select id="md-loader" style="max-width:120px;width:auto;flex:1" title="按加载器过滤" onchange="modDL.loader=this.value">${loaderSel}</select>
      <button class="btn primary" onclick="searchMods()">搜索</button>
    </div>
    ${modDL.loader ? `<div class="muted small" style="margin:-4px 0 8px">已按实例的加载器（<b>${esc(modDL.loader)}</b>）与游戏版本（<b>${esc(modDL.game || '未记录')}</b>）预置过滤条件，可自行调整。</div>` : ''}
    <div id="md-results" class="empty" style="padding:18px">输入关键词搜索</div>
    <div id="md-queue" style="margin-top:12px"></div>
    <div class="row right" style="margin-top:12px">
      <button class="btn warn" id="md-restart" style="display:none" onclick="closeModal();instRestart('${id}')">↻ 重启服务器生效</button>
      <button class="btn ghost" onclick="closeModal()">关闭</button>
    </div>`, 'wide');
  applyModSourceHint();
  loadInstalledMods();
}
function switchModSource(s) {
  if (!modDL || modDL.source === s) return;
  modDL.source = s;
  modDL.results = null;
  modDL.expanded = null;
  modDL.versions = {};
  $$('#md-tabs .tab').forEach(b => b.classList.remove('active'));
  $$('#md-tabs .tab')[s === 'modrinth' ? 0 : 1]?.classList.add('active');
  applyModSourceHint();
  const res = $('#md-results');
  if (res) res.innerHTML = `<div class="empty">输入关键词搜索（${s === 'curseforge' ? 'CurseForge' : 'Modrinth'}）</div>`;
}
function applyModSourceHint() {
  const el = $('#md-cfhint');
  if (el) el.style.display = (modDL && modDL.source === 'curseforge' && !modDL.cfOk) ? '' : 'none';
}
async function loadInstalledMods() {
  try {
    const h = await api(`/instances/${modDL.id}/mods/hashes`);
    modDL.installedFiles = h.files || [];
    if (h.files.length) {
      const r = await api('/moddb/version-files', {
        method: 'POST',
        body: { hashes: h.files.map(f => f.sha1).filter(Boolean) },
      });
      modDL.installed = new Set(
        Object.values(r).map(v => v && v.project_id).filter(Boolean)
      );
      if (modDL.results) renderModResults();
    }
  } catch {}
}
async function searchMods() {
  const res = $('#md-results');
  if (!res || !modDL) return;
  modDL.q = $('#md-q').value.trim();
  modDL.game = $('#md-game').value.trim();
  modDL.loader = $('#md-loader').value;
  res.innerHTML = '<div class="empty">搜索中…</div>';
  try {
    const d = await api(`/moddb/search?source=${modDL.source}&q=${encodeURIComponent(modDL.q)}&game=${encodeURIComponent(modDL.game)}&loader=${encodeURIComponent(modDL.loader)}`);
    modDL.results = d.results;
    modDL.expanded = null;
    modDL.versions = {};
    // 拉取客户端/服务端支持标记（仅 Modrinth 提供）
    if (modDL.source === 'modrinth' && d.results.length) {
      try {
        const ids = d.results.map(m => m.id).join(',');
        const p = await api(`/moddb/projects?source=modrinth&ids=${encodeURIComponent(ids)}`);
        for (const pj of p.projects || []) modDL.sides[pj.id] = pj;
      } catch {}
    }
    renderModResults();
  } catch (e) {
    res.innerHTML = `<div class="empty">${esc(e.message)}</div>`;
  }
}
function renderModResults() {
  const res = $('#md-results');
  if (!res || !modDL) return;
  if (!modDL.results) { res.innerHTML = '<div class="empty">输入关键词搜索</div>'; return; }
  if (!modDL.results.length) { res.innerHTML = '<div class="empty">没有搜索结果</div>'; return; }
  res.innerHTML = `<table class="table"><thead><tr><th>模组</th><th>下载量</th><th>操作</th></tr></thead><tbody>` +
    modDL.results.map((m, i) => {
      const installed = modDL.installed.has(m.id);
      const inQueue = modDL.queue.some(q => q.projectId === m.id);
      const expanded = modDL.expanded === i;
      const side = modDL.sides[m.id];
      const blocked = !!side && side.server_side === 'unsupported';
      const serverOnly = !!side && !blocked && side.client_side === 'unsupported';
      const sideBadge = blocked
        ? '<span class="pill st-warn" style="font-size:10px;padding:1px 8px">纯客户端</span>'
        : serverOnly
          ? '<span class="pill st-running" style="font-size:10px;padding:1px 8px">仅服务端</span>'
          : '';
      const dis = blocked ? `disabled title="纯客户端模组（或标注不支持服务端），服务器无需下载"` : '';
      return `<tr><td>
          ${m.icon ? `<img src="${esc(m.icon)}" style="width:24px;height:24px;vertical-align:-6px;margin-right:6px" onerror="this.remove()">` : ''}
          <b>${esc(m.name)}</b>
          ${sideBadge}
          ${installed ? '<span class="pill st-running" style="font-size:10px;padding:1px 8px">已安装</span>' : ''}
          ${inQueue ? '<span class="pill st-starting" style="font-size:10px;padding:1px 8px">队列中</span>' : ''}
          <span class="muted small">by ${esc(m.author)}</span>
          <div class="muted small">${esc(m.summary)}</div></td>
        <td class="muted">${m.downloads.toLocaleString()}</td>
        <td>
          <button class="btn small" onclick="toggleModVersions(${i})">版本 ▾</button>
          <button class="btn small primary" onclick="addLatestToQueue(${i})" ${inQueue || blocked ? 'disabled' : ''} ${dis}>+ 队列</button>
        </td></tr>` +
        (expanded ? `<tr><td colspan="3" style="background:var(--row-hover)"><div id="md-versions" class="muted small">加载版本中…</div></td></tr>` : '');
    }).join('') + '</tbody></table>';
  if (modDL.expanded !== null) loadVersionsInline(modDL.expanded);
}
async function toggleModVersions(i) {
  modDL.expanded = modDL.expanded === i ? null : i;
  renderModResults();
}
async function loadVersionsInline(i) {
  const m = modDL.results[i];
  try {
    const d = await api(`/moddb/versions?source=${modDL.source}&project=${encodeURIComponent(m.id)}&game=${encodeURIComponent(modDL.game)}&loader=${encodeURIComponent(modDL.loader)}`);
    modDL.versions[i] = d.versions;
    const el = $('#md-versions');
    if (!el || modDL.expanded !== i) return;
    if (!d.versions.length) { el.innerHTML = '<span>没有匹配当前版本/加载器的文件，可放宽过滤条件</span>'; return; }
    el.innerHTML = d.versions.slice(0, 8).map((v, vi) => {
      const clientOnly = v.environment === 'client_only';
      return `<div class="row between" style="padding:6px 0;border-bottom:1px solid var(--border)">
      <div><span class="mono small"><b>${esc(v.name)}</b> · ${esc(v.filename)}</span> <span class="muted small">${esc((v.date || '').slice(0, 10))}</span>
        ${clientOnly ? '<span class="pill st-warn" style="font-size:10px;padding:1px 8px">纯客户端</span>' : ''}</div>
      <button class="btn small primary" onclick="addToQueue(${i},${vi})" ${clientOnly ? 'disabled title="纯客户端版本，服务器无需下载"' : ''}>+ 队列</button></div>`;
    }).join('');
  } catch (e) {
    const el = $('#md-versions');
    if (el) el.innerHTML = `<span>${esc(e.message)}</span>`;
  }
}
async function addLatestToQueue(i) {
  const m = modDL.results[i];
  if (!m) return;
  try {
    if (!modDL.versions[i]) {
      const d = await api(`/moddb/versions?source=${modDL.source}&project=${encodeURIComponent(m.id)}&game=${encodeURIComponent(modDL.game)}&loader=${encodeURIComponent(modDL.loader)}`);
      modDL.versions[i] = d.versions;
    }
    const v = (modDL.versions[i] || [])[0];
    if (!v) return toast('没有匹配当前版本/加载器的文件', false);
    await addToQueue(i, modDL.versions[i].indexOf(v));
  } catch (e) { toast(e.message, false); }
}
async function addToQueue(ri, vi, dep) {
  const item = (modDL.versions[ri] || [])[vi];
  const m = modDL.results[ri];
  if (!item || !m) return;
  const side = modDL.sides[m.id];
  if (side && side.server_side === 'unsupported') {
    return toast('纯客户端模组（标注不支持服务端），服务器无需下载', false);
  }
  if (item.environment === 'client_only') {
    return toast('该版本为纯客户端版本，服务器无需下载', false);
  }
  const pushed = await queuePush({
    source: modDL.source,
    projectId: m.id,
    projectName: m.name,
    name: item.name,
    filename: item.filename,
    url: item.url,
    dependencies: item.dependencies || [],
    dep: !!dep,
  }, !!dep);
  if (pushed && !dep) toast(`已加入队列：${item.name}`);
  renderModResults();
  renderModQueue();
}
async function queuePush(item, dep) {
  if (modDL.queue.some(q => q.filename === item.filename)) return false;
  modDL.queue.push(item);
  renderModQueue();
  // 自动解析前置依赖（仅 Modrinth 提供依赖信息）
  if (item.source !== 'modrinth' || !item.dependencies) return true;
  for (const d of item.dependencies) {
    if (d.dependency_type !== 'required') continue;
    if (modDL.queue.some(q => q.projectId === d.project_id)) continue;
    if (modDL.installed.has(d.project_id)) continue;
    try {
      const dv = await api(`/moddb/versions?source=modrinth&project=${encodeURIComponent(d.project_id)}&game=${encodeURIComponent(modDL.game)}&loader=${encodeURIComponent(modDL.loader)}`);
      const v = dv.versions.find(x => x.environment !== 'client_only');
      if (!v) continue;
      await queuePush({
        source: 'modrinth',
        projectId: d.project_id,
        projectName: v.filename,
        name: v.name,
        filename: v.filename,
        url: v.url,
        dependencies: v.dependencies || [],
        dep: true,
      }, true);
      toast(`已自动添加前置依赖：${v.filename}`);
    } catch {}
  }
  return true;
}
function removeFromQueue(i) {
  modDL.queue.splice(i, 1);
  renderModResults();
  renderModQueue();
}
function clearModQueue() {
  modDL.queue = [];
  renderModResults();
  renderModQueue();
}
function renderModQueue() {
  const el = $('#md-queue');
  if (!el || !modDL) return;
  if (!modDL.queue.length) { el.innerHTML = ''; return; }
  const badge = q => {
    if (q.status === 'done') return '<span class="pill st-running">✓ 完成</span>';
    if (q.status === 'error') return `<span class="pill st-stopped" title="${esc(q.err || '')}">失败</span>`;
    if (q.status === 'downloading') return '<span class="pill st-starting">下载中…</span>';
    return '<span class="pill st-stopped">排队中</span>';
  };
  el.innerHTML = `<div class="card" style="padding:10px 14px">
    <div class="row between" style="margin-bottom:6px">
      <b>下载队列（${modDL.queue.length}）</b>
      <div class="row">
        <button class="btn small danger" onclick="clearModQueue()">清空</button>
        <button class="btn small primary" id="md-dl" onclick="downloadQueue()" ${modDL.queue.some(q => q.status === 'downloading') ? 'disabled' : ''}>⬇ 下载全部</button>
      </div>
    </div>
    ${modDL.queue.map((q, i) => `<div class="row between" style="padding:5px 0;border-bottom:1px solid var(--border)">
      <span class="small">${q.dep ? '<span class="tag t-basic">前置</span>' : ''}<b>${esc(q.name || q.filename)}</b>
        <span class="muted small mono">${esc(q.filename)}</span> ${badge(q)}</span>
      ${q.status ? '<span></span>' : `<button class="btn small danger" onclick="removeFromQueue(${i})">移除</button>`}
    </div>`).join('')}
  </div>`;
}
async function downloadQueue() {
  if (!modDL) return;
  const items = modDL.queue.filter(q => q.status !== 'done');
  if (!items.length) return;
  for (const q of items) {
    q.status = 'downloading';
    renderModQueue();
    try {
      const r = await api(`/instances/${modDL.id}/mods/download`, {
        method: 'POST',
        body: { url: q.url, filename: q.filename },
      });
      q.status = 'done';
      q.size = r.size;
    } catch (e) {
      q.status = 'error';
      q.err = e.message;
    }
    renderModQueue();
  }
  const ok = modDL.queue.filter(q => q.status === 'done').length;
  const bad = modDL.queue.filter(q => q.status === 'error').length;
  toast(`批量下载完成：成功 ${ok}，失败 ${bad}`, bad === 0);
  // 实例运行中才提示重启
  try {
    const s = await api(`/instances/${modDL.id}/status`);
    if (s.status !== 'stopped') {
      const rb = $('#md-restart');
      if (rb) rb.style.display = '';
    }
  } catch {}
}
async function doUploadMod(id) {
  const files = $('#mod-file').files;
  if (!files.length) return toast('请选择文件', false);
  const fd = new FormData();
  for (const f of files) fd.append('file', f);
  try {
    await api(`/instances/${id}/mods/upload`, { method: 'POST', body: fd });
    closeModal(); toast('上传成功'); refresh();
  } catch (e) { toast(e.message, false); }
}

/* ---------------- 文件管理 ---------------- */
async function renderTabFiles(id, el, t) {
  curPath = '';
  el.innerHTML = `
    <div class="row between"><h2>文件管理</h2>
      <div class="row"><button class="btn" onclick="filesMkdir('${id}')">新建文件夹</button><button class="btn" onclick="filesUpload('${id}')">上传文件</button></div></div>
    <div id="files-crumb" class="crumb"></div>
    <div id="files-body"><div class="empty">加载中…</div></div>`;
  await loadFiles(id, t, '');
}

async function loadFiles(id, t, path) {
  curPath = path;
  try {
    const { entries } = await api(`/instances/${id}/files?path=${encodeURIComponent(path)}`);
    if (t !== routeToken) return;
    const parts = path ? path.split('/') : [];
    let crumbs = `<a onclick="loadFiles('${id}',${t},'')">根目录</a>`;
    let acc = '';
    for (const p of parts) {
      acc = acc ? acc + '/' + p : p;
      crumbs += ` / <a onclick="loadFiles('${id}',${t},'${esc(acc)}')">${esc(p)}</a>`;
    }
    $('#files-crumb').innerHTML = crumbs;
    const full = f => (path ? path + '/' : '') + f.name;
    $('#files-body').innerHTML = `<table class="table">
      <thead><tr><th>名称</th><th>大小</th><th>修改时间</th><th>操作</th></tr></thead>
      <tbody>${entries.map(f => `<tr>
        <td>${f.dir ? '📁' : '📄'} <a onclick="${f.dir
          ? `loadFiles('${id}',${t},'${esc(full(f))}')`
          : `editFile('${id}','${esc(full(f))}')`}">${esc(f.name)}</a></td>
        <td class="muted">${f.dir ? '-' : fmtSize(f.size)}</td>
        <td class="muted">${esc(f.modified)}</td>
        <td>
          <button class="btn small" onclick="renameFile('${id}','${esc(full(f))}','${esc(f.name)}')">重命名</button>
          <button class="btn small danger" onclick="deleteFile('${id}','${esc(full(f))}')">删除</button>
        </td></tr>`).join('') || '<tr><td colspan="4" class="muted">空目录</td></tr>'}</tbody></table>`;
  } catch (e) {
    $('#files-body').innerHTML = `<div class="empty">${esc(e.message)}</div>`;
  }
}

async function editFile(id, path) {
  try {
    const f = await api(`/instances/${id}/files/content?path=${encodeURIComponent(path)}`);
    if (f.binary) {
      showModal(`<h2>无法编辑</h2>
        <p class="muted" style="margin:10px 0 16px">「${esc(path)}」是二进制文件（${fmtSize(f.size)}），请在文件管理中上传替换，或使用外部工具编辑。</p>
        <div class="row right"><button class="btn" onclick="closeModal()">知道了</button></div>`);
      return;
    }
    showModal(`<h2>编辑 ${esc(path)}</h2>
      <textarea id="editor" class="editor" spellcheck="false">${esc(f.content)}</textarea>
      <div class="row right"><button class="btn ghost" onclick="closeModal()">取消</button><button class="btn primary" onclick="saveFile('${id}','${esc(path)}')">保存</button></div>`, 'wide');
  } catch (e) { toast(e.message, false); }
}
async function saveFile(id, path) {
  try {
    await api(`/instances/${id}/files/content`, { method: 'PUT', body: { path, content: $('#editor').value } });
    closeModal(); toast('已保存');
  } catch (e) { toast(e.message, false); }
}
async function filesMkdir(id) {
  const name = prompt('文件夹名称:');
  if (!name) return;
  try {
    await api(`/instances/${id}/files/mkdir`, { method: 'POST', body: { path: curPath ? curPath + '/' + name : name } });
    toast('已创建'); loadFiles(id, routeToken, curPath);
  } catch (e) { toast(e.message, false); }
}
async function renameFile(id, path, oldName) {
  const nn = prompt('新名称:', oldName);
  if (!nn || nn === oldName) return;
  const parent = path.includes('/') ? path.slice(0, path.lastIndexOf('/')) : '';
  const to = parent ? parent + '/' + nn : nn;
  try {
    await api(`/instances/${id}/files/rename`, { method: 'POST', body: { from: path, to } });
    toast('已重命名'); loadFiles(id, routeToken, curPath);
  } catch (e) { toast(e.message, false); }
}
async function deleteFile(id, path) {
  if (!confirm(`确定删除 ${path}？\n如果是目录将被整个删除！`)) return;
  try {
    await api(`/instances/${id}/files/delete`, { method: 'POST', body: { path } });
    toast('已删除'); loadFiles(id, routeToken, curPath);
  } catch (e) { toast(e.message, false); }
}
function filesUpload(id) {
  showModal(`<h2>上传文件到当前目录</h2><p class="muted small" style="margin:8px 0">${esc(curPath || '根目录')}</p>
    <input type="file" id="up-file" multiple style="margin:10px 0 16px">
    <div class="row right"><button class="btn ghost" onclick="closeModal()">取消</button><button class="btn primary" onclick="doFilesUpload('${id}')">上传</button></div>`);
}
async function doFilesUpload(id) {
  const files = $('#up-file').files;
  if (!files.length) return toast('请选择文件', false);
  const fd = new FormData();
  for (const f of files) fd.append('file', f);
  try {
    await api(`/instances/${id}/files/upload?path=${encodeURIComponent(curPath)}`, { method: 'POST', body: fd });
    closeModal(); toast('上传成功'); loadFiles(id, routeToken, curPath);
  } catch (e) { toast(e.message, false); }
}

/* ---------------- server.properties ---------------- */
const PROP_CATS = { all: '全部', basic: '基本', player: '玩家', network: '网络', world: '世界', perf: '性能', other: '其他' };
let propCat = 'all';

/* 原版 server.properties 中文说明目录；未收录的键按当前值自动推断类型 */
const PROP_META = {
  // ---- 基本 ----
  'motd': { cat: 'basic', desc: '服务器简介，显示在多人游戏服务器列表中', type: 'text', placeholder: '例如：欢迎来到我的服务器！' },
  'server-port': { cat: 'basic', desc: '服务器监听端口（默认 25565），修改后需重启生效', type: 'number', min: 1, max: 65535 },
  'server-ip': { cat: 'basic', desc: '绑定的 IP 地址，多网卡时指定；留空表示监听所有网卡', type: 'text', placeholder: '留空 = 所有网卡' },
  'gamemode': { cat: 'basic', desc: '新玩家进入时的默认游戏模式', type: 'select', options: [['survival', 'survival · 生存'], ['creative', 'creative · 创造'], ['adventure', 'adventure · 冒险'], ['spectator', 'spectator · 旁观']] },
  'difficulty': { cat: 'basic', desc: '游戏难度', type: 'select', options: [['peaceful', 'peaceful · 和平'], ['easy', 'easy · 简单'], ['normal', 'normal · 普通'], ['hard', 'hard · 困难']] },
  'hardcore': { cat: 'basic', desc: '极限模式：死亡后无法重生，并锁定困难难度', type: 'bool' },
  'pvp': { cat: 'basic', desc: '是否允许玩家之间的攻击对战', type: 'bool' },
  'allow-nether': { cat: 'basic', desc: '是否允许进入下界', type: 'bool' },
  'generate-structures': { cat: 'basic', desc: '是否生成村庄、遗迹等世界结构', type: 'bool' },
  'level-name': { cat: 'basic', desc: '世界名称（存档文件夹名）', type: 'text' },
  'level-seed': { cat: 'basic', desc: '世界生成种子，留空则随机', type: 'text', placeholder: '留空 = 随机' },
  'level-type': { cat: 'basic', desc: '世界生成类型：normal 常规 / flat 超平坦 / large_biomes 巨型生物群系 / amplified 放大化', type: 'text', placeholder: 'minecraft:normal' },
  // ---- 玩家 ----
  'max-players': { cat: 'player', desc: '服务器最大同时在线人数', type: 'number', min: 0, max: 2147483647 },
  'white-list': { cat: 'player', desc: '是否启用白名单（仅名单内玩家可进入）', type: 'bool' },
  'enforce-whitelist': { cat: 'player', desc: '白名单更新后自动踢出不在名单中的玩家', type: 'bool' },
  'op-permission-level': { cat: 'player', desc: 'OP 权限等级：0 普通玩家 ~ 4 服务器所有者', type: 'number', min: 0, max: 4 },
  'allow-flight': { cat: 'player', desc: '是否允许飞行（关闭时检测到飞行动作会踢出，模组飞行请开启）', type: 'bool' },
  'player-idle-timeout': { cat: 'player', desc: '玩家挂机自动踢出时间（分钟），0 表示不踢出', type: 'number', min: 0 },
  'force-gamemode': { cat: 'player', desc: '玩家加入时强制切换为服务器默认游戏模式', type: 'bool' },
  'spawn-protection': { cat: 'player', desc: '出生点保护半径（方块），仅 OP 可在此建造；0 表示仅保护出生点方块', type: 'number', min: 0 },
  'accept-transfers': { cat: 'player', desc: '是否允许客户端转移到其他服务器登录（1.20.5+）', type: 'bool' },
  // ---- 网络 ----
  'online-mode': { cat: 'network', desc: '正版账号验证；关闭后允许离线玩家进入，请自行评估风险', type: 'bool' },
  'enforce-secure-profile': { cat: 'network', desc: '强制玩家使用 Mojang 签名公钥（正版聊天签名）', type: 'bool' },
  'prevent-proxy-connections': { cat: 'network', desc: '拒绝通过代理 / VPN 连入', type: 'bool' },
  'enable-status': { cat: 'network', desc: '是否在服务器列表中响应状态查询（在线人数、MOTD 等）', type: 'bool' },
  'enable-rcon': { cat: 'network', desc: '是否启用 Rcon 远程控制台', type: 'bool' },
  'rcon.port': { cat: 'network', desc: 'Rcon 远程控制台端口', type: 'number', min: 1, max: 65535 },
  'rcon.password': { cat: 'network', desc: 'Rcon 远程控制台密码，启用 Rcon 时务必使用强密码', type: 'password' },
  'broadcast-rcon-to-ops': { cat: 'network', desc: '将 Rcon 执行的命令广播给在线 OP', type: 'bool' },
  'enable-query': { cat: 'network', desc: '是否启用 GameSpy4 查询协议', type: 'bool' },
  'query.port': { cat: 'network', desc: 'GameSpy4 查询协议端口', type: 'number', min: 1, max: 65535 },
  'network-compression-threshold': { cat: 'network', desc: '网络数据包压缩阈值（字节）；-1 禁用，默认 256', type: 'number', min: -1, max: 256 },
  'rate-limit': { cat: 'network', desc: '入站数据包速率限制（字节/秒），0 表示禁用', type: 'number', min: 0 },
  // ---- 世界 ----
  'view-distance': { cat: 'world', desc: '视距（区块，3-32），决定玩家加载的区块范围，影响带宽与内存', type: 'number', min: 3, max: 32 },
  'simulation-distance': { cat: 'world', desc: '模拟距离（区块，3-32），决定实体活动、作物生长等的模拟范围', type: 'number', min: 3, max: 32 },
  'max-world-size': { cat: 'world', desc: '世界半径上限（方块，最大 29999984）', type: 'number', min: 1, max: 29999984 },
  'spawn-animals': { cat: 'world', desc: '是否生成动物', type: 'bool' },
  'spawn-monsters': { cat: 'world', desc: '是否生成敌对怪物', type: 'bool' },
  'spawn-npcs': { cat: 'world', desc: '是否生成 NPC（村民等）', type: 'bool' },
  'generator-settings': { cat: 'world', desc: '自定义世界生成器设置（JSON，配合 level-type 使用）', type: 'text', placeholder: '{"...": "..."}' },
  'initial-enabled-packs': { cat: 'world', desc: '世界首次创建时默认启用的数据包（逗号分隔）', type: 'text' },
  'initial-disabled-packs': { cat: 'world', desc: '世界首次创建时默认禁用的数据包（逗号分隔）', type: 'text' },
  'max-chained-neighbor-updates': { cat: 'world', desc: '单刻连锁方块更新数量上限，-1 表示不限制', type: 'number', min: -1 },
  // ---- 性能 ----
  'use-native-transport': { cat: 'perf', desc: '使用系统原生网络传输优化（Linux Epoll 等），建议保持开启', type: 'bool' },
  'sync-chunk-writes': { cat: 'perf', desc: '强制区块同步写入磁盘（崩溃时数据更安全，性能略降）', type: 'bool' },
  'max-tick-time': { cat: 'perf', desc: '单刻最大耗时（毫秒），超时触发看门狗崩溃保护；-1 禁用', type: 'number', min: -1 },
  'entity-broadcast-range-percentage': { cat: 'perf', desc: '实体状态同步范围百分比（0-500，默认 100），调低可减轻网络压力', type: 'number', min: 0, max: 500 },
  'pause-when-empty-seconds': { cat: 'perf', desc: '服务器无人时暂停内部刻计时（秒），-1 表示禁用', type: 'number', min: -1 },
  'function-level-limit': { cat: 'perf', desc: '数据包函数单次运行的最大指令数', type: 'number', min: 0 },
  // ---- 其他 ----
  'resource-pack': { cat: 'other', desc: '服务器资源包下载链接（URL），玩家进服会提示安装', type: 'text', placeholder: 'https://...' },
  'resource-pack-sha1': { cat: 'other', desc: '资源包 SHA1 校验值（40 位十六进制，可选，用于校验完整性）', type: 'text', placeholder: '40 位十六进制' },
  'resource-pack-prompt': { cat: 'other', desc: '玩家拒绝资源包时显示的自定义提示信息', type: 'text' },
  'require-resource-pack': { cat: 'other', desc: '是否强制玩家接受资源包，拒绝则无法进入', type: 'bool' },
  'enable-command-block': { cat: 'other', desc: '是否启用命令方块', type: 'bool' },
  'enable-jmx-monitoring': { cat: 'other', desc: '是否启用 JMX 监控接口（MBean，供运维工具使用）', type: 'bool' },
  'broadcast-console-to-ops': { cat: 'other', desc: '将服务器控制台执行的命令广播给在线 OP', type: 'bool' },
  'bug-report-link': { cat: 'other', desc: '自定义漏洞反馈链接（1.20.3+）', type: 'text' },
  'text-filtering-config': { cat: 'other', desc: '聊天文本过滤配置（JSON）', type: 'text' },
  'text-filtering-version': { cat: 'other', desc: '聊天文本过滤功能版本号', type: 'number', min: 0 },
  'log-ips': { cat: 'other', desc: '是否在日志中记录玩家 IP 地址', type: 'bool' },
};

async function renderTabProps(id, el, t) {
  el.innerHTML = `
    <div class="row between"><h2>server.properties</h2>
      <div class="row">
        <input id="prop-search" placeholder="搜索配置项 / 说明…" style="width:230px" oninput="renderPropsTable()">
        <button class="btn primary" onclick="saveProps('${id}')">保存全部</button>
      </div></div>
    <p class="muted small">共 <b id="prop-count"></b> 项配置，按类型渲染输入框；未收录的自定义键会根据当前值自动推断类型。运行中的服务器修改后需重启实例才能生效。</p>
    <div class="chips" id="prop-chips"></div>
    <div id="props-body"><div class="empty">加载中…</div></div>`;
  try {
    const p = await api(`/instances/${id}/properties`);
    if (t !== routeToken) return;
    propEntries = p.entries;
    if (!p.exists) {
      $('#props-body').innerHTML = '<div class="empty">server.properties 不存在（首次启动服务器后自动生成）</div>';
      return;
    }
    renderPropChips();
    renderPropsTable();
  } catch (e) {
    $('#props-body').innerHTML = `<div class="empty">${esc(e.message)}</div>`;
  }
}

function renderPropChips() {
  $('#prop-chips').innerHTML = Object.entries(PROP_CATS)
    .map(([k, t]) => `<button class="chip ${propCat === k ? 'active' : ''}" onclick="setPropCat('${k}')">${t}</button>`).join('');
}
function setPropCat(c) { propCat = c; renderPropChips(); renderPropsTable(); }

function propMetaFor(key, value) {
  if (PROP_META[key]) return PROP_META[key];
  if (value === 'true' || value === 'false') return { cat: 'other', desc: '自定义配置项（布尔值）', type: 'bool' };
  if (/^-?\d+$/.test(String(value))) return { cat: 'other', desc: '自定义配置项（整数）', type: 'number' };
  return { cat: 'other', desc: '自定义配置项（文本）', type: 'text' };
}

function propControl(e, i) {
  const meta = propMetaFor(e.key, e.value);
  const bind = `propEntries[${i}].value`;
  if (meta.type === 'bool') {
    return `<div class="bool-cell">
      <label class="switch"><input type="checkbox" ${e.value === 'true' ? 'checked' : ''} onchange="${bind}=this.checked?'true':'false'"><span class="knob"></span></label>
      <span class="mono muted small">${esc(e.value)}</span></div>`;
  }
  if (meta.type === 'number') {
    const min = meta.min !== undefined ? ` min="${meta.min}"` : '';
    const max = meta.max !== undefined ? ` max="${meta.max}"` : '';
    return `<input type="number" class="prop-input"${min}${max} value="${esc(e.value)}" oninput="${bind}=this.value">`;
  }
  if (meta.type === 'select') {
    const has = meta.options.some(([v]) => v === e.value);
    const extra = has || e.value === '' ? '' : `<option value="${esc(e.value)}" selected>${esc(e.value)}（当前值）</option>`;
    const opts = meta.options
      .map(([v, t]) => `<option value="${esc(v)}" ${e.value === v ? 'selected' : ''}>${esc(t)}</option>`).join('');
    return `<select class="prop-input" onchange="${bind}=this.value">${extra}${opts}</select>`;
  }
  if (meta.type === 'password') {
    return `<input type="password" class="prop-input" value="${esc(e.value)}" oninput="${bind}=this.value" autocomplete="new-password">`;
  }
  const ph = meta.placeholder ? ` placeholder="${esc(meta.placeholder)}"` : '';
  return `<input type="text" class="prop-input" value="${esc(e.value)}" oninput="${bind}=this.value"${ph}>`;
}

function renderPropsTable() {
  const q = ($('#prop-search')?.value || '').toLowerCase();
  const rows = propEntries
    .map((e, i) => ({ e, i }))
    .filter(({ e }) => e.key && (propCat === 'all' || propMetaFor(e.key, e.value).cat === propCat))
    .filter(({ e }) => {
      if (!q) return true;
      const meta = propMetaFor(e.key, e.value);
      return e.key.toLowerCase().includes(q) || String(e.value).toLowerCase().includes(q) || meta.desc.toLowerCase().includes(q);
    });
  const cnt = $('#prop-count');
  if (cnt) cnt.textContent = propEntries.filter(x => x.key).length;
  $('#props-body').innerHTML = rows.length ? `<table class="table props">
    <thead><tr><th>配置项</th><th>值</th></tr></thead>
    <tbody>${rows.map(({ e, i }) => {
      const meta = propMetaFor(e.key, e.value);
      return `<tr>
        <td class="mono"><span class="tag t-${meta.cat}">${PROP_CATS[meta.cat]}</span>${esc(e.key)}
          <div class="muted small">${esc(meta.desc)}</div></td>
        <td>${propControl(e, i)}</td></tr>`;
    }).join('')}</tbody></table>`
    : '<div class="empty">没有匹配的配置项</div>';
}
async function saveProps(id) {
  try {
    await api(`/instances/${id}/properties`, { method: 'PUT', body: { entries: propEntries } });
    toast('已保存');
  } catch (e) { toast(e.message, false); }
}

/* ---------------- 实例设置 ---------------- */
async function renderTabSettings(id, el) {
  const s = await api(`/instances/${id}`);
  const { jars } = await api(`/instances/${id}/jars`);
  el.innerHTML = `<h2>实例设置</h2>
    <div class="form card">
      <label class="full">实例名称<input id="f-name" value="${esc(s.name)}"></label>
      <label>主程序 JAR（相对实例目录）
        <div class="row">
          <input id="f-jar" value="${esc(s.jar || '')}" list="jar-list" placeholder="例如 server.jar">
          <datalist id="jar-list">${jars.map(j => `<option value="${esc(j)}">`).join('')}</datalist>
          <button class="btn ghost" onclick="detectJar('${id}')">自动检测</button>
        </div>
        <div class="muted small">普通服务端填写可执行的服务器 JAR（如 server.jar）。Forge / NeoForge 1.17+ 服务包：此处留空，改在「JVM / 启动参数」中填 @user_jvm_args.txt 与 @libraries/.../win_args.txt（导入整合包时已自动识别）。</div>
      </label>
      <label>Java 路径
        <div class="row">
          <input id="f-java" value="${esc(s.java_path || '')}" placeholder="java（使用 PATH 中的 java）">
          <button class="btn ghost" onclick="detectJava()">检测</button>
          <button class="btn ghost" onclick="rescanJavas()">扫描本机 Java</button>
        </div>
        <select id="java-picker" style="margin-top:6px" onchange="if(this.value){document.getElementById('f-java').value=this.value;detectJava();}">
          <option value="">加载缓存…</option>
        </select>
        <div class="muted small">Java 17+ 运行 1.18~1.20.4，Java 21+ 运行 1.20.5+，更新版本需要更高 Java；扫描会遍历 Program Files、Prism Launcher、.jdks 等位置并持久化结果。从列表选择后记得点「保存设置」。</div>
        <div id="java-hint" class="muted small mono"></div>
      </label>
      <label>最小内存 (MB)<input id="f-min" type="number" min="512" step="512" value="${s.min_ram_mb}">
        <div class="muted small">JVM 初始堆大小（-Xms），一般与最大内存设为一致</div></label>
      <label>最大内存 (MB)<input id="f-max" type="number" min="512" step="512" value="${s.max_ram_mb}">
        <div class="muted small">JVM 最大堆大小（-Xmx）：原版服 2048~4096，模组服建议 6144 以上</div></label>
      <label class="full">JVM / 启动参数（空格分隔，支持 @argfile）<input id="f-jvm" value="${esc(s.jvm_args || '')}" placeholder="-XX:+UseG1GC -XX:ParallelGCThreads=4 -Dfile.encoding=UTF-8">
        <div class="muted small">追加在 -Xms/-Xmx 之后的 JVM 参数；主程序 JAR 为空时，这里就是完整的启动参数（支持 @user_jvm_args.txt 等 argfile 写法）。</div></label>
      <label class="check"><input type="checkbox" id="f-auto" ${s.auto_restart ? 'checked' : ''}> 进程异常退出时自动重启（5 秒后）</label>
      <label class="check"><input type="checkbox" id="f-autoboot" ${s.auto_start_on_boot ? 'checked' : ''}> 面板启动时自动运行此实例（多个实例将间隔 5 秒依次拉起）</label>
      <div class="row right"><button class="btn primary" onclick="saveInstance('${id}')">保存设置</button></div>
    </div>`;
  loadCachedJavas();
}
async function loadCachedJavas() {
  const sel = $('#java-picker');
  if (!sel) return;
  try {
    const d = await api('/javas');
    fillJavaPicker(sel, d.javas || [], !!d.scanned);
  } catch (e) {
    sel.innerHTML = `<option value="">加载失败：${esc(e.message)}</option>`;
  }
}
function fillJavaPicker(sel, javas, scanned) {
  if (!javas.length) {
    sel.innerHTML = `<option value="">${scanned ? '未扫描到任何 Java，请手动填写路径' : '尚未扫描：点击「扫描本机 Java」检测'}</option>`;
    return;
  }
  sel.innerHTML = `<option value="">— 本机 ${javas.length} 个 Java（${scanned ? '上次扫描结果' : ''}，点击选择）—</option>` +
    javas.map(j => `<option value="${esc(j.path)}">Java ${j.major} · ${esc(j.version)}（${esc(j.source)}）</option>`).join('');
}
async function rescanJavas() {
  const sel = $('#java-picker');
  if (!sel) return;
  sel.innerHTML = '<option value="">正在扫描本机 Java…</option>';
  try {
    const d = await api('/javas/scan', { method: 'POST' });
    fillJavaPicker(sel, d.javas || [], true);
    toast(`扫描完成，共发现 ${d.javas.length} 个 Java`);
  } catch (e) {
    sel.innerHTML = `<option value="">扫描失败：${esc(e.message)}</option>`;
  }
}
async function detectJar(id) {
  try {
    const { jars } = await api(`/instances/${id}/jars`);
    if (!jars.length) return toast('未找到任何 JAR 文件', false);
    $('#f-jar').value = jars[0];
    toast('已填入: ' + jars[0]);
  } catch (e) { toast(e.message, false); }
}
async function detectJava() {
  try {
    const r = await api(`/java?path=${encodeURIComponent($('#f-java').value.trim())}`);
    $('#java-hint').textContent = r.version || '未获取到版本信息';
  } catch (e) { $('#java-hint').textContent = e.message; }
}
async function saveInstance(id) {
  try {
    await api(`/instances/${id}`, {
      method: 'PATCH',
      body: {
        name: $('#f-name').value,
        jar: $('#f-jar').value,
        java_path: $('#f-java').value,
        min_ram_mb: +$('#f-min').value || 1024,
        max_ram_mb: +$('#f-max').value || 4096,
        jvm_args: $('#f-jvm').value,
        auto_restart: $('#f-auto').checked,
        auto_start_on_boot: $('#f-autoboot').checked,
      },
    });
    toast('已保存');
  } catch (e) { toast(e.message, false); }
}

/* ---------------- 面板设置 ---------------- */
async function renderPanelSettings() {
  const t = ++routeToken;
  const c = await api('/settings');
  if (t !== routeToken) return;
  $('#main').innerHTML = `<h1>面板设置</h1>
    <div class="form card">
      <label>监听地址<input id="ps-listen" value="${esc(c.listen)}" placeholder="127.0.0.1:8080">
        <div class="muted small">格式 IP:端口；127.0.0.1 仅本机访问，0.0.0.0 对局域网开放。修改后需重启面板生效。</div></label>
      <label>访问令牌<input id="ps-token" value="${esc(c.token)}" placeholder="留空则无需鉴权">
        <div class="muted small">设置后所有 API 与 WebSocket 控制台都需要令牌，保存后立即生效；对外暴露时建议设置。</div></label>
      <label>CurseForge API Key<input id="ps-cfkey" value="${esc(c.curseforge_api_key || '')}" placeholder="留空则模组下载仅支持 Modrinth">
        <div class="muted small">用于「模组下载」中 CurseForge 的搜索与文件列表；在 console.curseforge.com 可免费创建。</div></label>
      <label class="full">数据目录<input id="ps-dir" value="${esc(c.data_dir)}">
        <div class="muted small">实例存放的根目录（相对路径基于面板工作目录），重启面板后生效。</div></label>
      <div class="row right"><button class="btn primary" onclick="savePanelSettings()">保存</button></div>
    </div>`;
}
async function savePanelSettings() {
  try {
    await api('/settings', {
      method: 'PUT',
      body: {
        listen: $('#ps-listen').value,
        token: $('#ps-token').value,
        data_dir: $('#ps-dir').value,
        curseforge_api_key: $('#ps-cfkey').value,
      },
    });
    toast('已保存');
  } catch (e) { toast(e.message, false); }
}

/* ---------------- 启动 ---------------- */
route();

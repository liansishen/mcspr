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
  $('#modal-root').innerHTML = `<div class="modal-backdrop"><div class="modal ${cls}">${html}</div></div>`;
  // 只在按下与抬起都落在遮罩上时才关闭：避免在输入框/文本上拖动选择、
  // 鼠标移出弹窗范围后松开，被误判为点击遮罩而关闭弹窗
  const bd = $('#modal-root .modal-backdrop');
  bd.addEventListener('mousedown', e => { bd._downOnBackdrop = e.target === bd; });
  bd.addEventListener('mouseup', e => {
    if (bd._downOnBackdrop && e.target === bd) closeModal();
    bd._downOnBackdrop = false;
  });
}
let modsTabReload = null;
function closeModal() {
  const wasModDownload = !!$('#md-results');
  $('#modal-root').innerHTML = '';
  // 下载模组弹窗关闭后，刷新背后的模组列表
  if (wasModDownload && modsTabReload) modsTabReload();
}

/* ---------------- 自绘对话框（替代 alert / confirm / prompt） ----------------
   独立于主弹窗层，可叠加在任意弹窗之上；点遮罩不关闭，必须点按钮，杜绝误触丢数据 */
function dlgLayer() {
  let root = document.getElementById('dialog-root');
  if (!root) {
    root = document.createElement('div');
    root.id = 'dialog-root';
    document.body.appendChild(root);
  }
  return root;
}
function dlgShow(html, onOpen) {
  const root = dlgLayer();
  root.innerHTML = `<div class="modal-backdrop" style="z-index:200"><div class="modal" style="max-width:400px">${html}</div></div>`;
  const dlg = new Promise(resolve => {
    const done = v => { root.innerHTML = ''; resolve(v); };
    root.querySelectorAll('[data-dlg]').forEach(btn =>
      btn.addEventListener('click', () => done(btn.getAttribute('data-dlg'))));
    if (onOpen) onOpen(root, done);
  });
  return dlg;
}
function appAlert(msg, title = '提示') {
  return dlgShow(`<h3 style="margin:0 0 10px">${esc(title)}</h3>
    <div style="white-space:pre-wrap">${esc(msg)}</div>
    <div class="row right" style="margin-top:14px"><button class="btn primary" data-dlg="ok">确定</button></div>`);
}
function appConfirm(msg, { title = '确认操作', okText = '确定', danger = false } = {}) {
  return dlgShow(`<h3 style="margin:0 0 10px">${esc(title)}</h3>
    <div style="white-space:pre-wrap">${esc(msg)}</div>
    <div class="row right" style="margin-top:14px">
      <button class="btn ghost" data-dlg="no">取消</button>
      <button class="btn ${danger ? 'danger' : 'primary'}" data-dlg="yes">${esc(okText)}</button></div>`)
    .then(v => v === 'yes');
}
function appPrompt(msg, def = '', { title = '输入', placeholder = '' } = {}) {
  return new Promise(resolve => {
    const root = dlgLayer();
    root.innerHTML = `<div class="modal-backdrop" style="z-index:200"><div class="modal" style="max-width:400px"><h3 style="margin:0 0 10px">${esc(title)}</h3>
      <div style="margin-bottom:8px">${esc(msg)}</div>
      <input id="dlg-input" value="${esc(def)}" placeholder="${esc(placeholder)}" style="width:100%">
      <div class="row right" style="margin-top:14px">
        <button class="btn ghost" data-dlg="no">取消</button>
        <button class="btn primary" data-dlg="yes">确定</button></div></div></div>`;
    const input = root.querySelector('#dlg-input');
    input.focus();
    input.select();
    const finish = ok => { const val = ok ? input.value : null; root.innerHTML = ''; resolve(val); };
    root.querySelectorAll('[data-dlg]').forEach(btn =>
      btn.addEventListener('click', () => finish(btn.getAttribute('data-dlg') === 'yes')));
    input.addEventListener('keydown', e => {
      if (e.key === 'Enter') { e.preventDefault(); finish(true); }
      if (e.key === 'Escape') finish(false);
    });
  });
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
const THEME_ORDER = ['dark', 'light', 'mc', 'claude'];
const THEME_LABEL = { dark: '深色', light: '亮色', mc: 'MC 像素', claude: 'Claude' };

function currentTheme() {
  const de = document.documentElement;
  return de.classList.contains('light') ? 'light'
    : de.classList.contains('mc') ? 'mc'
    : de.classList.contains('claude') ? 'claude' : 'dark';
}
function applyTheme(t) {
  const de = document.documentElement;
  de.classList.toggle('light', t === 'light');
  de.classList.toggle('mc', t === 'mc');
  de.classList.toggle('claude', t === 'claude');
  try { localStorage.setItem('mcspr.theme', t); } catch {}
  const sel = $('#theme-select');
  if (sel) sel.value = t;
}
applyTheme(currentTheme());

function refresh() { route(); }

/* ---------------- 路由 ---------------- */
window.addEventListener('hashchange', route);

async function route() {
  const t = ++routeToken;
  clearTimers();
  const hash = location.hash.replace(/^#/, '') || '/dashboard';
  const parts = hash.split('/').filter(Boolean);
  document.body.classList.toggle('detail-view', parts[0] === 'instance' && !!parts[1]);
  $('#main').classList.toggle('detail-layout', parts[0] === 'instance' && !!parts[1]);
  $$('#nav a').forEach(a => {
    const nav = a.dataset.nav;
    const active = (nav === 'instances' && parts[0] === 'instance') || nav === parts[0];
    a.classList.toggle('active', active);
  });
  try {
    if (parts[0] === 'dashboard') await renderDashboard(t);
    else if (parts[0] === 'instances') await renderInstances(t);
    else if (parts[0] === 'instance' && parts[1]) await renderInstance(parts[1], parts[2] || 'console', t);
    else if (parts[0] === 'settings') await renderPanelSettings(t);
    else location.hash = '#/dashboard';
  } catch (e) {
    if (t === routeToken) $('#main').innerHTML = `<div class="empty">加载失败: ${esc(e.message)}</div>`;
  }
}

/* ---------------- 仪表盘 ---------------- */
async function renderDashboard(t = ++routeToken) {
  $('#main').innerHTML = `<h1>仪表盘</h1><div id="dash"><div class="empty">加载中…</div></div>`;
  const load = async () => {
    try {
      const s = await api('/stats');
      if (t !== routeToken) return;
      const cpu = (s.cpu_usage || 0);
      const memPct = s.mem_total ? (s.mem_used / s.mem_total * 100) : 0;
      const running = s.instances.filter(i => i.status === 'running').length;
      // 磁盘用量 + 实例空间排行
      const diskFree = s.disk ? s.disk.free : 0, diskTotal = s.disk ? s.disk.total : 0;
      const diskPct = diskTotal ? ((diskTotal - diskFree) / diskTotal * 100) : 0;
      const nameOf = Object.fromEntries(s.instances.map(i => [i.id, i.name]));
      const topSizes = Object.entries(s.sizes || {}).map(([id, bytes]) => ({ name: nameOf[id] || id, bytes }))
        .sort((a, b) => b.bytes - a.bytes).slice(0, 5);
      const sizeList = topSizes.length
        ? topSizes.map(t => `<div class="row between" style="padding:3px 0"><span class="muted small">${esc(t.name)}</span><span class="small">${fmtSize(t.bytes)}</span></div>`).join('')
        : '<div class="muted small">暂无实例</div>';
      $('#dash').innerHTML = `
        <div class="grid stats-grid">
          <div class="card"><h3>CPU 使用率</h3><div class="big">${cpu.toFixed(1)}%</div><div class="bar"><i style="width:${Math.min(cpu, 100)}%"></i></div></div>
          <div class="card"><h3>系统内存</h3><div class="big">${fmtSize(s.mem_used)} <span class="muted small">/ ${fmtSize(s.mem_total)}</span></div><div class="bar"><i style="width:${memPct.toFixed(1)}%"></i></div></div>
          <div class="card"><h3>实例</h3><div class="big">${running} <span class="muted small">/ ${s.instances.length} 运行中</span></div></div>
          <div class="card"><h3>磁盘可用</h3><div class="big">${fmtSize(diskFree)}</div><div class="bar"><i style="width:${diskPct.toFixed(1)}%;background:linear-gradient(90deg,var(--warn),var(--danger))"></i></div><div class="muted small">共 ${fmtSize(diskTotal)} · 已用 ${diskPct.toFixed(0)}%</div></div>
        </div>
        <div class="grid stats-grid">
          <div class="card" style="grid-column:1/-1"><h3>实例空间排行（目录 + 备份，5 分钟缓存）</h3>${sizeList}</div>
        </div>
        <div class="row right" style="margin-bottom:8px"><button class="btn small primary" onclick="batchStart()">▶ 启动选中</button><button class="btn small warn" onclick="batchStop()">■ 停止选中</button></div>
        <h2>实例概览</h2>
        <div class="grid cards-grid">${s.instances.map(i => {
          const p = (s.per_instance && s.per_instance[i.id]) || {};
          return `<div class="card inst-card">
            <div class="inst-head"><label class="check" style="margin:0"><input type="checkbox" class="dash-check" data-id="${i.id}"> <a href="#/instance/${i.id}/console">${esc(i.name)}</a></label>${statusPill(i.status)}</div>
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
  if (t === routeToken) every(3000, load);
}

/* ---------------- 实例列表 ---------------- */
async function renderInstances(t = ++routeToken) {
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
      $('#inst-list').innerHTML = instances.length ? `<div class="table-wrap"><table class="table">
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
            <button class="btn small" onclick="cloneInstance('${i.id}','${esc(i.name)}')">克隆</button>
            <button class="btn small danger" onclick="delInstance('${i.id}','${esc(i.name)}')">删除</button>
          </td></tr>`).join('')}</tbody></table></div>`
        : '<div class="empty">暂无实例。点击右上角「导入整合包」或「新建空白实例」开始。</div>';
    } catch (e) {
      if (t === routeToken) $('#inst-list').innerHTML = `<div class="empty">加载失败: ${esc(e.message)}</div>`;
    }
  };
  await load();
  if (t === routeToken) every(3000, load);
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
      <label>大版本<select id="ci-ver-major" onchange="updateVanillaMajor();updateVanillaMinor()"><option value="">加载中…</option></select></label>
      <label class="check" style="margin:4px 0"><input type="checkbox" id="ci-show-pre" onchange="updateVanillaMajor();updateVanillaMinor()"> 显示预览版 / 快照 / 预发布版本</label>
      <label>具体版本（自动下载）
        <select id="ci-version"><option value="">不下载，稍后手动导入或配置</option></select>
        <div class="muted small">从官方源下载对应服务端 jar 并配置为主程序（国内网络自动切换 BMCLAPI 镜像）。模组整合包请使用「导入整合包」。版本按发布时间从新到旧排列。</div>
      </label>
    </div>
    <div id="create-modded" style="display:none">
      <label>模组加载器
        <select id="ci-loader" onchange="loadLoaderVersions()">
          <option value="fabric">Fabric</option>
          <option value="quilt">Quilt</option>
          <option value="forge">Forge</option>
          <option value="neoforge">NeoForge</option>
          <option value="paper">Paper（插件服）</option>
          <option value="purpur">Purpur（插件服）</option>
          <option value="folia">Folia（插件服 · 多线程）</option>
          <option value="velocity">Velocity（代理端）</option>
          <option value="waterfall">Waterfall（代理端）</option>
          <option value="bungeecord">BungeeCord（代理端）</option>
        </select>
        <div class="muted small">Fabric / Quilt：官方一键启动器。Forge / NeoForge：运行官方安装器（需要几分钟）。Paper / Purpur / Folia：插件服，装 Bukkit 系插件到 plugins/。Velocity / BungeeCord：代理端，用于群组服。安装完成后建议检查实例设置的 Java 是否满足要求。</div>
      </label>
      <label class="check" style="margin:4px 0"><input type="checkbox" id="ci-mod-pre" onchange="loadLoaderVersions()"> 显示预览版 / 快照版本</label>
      <div class="row" id="ci-loader-game-row">
        <label style="flex:1">大版本<select id="ci-loader-game" onchange="updateLoaderMinor()"><option value="">加载中…</option></select></label>
        <label style="flex:1">具体版本<select id="ci-loader-minor" onchange="loadLoaderVerList()"><option value="">—</option></select></label>
        <label style="flex:1">服务端版本<select id="ci-loader-ver"><option value="">加载中…</option></select></label>
      </div>
      <div class="muted small" id="ci-loader-hint"></div>
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
  var loader = document.getElementById('ci-loader') ? document.getElementById('ci-loader').value : '';
  var gameRow = document.getElementById('ci-loader-game-row');
  if (!loader || !gameRow) return;
  // 切换加载器后立即清空三个下拉框，避免短暂显示上一个加载器的数据
  var majorSel0 = document.getElementById('ci-loader-game');
  var minorSel0 = document.getElementById('ci-loader-minor');
  var verSel = document.getElementById('ci-loader-ver');
  if (majorSel0) majorSel0.innerHTML = '<option value="">加载中…</option>';
  if (minorSel0) minorSel0.innerHTML = '<option value="">—</option>';
  if (verSel) verSel.innerHTML = '<option value="">—</option>';
  // 代理端无 MC 版本维度
  var isProxy = loader === 'velocity' || loader === 'bungeecord';
  var gameLbl = gameRow.querySelector('label:first-child');
  if (gameLbl) gameLbl.style.display = isProxy ? 'none' : '';
  var minorLbl = gameRow.querySelector('label:nth-child(2)');
  if (minorLbl) minorLbl.style.display = isProxy ? 'none' : '';
  // 各加载器覆盖范围提示（数据源官方下限，避免误以为列表缺失）
  var hint = document.getElementById('ci-loader-hint');
  if (hint) {
    var hints = {
      fabric: 'Fabric 官方支持 MC 1.14 及以上，更早版本请选择 Forge；默认仅列出正式版，勾选「显示预览版」可查看快照 / 预发布。',
      quilt: 'Quilt 官方支持 MC 1.14.4 及以上，更早版本请选择 Forge；默认仅列出正式版，勾选「显示预览版」可查看快照 / 预发布。',
      forge: 'Forge 覆盖 MC 1.1 至最新版本，列表来自官方 maven 全量数据。',
      neoforge: 'NeoForge 支持 MC 1.20.1 及以上（年制版本 26.x 同步支持）。',
      paper: 'Paper 官方 Fill API 提供 1.7.10 至最新版本的全量构建。',
      purpur: 'Purpur 基于 Paper，支持 MC 1.8.8 及以上版本。',
      folia: 'Folia 基于 Paper 的多线程分支，仅支持较新版本（1.19.4+）。',
      velocity: 'Velocity 为现代代理端，构建与 MC 版本无关。',
      waterfall: 'Waterfall 为 BungeeCord 分支代理端。',
      bungeecord: 'BungeeCord 为官方代理端，提供最新稳定构建。'
    };
    hint.textContent = hints[loader] || '';
  }
  try {
    var all = await ensureLoaderGameVersions(loader);
    renderLoaderGameVersions(loader, all, isProxy);
  } catch (e) {
    var sel = document.getElementById('ci-loader-game');
    if (sel) sel.innerHTML = '<option value="">加载失败：' + esc(e.message) + '</option>';
  }
}
// 各加载器的 MC 版本列表按加载器缓存：切换回来时无需重新请求
async function ensureLoaderGameVersions(loader) {
  if (!window._loaderGameCache) window._loaderGameCache = {};
  if (!window._loaderGameCache[loader]) {
    var g = await api('/loaders/' + loader + '/game-versions');
    window._loaderGameCache[loader] = g.versions || [];
  }
  return window._loaderGameCache[loader];
}
function renderLoaderGameVersions(loader, all, isProxy) {
  var showPre = document.getElementById('ci-mod-pre') ? document.getElementById('ci-mod-pre').checked : true;
  var filtered = showPre ? all : all.filter(function(v) { return v.stable; });
  var ids = filtered.map(function(v) { return v.id; });
  var gv = groupVersions(ids);
  var majorSel = document.getElementById('ci-loader-game');
  if (isProxy) {
    // 代理端：直接加载版本列表
    loadLoaderVerList();
    return;
  }
  majorSel.innerHTML = gv.order.map(function(gp) {
    return '<option value="' + esc(gp) + '">' + esc(gp) + '</option>';
  }).join('');
  window._loaderGroups = gv.groups;
  updateLoaderMinor(loader);
}
function updateLoaderMinor(loader) {
  var g = document.getElementById('ci-loader-game') ? document.getElementById('ci-loader-game').value : '';
  var minorSel = document.getElementById('ci-loader-minor');
  if (!minorSel) return;
  var list = (window._loaderGroups && window._loaderGroups[g]) || [];
  minorSel.innerHTML = list.map(function(v) {
    return '<option value="' + esc(v) + '">' + esc(v) + '</option>';
  }).join('');
  loadLoaderVerList();
}
async function loadLoaderVerList() {
  const loader = $('#ci-loader')?.value;
  const gameSel = $('#ci-loader-game');
  const minorSel = $('#ci-loader-minor');
  const verSel = $('#ci-loader-ver');
  if (!loader || !verSel) return;
  // 服务端构建按具体 MC 版本查询；代理端无该维度时回退到大版本
  const game = (minorSel && minorSel.value) || (gameSel && gameSel.value) || '';
  verSel.innerHTML = '<option value="">加载中…</option>';
  try {
    const d = await api(`/loaders/${loader}/loader-versions?game=${encodeURIComponent(game)}`);
    verSel.innerHTML = d.versions.map(v => `<option value="${esc(v)}">${esc(v)}</option>`).join('');
  } catch (e) {
    verSel.innerHTML = `<option value="">加载失败：${esc(e.message)}</option>`;
  }
}
// 按大版本分组（取前两段点分数字作为组名；每周快照归入同一年 "Nw 系列"）
function groupVersions(ids) {
  var groups = {};
  var order = [];
  for (var i = 0; i < ids.length; i++) {
    var id = ids[i];
    var g;
    if (/^\d{2}w\d{2}/.test(id)) {
      g = id.slice(0, 3) + ' 系列';
    } else {
      var parts = id.split(/[.\-]/);
      g = parts.length >= 2 ? parts[0] + '.' + parts[1] : id;
    }
    if (!groups[g]) { groups[g] = []; order.push(g); }
    groups[g].push(id);
  }
  return { groups: groups, order: order };
}

// 填充两层版本下拉框
function fillVersionTwoLevel(majorSel, minorSel, allIds, selectedId) {
  var gv = groupVersions(allIds);
  majorSel.innerHTML = gv.order.map(function(g) {
    return '<option value="' + esc(g) + '">' + esc(g) + '</option>';
  }).join('');
  function updateMinor() {
    var g = majorSel.value;
    var list = gv.groups[g] || [];
    minorSel.innerHTML = list.map(function(v) {
      return '<option value="' + esc(v) + '">' + esc(v) + '</option>';
    }).join('');
    if (selectedId && list.includes(selectedId)) minorSel.value = selectedId;
  }
  majorSel.onchange = updateMinor;
  updateMinor();
}

async function loadVersionOptions() {
  try {
    var d = await api('/versions');
    // 存储全部版本（含 release_time），按发布时间从新到旧排序
    window._mcVersions = (d.versions || []).slice().sort(function(a, b) {
      return (b.release_time || '').localeCompare(a.release_time || '');
    });
    updateVanillaMajor();
    updateVanillaMinor();
  } catch (e) {
    var sel = document.getElementById('ci-version');
    if (sel) sel.innerHTML = '<option value="">版本清单获取失败：' + esc(e.message) + '</option>';
  }
}
function updateVanillaMinor() {
  var majorSel = document.getElementById('ci-ver-major');
  var minorSel = document.getElementById('ci-version');
  if (!majorSel || !minorSel) return;
  var showPre = document.getElementById('ci-show-pre') ? document.getElementById('ci-show-pre').checked : false;
  var g = majorSel.value;
  var all = window._mcVersions || [];
  // 用 updateVanillaMajor 建好的 分组映射 过滤，保证与下拉框分组一致
  var inGroup = all.filter(function(v) {
    if (showPre === false && v.type !== 'release') return false;
    return window._vanillaGroupMap && window._vanillaGroupMap[v.id] === g;
  });
  minorSel.innerHTML = '<option value="">不下载，稍后手动导入或配置</option>' +
    inGroup.map(function(v) { return '<option value="' + esc(v.id) + '">' + esc(v.id) + '</option>'; }).join('');
}
function updateVanillaMajor() {
  var majorSel = document.getElementById('ci-ver-major');
  if (!majorSel) return;
  var showPre = document.getElementById('ci-show-pre') ? document.getElementById('ci-show-pre').checked : false;
  var all = window._mcVersions || [];
  var filtered = showPre ? all : all.filter(function(v) { return v.type === 'release'; });
  var ids = filtered.map(function(v) { return v.id; });
  var gv = groupVersions(ids);
  // 保存 版本id → 组名 映射，供 updateVanillaMinor 使用
  var map = {};
  gv.order.forEach(function(gp) { gv.groups[gp].forEach(function(id) { map[id] = gp; }); });
  window._vanillaGroupMap = map;
  var prev = majorSel.value;
  majorSel.innerHTML = gv.order.map(function(g) {
    return '<option value="' + esc(g) + '">' + esc(g) + '</option>';
  }).join('');
  if (prev && gv.order.includes(prev)) majorSel.value = prev;
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
    const game = $('#ci-loader-minor') ? $('#ci-loader-minor').value : ($('#ci-loader-game') ? $('#ci-loader-game').value : '');
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
  if (!(await appConfirm(`确定删除实例「${name}」？\n实例目录将被彻底删除，不可恢复！`, { danger: true, okText: '彻底删除' }))) return;
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
const INST_TABS = [['console', '控制台'], ['monitor', '监控'], ['mods', '模组'], ['backups', '备份'], ['game-backups', '游戏内备份'], ['worlds', '世界'], ['tasks', '计划任务'], ['files', '文件'], ['props', '服务器设置'], ['settings', '实例设置']];

function instanceRuntimeText(s) {
  return (s.status === 'running' || s.status === 'starting')
    ? `已运行 ${fmtUptime(s.uptime_secs)} · PID ${s.pid || '-'} · ${s.players || 0} 名玩家在线`
    : '';
}

async function renderInstance(id, tab, t = ++routeToken) {
  let s;
  try { s = await api(`/instances/${id}`); }
  catch (e) { if (t === routeToken) $('#main').innerHTML = `<div class="empty">${esc(e.message)}</div>`; return; }
  if (t !== routeToken) return;
  currentInstanceInfo = s;
  const main = $('#main');
  if (main.dataset.instanceId === String(id) && $('#tab-body')) {
    $$('#main > .detail-tabs .tab').forEach(a => a.classList.toggle('active', a.getAttribute('href') === `#/instance/${id}/${tab}`));
    $('#tab-body').replaceChildren();
    $('#tab-body').scrollTop = 0;
  } else {
    main.dataset.instanceId = String(id);
    main.classList.add('detail-layout');
    main.innerHTML = `
    <div class="page-head detail-head"><div class="detail-title"><h1><span id="inst-name">${esc(s.name)}</span></h1></div><div class="detail-status"><span id="inst-status">${statusPill(s.status)}</span><span class="muted small" id="inst-sub"></span></div><div class="row" id="inst-actions"></div></div>
    <div id="eula-banner"></div>
    <div class="tabs detail-tabs">${INST_TABS.map(([k, label]) => `<a class="tab ${k === tab ? 'active' : ''}" href="#/instance/${id}/${k}">${label}</a>`).join('')}</div>
    <div id="tab-body"></div>`;
  }
  $('#inst-name').textContent = s.name;
  $('#inst-status').innerHTML = statusPill(s.status);
  $('#inst-sub').textContent = instanceRuntimeText(s);
  $('#inst-actions').innerHTML = actionButtons(id, s.status);
  const body = $('#tab-body');
  if (tab === 'console') renderTabConsole(id, body, t);
  else if (tab === 'monitor') renderTabMonitor(id, body, t);
  else if (tab === 'mods') renderTabMods(id, body, t);
  else if (tab === 'backups') renderTabBackups(id, body, t);
  else if (tab === 'worlds') renderTabWorlds(id, body, t);
  else if (tab === 'tasks') renderTabTasks(id, body, t);
  else if (tab === 'files') renderTabFiles(id, body, t);
  else if (tab === 'props') renderTabProps(id, body, t);
  else if (tab === 'game-backups') renderTabGameBackups(id, body, t);
  else renderTabSettings(id, body);

  const upd = async () => {
    try {
      const s2 = await api(`/instances/${id}/status`);
      if (t !== routeToken) return;
      const previousStatus = currentInstanceInfo?.status;
      currentInstanceInfo = { ...currentInstanceInfo, ...s2 };
      $('#inst-actions').innerHTML = actionButtons(id, s2.status);
      const pill = $('#inst-status .pill');
      if (pill) { pill.className = `pill st-${esc(s2.status)}`; pill.textContent = STATUS_TEXT[s2.status] || s2.status; }
      const sub = $('#inst-sub');
      if (sub) sub.textContent = instanceRuntimeText(s2);
      $('#eula-banner').innerHTML =
        (!s2.eula_accepted && (s2.status === 'stopped'))
          ? `<div class="banner warn"><span>该实例尚未同意 Minecraft EULA，直接启动会失败。</span><button class="btn small" onclick="acceptEula('${id}')">同意 EULA 并继续</button></div>`
          : '';
      if (tab === 'game-backups' && previousStatus !== s2.status) loadGameBackups(id, t);
    } catch {}
  };
  await upd();
  if (t === routeToken) every(2500, upd);
}

function actionButtons(id, status) {
  const parts = [];
  if (status === 'stopped') parts.push(`<button class="btn primary" onclick="instStart('${id}')">▶ 启动</button>`);
  else if (status === 'running' || status === 'starting') parts.push(
    `<button class="btn warn" onclick="instStop('${id}')">■ 停止</button>`,
    `<button class="btn" onclick="instRestart('${id}')">⟳ 重启</button>`);
  else parts.push(`<span class="muted small">停止中…</span>`);
  parts.push(`<button class="btn ghost" onclick="openFolder('${id}')">打开目录</button>`);
  parts.push(`<button class="btn ghost" onclick="showModpackUpdate('${id}')">⤒ 更新整合包</button>`);
  return parts.join('');
}

/* ---------------- 更新整合包 ---------------- */
let mpUp = null;

function showModpackUpdate(id) {
  id = id || (currentInstanceInfo && currentInstanceInfo.id);
  const meta = currentInstanceInfo || {};
  mpUp = { id, preview: null, currentMods: [] };
  showModal(`<h2>⤒ 更新整合包</h2>
    <p class="muted small" style="margin:0 0 10px">选择新版本的整合包 zip，解析后先预览差异再执行。world 存档、server.properties 与实例设置不会被改动；更新前要求实例已停止。</p>
    <label>整合包 zip 文件<input type="file" id="mu-file" accept=".zip" style="margin:6px 0 10px"></label>
    <label>整合包未包含的模组<select id="mu-orphan">
      <option value="disable">禁用（改名 .disabled，可恢复）【推荐】</option>
      <option value="keep">保留不动</option>
      <option value="delete">直接删除</option>
    </select></label>
    <div id="mu-progress" style="display:none;margin:10px 0">
      <div class="bar"><i id="mu-bar" style="width:40%"></i></div>
      <div class="muted small" id="mu-status">解析中…</div>
    </div>
    <div id="mu-preview"></div>
    <div class="row right" style="margin-top:12px" id="mu-actions">
      <button class="btn ghost" onclick="closeModal()">取消</button>
      <button class="btn primary" id="mu-go" onclick="parseModpackPreview()">① 解析预览</button>
    </div>`);
  // 预取当前模组列表，用于差异计算
  api(`/instances/${id}/mods`).then(d => { mpUp.currentMods = (d.mods || []).map(m => m.file); }).catch(() => {});
}

async function parseModpackPreview() {
  const fileInput = document.getElementById('mu-file');
  const f = fileInput && fileInput.files[0];
  if (!f) return toast('请选择整合包 zip 文件', false);
  if (currentInstanceInfo && currentInstanceInfo.status !== 'stopped') {
    return toast('更新前请先停止实例', false);
  }
  const fd = new FormData();
  fd.append('file', f);
  const bar = document.getElementById('mu-bar');
  const st = document.getElementById('mu-status');
  document.getElementById('mu-progress').style.display = '';
  if (bar) bar.style.width = '40%';
  if (st) st.textContent = '上传并解析整合包…（大包可能需要一些时间）';
  try {
    mpUp.preview = await api(`/instances/${mpUp.id}/modpack/preview`, { method: 'POST', body: fd });
    if (bar) bar.style.width = '100%';
    if (st) st.textContent = '解析完成';
    renderModpackPreview();
  } catch (e) {
    document.getElementById('mu-progress').style.display = 'none';
    toast(e.message, false);
  }
}

function renderModpackPreview() {
  const p = mpUp.preview;
  if (!p) return;
  const current = new Set(mpUp.currentMods.map(x => x.toLowerCase()));
  const packNames = p.mods.map(m => m.filename);
  const packLower = new Set(packNames.map(x => x.toLowerCase()));
  const added = packNames.filter(n => !current.has(n.toLowerCase()));
  const sameName = packNames.filter(n => current.has(n.toLowerCase()));
  const removed = mpUp.currentMods.filter(n => {
    const low = n.toLowerCase();
    const base = low.endsWith('.disabled') ? low.slice(0, -9) : low;
    return !packLower.has(base);
  });
  const loaderChanged = p.loader && p.loader !== (currentInstanceInfo?.mod_loader || '');
  const mcChanged = p.mc_version && p.mc_version !== (currentInstanceInfo?.mc_version || '');
  const el = document.getElementById('mu-preview');
  el.innerHTML = `<div class="card" style="padding:10px 14px;margin-top:8px">
    <div class="row between"><b>${esc(p.name)}</b>
      <span class="muted small">${{ curseforge: 'CurseForge 包', modrinth: 'Modrinth 包', generic: '通用服务端包' }[p.pack_type] || p.pack_type}</span></div>
    <div class="muted small" style="margin:4px 0 8px">
      MC ${esc(p.mc_version || '未声明')} · 加载器 ${esc(p.loader || '无')}${p.loader_version ? ' ' + esc(p.loader_version) : ''}
      ${loaderChanged ? '<span class="pill st-warn">加载器将变化</span>' : ''}
      ${mcChanged ? '<span class="pill st-warn">MC 版本将变化（自动重装加载器）</span>' : ''}
    </div>
    <div class="small" style="line-height:1.9">
      模组：<b>${p.mods.length}</b> 个（新增 ${added.length}，同名覆盖 ${sameName.length}）
      ${removed.length ? `· 包外 ${removed.length} 个将按所选方式处理` : '· 无包外模组'}
      ${p.overrides_files ? `· 配置/资源覆盖 <b>${p.overrides_files}</b> 个文件` : ''}
    </div>
    ${(added.length || removed.length) ? `<details style="margin-top:6px"><summary class="muted small" style="cursor:pointer">查看明细</summary>
      ${added.length ? `<div class="mono small" style="margin-top:6px;max-height:150px;overflow:auto">新增/更新：${added.map(n => esc(n)).join('、')}</div>` : ''}
      ${removed.length ? `<div class="mono small" style="margin-top:6px;max-height:100px;overflow:auto">包外：${removed.map(n => esc(n)).join('、')}</div>` : ''}
    </details>` : ''}
    ${p.cf_unresolved ? `<div class="banner warn" style="padding:8px 12px;margin-top:8px">有 ${p.cf_unresolved} 个 CurseForge 模组未能解析（请检查面板设置中的 CurseForge API Key），将无法下载。</div>` : ''}
  </div>`;
  document.getElementById('mu-actions').innerHTML = `
    <button class="btn ghost" onclick="closeModal()">取消</button>
    <button class="btn primary" id="mu-go" onclick="startModpackUpdate()">② 开始更新</button>`;
}

function askModpackBackup() {
  return new Promise(resolve => {
    const root = dlgLayer();
    root.innerHTML = `<div class="modal-backdrop" style="z-index:200"><div class="modal" style="max-width:420px">
      <h3 style="margin:0 0 10px">更新前备份</h3>
      <div>更新整合包前是否先备份实例？<div class="muted small" style="margin-top:6px">备份为完整 tar.gz，保存在「备份」页，可随时恢复回滚。</div></div>
      <div class="row right" style="margin-top:14px">
        <button class="btn ghost" data-dlg="cancel">取消更新</button>
        <button class="btn" data-dlg="no">直接更新</button>
        <button class="btn primary" data-dlg="yes">备份并更新</button></div></div></div>`;
    const finish = v => { root.innerHTML = ''; resolve(v); };
    root.querySelectorAll('[data-dlg]').forEach(btn =>
      btn.addEventListener('click', () => finish(btn.getAttribute('data-dlg'))));
  });
}

async function startModpackUpdate() {
  const p = mpUp && mpUp.preview;
  if (!p) return;
  const choice = await askModpackBackup();
  if (choice === 'cancel') return;
  document.getElementById('mu-preview').innerHTML = '';
  document.getElementById('mu-progress').style.display = '';
  const bar = document.getElementById('mu-bar');
  const st = document.getElementById('mu-status');
  if (bar) { bar.style.width = '30%'; bar.classList.add('indeterminate'); }
  if (st) st.textContent = choice === 'yes' ? '正在执行更新（先备份）…' : '正在执行更新…';
  const go = document.getElementById('mu-go');
  if (go) go.disabled = true;
  try {
    const r = await api(`/instances/${mpUp.id}/modpack/apply`, {
      method: 'POST',
      body: {
        preview_id: p.preview_id,
        backup: choice === 'yes',
        orphan_mode: document.getElementById('mu-orphan') ? document.getElementById('mu-orphan').value : 'disable',
        allow_reinstall: true,
      },
    });
    await pollJob(r.job_id, j => {
      if (st) {
        st.textContent = j.logs.slice(-1)[0] || '更新中…';
        if (bar) bar.style.width = Math.max(30, Math.min(95, 30 + j.logs.length * 2)) + '%';
      }
    });
    if (bar) bar.style.width = '100%';
    if (st) st.textContent = '✅ 更新完成';
    toast('整合包更新完成', true);
  } catch (e) {
    if (st) st.textContent = '更新失败：' + e.message;
    toast(e.message, false);
  }
}

/* ---------------- 控制台 + 用户管理 ---------------- */

/* ---------------- Commit3 前端补充 ---------------- */
// 控制台增强：命令历史 / Tab 补全 / 搜索过滤 / 日志下载
let consoleLines = [];
let cmdHistory = [];
let cmdHistIdx = -1;
let consoleFilter = '';

function applyConsoleFilter() {
  const logEl = document.getElementById('console-log');
  if (!logEl) return;
  const q = (document.getElementById('console-search')?.value || '').toLowerCase();
  const lvl = document.getElementById('console-level')?.value || '';
  for (const div of logEl.children) {
    const text = div.textContent.toLowerCase();
    let show = true;
    if (q && !text.includes(q)) show = false;
    if (lvl === 'err' && !div.classList.contains('err')) show = false;
    if (lvl === 'warn' && !div.classList.contains('warn') && !div.classList.contains('err')) show = false;
    div.style.display = show ? '' : 'none';
  }
}
async function downloadConsole(id) {
  try {
    const r = await fetch(`/api/instances/${id}/console/download`, { headers: headers() });
    if (!r.ok) throw new Error(r.statusText);
    const blob = await r.blob();
    const a = document.createElement('a');
    a.href = URL.createObjectURL(blob);
    a.download = `console-${id}.log`;
    a.click();
    URL.revokeObjectURL(a.href);
  } catch (e) { toast(e.message, false); }
}
const TAB_COMMANDS = ['list', 'say ', 'op ', 'deop ', 'kick ', 'ban ', 'ban-ip ', 'pardon ', 'pardon-ip ', 'whitelist add ', 'whitelist remove ', 'whitelist list', 'stop', 'save-all', 'save-on', 'save-off', 'tps', 'restart ', 'difficulty ', 'gamemode ', 'time set ', 'weather ', 'give ', 'tp ', 'fill ', 'setworldspawn ', 'defaultgamemode '];
function consoleTabComplete(id) {
  const input = document.getElementById('cmd-input');
  if (!input) return;
  const text = input.value;
  const parts = text.split(' ');
  const last = parts[parts.length - 1].toLowerCase();
  // 第一个词：补全命令；之后：补全在线玩家名
  let pool = null, prefix = '';
  if (parts.length === 1) {
    pool = TAB_COMMANDS.filter(c => !c.endsWith(' ')).concat(['help']);
    prefix = last;
  } else if (parts.length >= 2 && ['op', 'deop', 'kick', 'ban', 'pardon', 'whitelist'].includes(parts[0])) {
    pool = (modDL ? [] : []).concat((usersData?.online || []), (playersData?.players || []));
    prefix = last;
  }
  if (!pool) return;
  const hit = pool.find(c => c.toLowerCase().startsWith(prefix) && prefix);
  if (hit !== undefined && prefix) {
    parts[parts.length - 1] = hit;
    input.value = parts.join(' ');
  }
}
function consoleLineParts(line) {
  const m = /^(\[[^\]]+\])\s+(\[[^\]]+\])\s*(?:\[([^\]]+)\]\s*)?(.*)$/.exec(String(line));
  return m ? [m[1], m[2], m[3] || '', m[4]] : null;
}
function consoleBatch(message, lastSeq = 0) {
  if (message?.type === 'history' && Array.isArray(message.lines)) {
    const seq = Number(message.cursor) || Math.max(0, ...message.lines.map(x => Number(x.seq) || 0));
    return { replace: true, lines: message.lines, cursor: message.cursor ?? 0, lastSeq: seq };
  }
  if (message?.line !== undefined && (!message.seq || message.seq > lastSeq))
    return { replace: false, lines: [message], cursor: message.seq ?? lastSeq, lastSeq: message.seq ?? lastSeq };
  return { replace: false, lines: [], cursor: lastSeq, lastSeq };
}
function consoleLineMeta(line) {
  const classes = [];
  if (/ERROR|FATAL|Exception|崩溃|\[ERROR\]/i.test(line)) classes.push('err');
  else if (/WARN|警告|\[WARN\]/i.test(line)) classes.push('warn');
  if (/\[(?:[^\]]*\/)?INFO\].*(?:Done|完成)/i.test(line)) classes.push('log-success');
  if (/ joined the game| left the game|加入了游戏|离开了游戏/i.test(line)) classes.push('log-player');
  const level = /(?:\/|\[)(TRACE|DEBUG|INFO|WARN|ERROR|FATAL)\]/i.exec(line)?.[1]?.toLowerCase() || '';
  return { classes, levelClass: `log-level-${level}` };
}

function renderTabConsole(id, el, t) {
  el.innerHTML = `
    <div class="console-wrap">
      <div id="console-log" class="console"></div>
      <div class="row console-input">
        <input id="cmd-input" placeholder="输入命令（↑↓ 历史，Tab 补全）" autocomplete="off"
          onkeydown="consoleKeydown(event, '${id}')">
        <button class="btn" id="cmd-send">发送</button>
      </div>
      <div class="row" style="margin-top:8px">
        <input id="console-search" placeholder="搜索日志…" style="width:200px" oninput="applyConsoleFilter()">
        <select id="console-level" style="width:auto" onchange="applyConsoleFilter()">
          <option value="">全部级别</option><option value="warn">仅警告+</option><option value="err">仅错误</option>
        </select>
        <button class="btn small ghost" onclick="downloadConsole('${id}')">⬇ 下载日志</button>
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
  let retryTimer = null, pollTimer = null, cursor = 0, lastSeq = 0, wsReady = false;
  const stopPoll = () => { if (pollTimer) clearTimeout(pollTimer); pollTimer = null; };
  const appendMany = (items, forceBottom = false) => {
    const oldTop = logEl.scrollTop;
    if (forceBottom && items.length > maxLines) items = items.slice(-maxLines);
    const nearBottom = logEl.scrollHeight - logEl.scrollTop - logEl.clientHeight <= 60;
    const frag = document.createDocumentFragment();
    for (const o of items) {
      const line = String(o.line ?? ''), meta = consoleLineMeta(line);
      const div = document.createElement('div'); div.className = `cline ${meta.classes.join(' ')}`;
      const match = consoleLineParts(line);
      if (!match && o.ts) { const ts = document.createElement('span'); ts.className = 'ts'; ts.textContent = String(o.ts); div.appendChild(ts); }
      if (match) {
        for (const [text, cls] of [[match[1], 'log-time'], [match[2], meta.levelClass], [match[3] ? `[${match[3]}]` : '', 'log-mod'], [match[4], '']]) {
          if (!text) continue;
          const span = document.createElement('span'); if (cls) span.className = cls; span.textContent = text + (cls ? ' ' : ''); div.appendChild(span);
        }
      } else div.appendChild(document.createTextNode(line));
      frag.appendChild(div);
    }
    logEl.appendChild(frag);
    let removedHeight = 0;
    while (logEl.children.length > maxLines) {
      removedHeight += logEl.firstChild.offsetHeight;
      logEl.removeChild(logEl.firstChild);
    }
    applyConsoleFilter();
    logEl.scrollTop = forceBottom || nearBottom ? logEl.scrollHeight : Math.max(0, oldTop - removedHeight);
  };
  const pollConsole = async () => {
    if (t !== routeToken) return;
    try {
      let d = await api(`/instances/${id}/console?after=${cursor}`);
      if (t !== routeToken || wsReady) return;
      const initial = cursor === 0;
      if (d.cursor < cursor) {
        cursor = 0; lastSeq = 0; logEl.replaceChildren();
        d = await api(`/instances/${id}/console?after=0`);
        if (t !== routeToken || wsReady) return;
      }
      const fresh = (d.lines || []).filter(x => !x.seq || x.seq > lastSeq);
      if (fresh.length) { appendMany(fresh, initial || lastSeq === 0); lastSeq = Math.max(lastSeq, ...fresh.map(x => Number(x.seq) || 0)); }
      cursor = d.cursor ?? cursor;
    } catch {}
    if (t === routeToken && !wsReady) { pollTimer = setTimeout(pollConsole, 2000); timers.push(pollTimer); }
  };

  const connect = () => {
    if (t !== routeToken) return;
    const proto = location.protocol === 'https:' ? 'wss' : 'ws';
    const ws = new WebSocket(`${proto}://${location.host}/api/instances/${id}/ws?token=${encodeURIComponent(TOKEN)}`);
    activeWS = ws;
    ws.onmessage = e => {
      if (t !== routeToken || activeWS !== ws) return;
      try {
        const batch = consoleBatch(JSON.parse(e.data), lastSeq);
        if (batch.replace) { logEl.replaceChildren(); appendMany(batch.lines, true); }
        else if (batch.lines.length) appendMany(batch.lines);
        cursor = batch.cursor ?? cursor;
        lastSeq = batch.lastSeq;
      } catch {}
    };
    ws.onopen = () => { wsReady = true; stopPoll(); };
    ws.onclose = () => {
      if (t === routeToken && activeWS === ws) {
        wsReady = false; pollConsole(); retryTimer = setTimeout(connect, 3000); timers.push(retryTimer);
      }
    };
  };
  connect();

  const send = async () => {
    const v = $('#cmd-input').value.trim();
    if (!v) return;
    cmdHistory.push(v);
    cmdHistIdx = cmdHistory.length;
    try {
      await api(`/instances/${id}/command`, { method: 'POST', body: { command: v } });
      $('#cmd-input').value = '';
    } catch (e) { toast(e.message, false); }
  };
  $('#cmd-send').onclick = send;
  $('#cmd-input').onkeydown = e => consoleKeydown(e, id);

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
    $('#users-body').innerHTML = list.length ? `<div class="table-wrap"><table class="table">
      <thead><tr><th>玩家</th><th>快捷操作</th></tr></thead>
      <tbody>${list.map(p => `<tr><td><b>${esc(p)}</b></td><td>
        <button class="btn small primary" onclick="userAction('${id}','op','${esc(p)}')">OP</button>
        <button class="btn small" onclick="userAction('${id}','whitelist_add','${esc(p)}')">白名单</button>
        <button class="btn small warn" onclick="userAction('${id}','kick','${esc(p)}')">踢出</button>
        <button class="btn small danger" onclick="userAction('${id}','ban','${esc(p)}')">封禁</button>
      </td></tr>`).join('')}</tbody></table></div>`
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
    ? `<div class="table-wrap"><table class="table"><thead><tr>${conf.cols.map(c => `<th>${c}</th>`).join('')}<th>操作</th></tr></thead><tbody>${rows}</tbody></table></div>`
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

/* ---------------- 监控（TPS / CPU 内存历史 / 在线时长 / 崩溃归档） ---------------- */
function renderChart(el, points) {
  if (!points.length) { el.innerHTML = '<div class="empty">暂无数据（实例运行后每 30 秒采样一次）</div>'; return; }
  const w = 800, h = 220, pad = 36;
  const n = points.length;
  const x = i => pad + (w - 2 * pad) * (i / Math.max(n - 1, 1));
  const cpuMax = Math.max(10, ...points.map(p => p[1]));
  const memMax = Math.max(1, ...points.map(p => p[2]));
  const cpuPts = points.map((p, i) => `${x(i)},${h - pad - (h - 2 * pad) * (p[1] / cpuMax)}`).join(' ');
  const memPts = points.map((p, i) => `${x(i)},${h - pad - (h - 2 * pad) * (p[2] / memMax)}`).join(' ');
  el.innerHTML = `<svg viewBox="0 0 ${w} ${h}" style="width:100%;background:var(--console-bg);border-radius:8px">
    <polyline fill="none" stroke="var(--accent)" stroke-width="1.5" points="${cpuPts}"/>
    <polyline fill="none" stroke="var(--blue)" stroke-width="1.5" points="${memPts}"/>
    <text x="${pad}" y="18" fill="var(--accent)" font-size="12">CPU%（峰 ${cpuMax.toFixed(1)}）</text>
    <text x="${pad + 240}" y="18" fill="var(--blue)" font-size="12">内存 MB（峰 ${memMax.toFixed(0)}）</text>
    <text x="${pad}" y="${h - 8}" fill="var(--muted)" font-size="11">${new Date(points[0][0] * 1000).toLocaleTimeString()}</text>
    <text x="${w - pad - 110}" y="${h - 8}" fill="var(--muted)" font-size="11">${new Date(points[n - 1][0] * 1000).toLocaleTimeString()}</text>
  </svg>`;
}
async function renderTabMonitor(id, el, t) {
  el.innerHTML = `<h2>监控</h2>
    <div class="grid stats-grid">
      <div class="card"><h3>TPS / MSPT</h3><div class="big" id="mon-tps">-</div><div class="muted small" id="mon-tps-hint"></div></div>
      <div class="card"><h3>运行状态</h3><div class="muted small" id="mon-live">-</div></div>
    </div>
    <h3>CPU / 内存历史（每 30 秒采样，最多 24 小时）</h3>
    <div id="mon-chart"><div class="empty">加载中…</div></div>
    <h3>玩家在线时长</h3>
    <div id="mon-playtime"><div class="empty">加载中…</div></div>
    <h3>崩溃归档</h3>
    <div id="mon-crashes"><div class="empty">加载中…</div></div>`;
  const load = async () => {
    if (t !== routeToken) return;
    try {
      const [s, m, pt, cr] = await Promise.all([
        api(`/instances/${id}/status`), api(`/instances/${id}/metrics`),
        api(`/instances/${id}/playtime`), api(`/instances/${id}/crashes`),
      ]);
      if (t !== routeToken) return;
      const tps = s.tps;
      const tEl = document.getElementById('mon-tps'), hEl = document.getElementById('mon-tps-hint');
      if (tps && tps.tps !== undefined) { tEl.textContent = tps.tps.toFixed(1); hEl.textContent = tps.mspt !== undefined ? `MSPT ${tps.mspt}ms` : ''; }
      else if (tps && tps.needs_rcon) { tEl.textContent = '需要 RCON'; hEl.textContent = '在服务器设置中开启 enable-rcon 并设置 rcon.password 后，面板每 10 秒采样 TPS。'; }
      else { tEl.textContent = '采样中…'; hEl.textContent = ''; }
      document.getElementById('mon-live').innerHTML = s.status === 'running'
        ? `运行 ${fmtUptime(s.uptime_secs)} · PID ${s.pid || '-'}`
        : '未运行';
      renderChart(document.getElementById('mon-chart'), m.points || []);
      const pl = pt.players || [];
      document.getElementById('mon-playtime').innerHTML = pl.length
        ? '<div class="table-wrap"><table class="table"><thead><tr><th>玩家</th><th>总时长</th><th>会话数</th></tr></thead><tbody>' +
          pl.map(x => `<tr><td><b>${esc(x.name)}</b></td><td>${fmtUptime(x.total_secs)}</td><td>${x.sessions}</td></tr>`).join('') +
          '</tbody></table></div>'
        : '<div class="empty" style="padding:14px">暂无数据（玩家进出服务器时统计）</div>';
      const files = cr.files || [];
      document.getElementById('mon-crashes').innerHTML = files.length
        ? '<div class="table-wrap"><table class="table"><tbody>' +
          files.map(f => `<tr><td class="mono small">${esc(f.name)}</td><td>${fmtSize(f.size)}</td>` +
            `<td><button class="btn small" onclick="viewCrash('${id}','${esc(f.name)}')">查看</button></td></tr>`).join('') +
          '</tbody></table></div>'
        : '<div class="empty" style="padding:14px">无崩溃记录（异常退出时会自动归档控制台末尾与 crash-report）</div>';
    } catch (e) {
      const el = document.getElementById('mon-chart');
      if (el) el.innerHTML = `<div class="empty">${esc(e.message)}</div>`;
    }
  };
  await load();
  every(10000, load);
}
async function viewCrash(id, name) {
  try {
    const d = await api(`/instances/${id}/crashes/file?name=${encodeURIComponent(name)}`);
    showModal(`<h2>${esc(name)}</h2><pre class="job-log" style="max-height:440px">${esc(d.content)}</pre>
      <div class="row right" style="margin-top:10px"><button class="btn" onclick="closeModal()">关闭</button></div>`, 'wide');
  } catch (e) { toast(e.message, false); }
}

async function renderTabMods(id, el, t) {
  const isPlugin = ['paper', 'purpur', 'folia', 'velocity', 'waterfall', 'bungeecord'].includes(currentInstanceInfo?.mod_loader);
  const noun = isPlugin ? '插件' : '模组';
  el.innerHTML = `
    <div class="row between"><h2>${noun}</h2>
      <div class="row"><button class="btn primary" onclick="showModDownload('${id}')">⬇ 下载${noun}</button><button class="btn" onclick="uploadMod('${id}')">上传${noun}</button></div></div>
    <div id="mods-body"><div class="empty">加载中…</div></div>`;
  const load = async () => {
    try {
      const { mods } = await api(`/instances/${id}/mods`);
      if (t !== routeToken) return;
      $('#mods-body').innerHTML = mods.length ? `<div class="table-wrap"><table class="table">
        <thead><tr><th>状态</th><th>名称</th><th>版本</th><th>加载器</th><th>MC 版本</th><th>大小</th><th>操作</th></tr></thead>
        <tbody>${mods.map(m => `<tr>
          <td><span class="pill ${m.enabled ? 'st-running' : 'st-stopped'}">${m.enabled ? '启用' : '禁用'}</span></td>
          <td title="${esc(m.description)}">${esc(m.display_name)}${m.authors ? `<div class="muted small">by ${esc(m.authors)}</div>` : ''}<div class="muted small">${esc(m.file)}</div></td>
          <td>${esc(m.version)}</td><td>${esc(m.loader)}</td><td>${esc(m.mc_version || '-')}</td><td>${fmtSize(m.size)}</td>
          <td>
            <button class="btn small" onclick="toggleMod('${id}','${esc(m.file)}')">${m.enabled ? '禁用' : '启用'}</button>
            <button class="btn small danger" onclick="deleteMod('${id}','${esc(m.file)}')">删除</button>
          </td></tr>`).join('')}</tbody></table></div>`
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
// 通过 Modrinth 哈希反查构建已安装模组的依赖图，找出（直接或间接）依赖 targetFile 的已装模组文件名
async function modDependents(id, targetFile) {
  const h = await api(`/instances/${id}/mods/hashes`);
  const files = h.files || [];
  const target = files.find(f => f.filename === targetFile);
  if (!target || !target.sha1) return [];
  const r = await api('/moddb/version-files', {
    method: 'POST',
    body: { hashes: files.map(f => f.sha1).filter(Boolean) },
  });
  // sha1 → 本地文件名；项目 → 本地文件名 / 必需依赖项目列表
  const shaToFile = {};
  files.forEach(f => { if (f.sha1) shaToFile[f.sha1] = f.filename; });
  const pidToFile = {};
  const requiredDeps = {};
  let targetPid = null;
  for (const [sha, v] of Object.entries(r || {})) {
    if (!v || !v.project_id) continue;
    const local = shaToFile[sha];
    if (!local) continue;
    pidToFile[v.project_id] = local;
    requiredDeps[v.project_id] = (v.dependencies || [])
      .filter(d => d.dependency_type === 'required')
      .map(d => d.project_id);
    if (local === targetFile) targetPid = v.project_id;
  }
  if (!targetPid) return [];
  // 反向广度优先：多层依赖逐层展开（谁依赖了当前层，谁就是下一层）
  const dependents = new Set();
  let frontier = [targetPid];
  while (frontier.length) {
    const next = [];
    for (const pid of Object.keys(requiredDeps)) {
      if (pid === targetPid || dependents.has(pid)) continue;
      if (requiredDeps[pid].some(d => frontier.includes(d))) {
        dependents.add(pid);
        next.push(pid);
      }
    }
    frontier = next;
  }
  const stem = fileStem(targetFile);
  return Array.from(dependents)
    .map(pid => pidToFile[pid])
    .filter(f => f && f !== targetFile && fileStem(f) !== stem);
}
// 依赖确认对话框：列出依赖它的模组，可勾选同时删除
function deleteModDialog(file, deps) {
  return new Promise(resolve => {
    const root = dlgLayer();
    root.innerHTML = `<div class="modal-backdrop" style="z-index:200"><div class="modal" style="max-width:460px">
      <h3 style="margin:0 0 10px">删除模组</h3>
      <div style="margin-bottom:8px">确定删除 <b class="mono">${esc(file)}</b>？</div>
      <div class="banner warn" style="padding:8px 12px;margin-bottom:8px">以下 ${deps.length} 个已安装模组（直接或间接）依赖此模组，删除后它们将无法正常工作：</div>
      <div style="max-height:180px;overflow:auto;margin-bottom:10px">${deps.map(d => `<div class="mono small" style="padding:2px 0">• ${esc(d)}</div>`).join('')}</div>
      <label class="check"><input type="checkbox" id="dlg-dep" checked> 同时删除以上依赖模组</label>
      <div class="row right" style="margin-top:14px">
        <button class="btn ghost" data-dlg="no">取消</button>
        <button class="btn danger" data-dlg="yes">删除</button></div></div></div>`;
    const cb = root.querySelector('#dlg-dep');
    const finish = ok => { const also = ok && cb.checked; root.innerHTML = ''; resolve({ ok, also }); };
    root.querySelectorAll('[data-dlg]').forEach(btn =>
      btn.addEventListener('click', () => finish(btn.getAttribute('data-dlg') === 'yes')));
  });
}
async function deleteMod(id, file) {
  let deps = [];
  try { deps = await modDependents(id, file); } catch {}
  let also = [];
  if (deps.length) {
    const r = await deleteModDialog(file, deps);
    if (!r.ok) return;
    if (r.also) also = deps;
  } else {
    if (!(await appConfirm(`确定删除 ${file}？`, { danger: true, okText: '删除' }))) return;
  }
  try {
    for (const f of [file, ...also]) {
      await api(`/instances/${id}/mods/delete`, { method: 'POST', body: { file: f } });
    }
    toast(also.length ? `已删除 ${also.length + 1} 个模组（含依赖它的模组）` : '已删除');
    refresh();
  } catch (e) { toast(e.message, false); }
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
    installedNames: new Set(),
    installedByProject: {},
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
    // 文件名干集合（去掉 .jar 后缀），用于名称匹配
    modDL.installedNames = new Set();
    for (const f of modDL.installedFiles) {
      const n = fileStem(f.filename);
      if (n) modDL.installedNames.add(n);
    }
    const found = new Set();
    // 项目 → 已安装文件名 映射：换版本下载时据此自动替换旧文件
    const byProject = {};
    const mapProject = (pid, filename) => {
      if (!pid || !filename) return;
      found.add(pid);
      (byProject[pid] = byProject[pid] || []).push(filename);
    };
    // Modrinth：SHA1 精确匹配（响应含完整版本对象，可取到文件名）
    const sha1s = modDL.installedFiles.map(f => f.sha1).filter(Boolean);
    if (sha1s.length) {
      try {
        const r = await api('/moddb/version-files', { method: 'POST', body: { hashes: sha1s } });
        Object.values(r || {}).forEach(v => {
          if (!v || !v.project_id) return;
          const files = v.files || [];
          const primary = files.find(f => f.primary) || files[0];
          mapProject(v.project_id, primary && primary.filename);
        });
      } catch {}
    }
    // CurseForge：murmur2 指纹精确匹配
    const murms = modDL.installedFiles.map(f => f.murmur2).filter(Boolean);
    if (murms.length) {
      try {
        const r = await api('/moddb/version-files', { method: 'POST', body: { source: 'curseforge', hashes: murms } });
        Object.values(r || {}).forEach(v => { if (v && v.project_id) mapProject(v.project_id, v.filename); });
      } catch {}
    }
    modDL.installed = found;
    modDL.installedByProject = byProject;
    if (modDL.results) renderModResults();
  } catch {}
}
// 文件名干（去 .jar / .jar.disabled 后缀并转小写）
function fileStem(name) {
  let n = (name || '').toLowerCase();
  if (n.endsWith('.jar.disabled')) n = n.slice(0, -'.jar.disabled'.length);
  else if (n.endsWith('.jar')) n = n.slice(0, -'.jar'.length);
  return n;
}
// 已安装判定：哈希/指纹精确匹配，或名称与已装文件名前缀吻合（手动安装的模组）
function modNameInstalled(name) {
  if (!name || !modDL.installedNames) return false;
  const key = String(name).toLowerCase().replace(/[\s_]+/g, '-');
  if (!key) return false;
  for (const n of modDL.installedNames) {
    if (n === key || n.startsWith(key + '-')) return true;
  }
  return false;
}
function modInstalled(m) {
  return modDL.installed.has(m.id) || modNameInstalled(m.slug);
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
      const installedExact = modDL.installed.has(m.id);
      const installed = installedExact || modNameInstalled(m.slug);
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
      const dis = blocked ? `disabled title="纯客户端模组（或标注不支持服务端），服务器无需下载"`
        : inQueue ? `disabled title="已在下载队列中（可展开版本列表更换版本）"` : '';
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
      const sameFile = modDL.installedNames && modDL.installedNames.has(fileStem(v.filename));
      return `<div class="row between" style="padding:6px 0;border-bottom:1px solid var(--border)">
      <div><span class="mono small"><b>${esc(v.name)}</b> · ${esc(v.filename)}</span> <span class="muted small">${esc((v.date || '').slice(0, 10))}</span>
        ${sameFile ? '<span class="pill st-running" style="font-size:10px;padding:1px 8px">当前版本</span>' : ''}
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
  // Prism 式替换语义：已安装的模组选择其他版本时，下载完成后自动删除旧文件
  const replaceFiles = ((modDL.installedByProject || {})[m.id] || [])
    .filter(f => f !== item.filename);
  const result = await queuePush({
    source: modDL.source,
    projectId: m.id,
    projectName: m.name,
    name: item.name,
    filename: item.filename,
    url: item.url,
    sha1: item.sha1 || '',
    replaceFiles,
    dependencies: item.dependencies || [],
    dep: !!dep,
  }, !!dep);
  if (!dep) {
    if (result === 'added') toast(`已加入队列：${item.name}`);
    else if (result === 'replaced') toast(`已更换版本：${item.name}`);
    else if (result === false) toast('该模组正在下载中，请稍候', false);
  }
  renderModResults();
  renderModQueue();
}
async function queuePush(item, dep) {
  // 同一项目（同来源）只保留一个队列项：重复加入视为更换版本（尚未开始下载的才可更换）
  const exist = modDL.queue.find(q =>
    q.source === item.source && q.projectId && q.projectId === item.projectId);
  if (exist) {
    if (exist.status === 'downloading') return false;
    Object.assign(exist, item);
    renderModQueue();
    if (modDL.results) renderModResults();
    return 'replaced';
  }
  if (modDL.queue.some(q => q.filename === item.filename)) return 'added';
  modDL.queue.push(item);
  renderModQueue();
  // 自动解析前置依赖（仅 Modrinth 提供依赖信息）
  if (item.source !== 'modrinth' || !item.dependencies) return 'added';
  for (const d of item.dependencies) {
    if (d.dependency_type !== 'required') continue;
    if (modDL.queue.some(q => q.projectId === d.project_id)) continue;
    if (modDL.installed.has(d.project_id)) continue;
    let v = null;
    try {
      const dv = await api(`/moddb/versions?source=modrinth&project=${encodeURIComponent(d.project_id)}&game=${encodeURIComponent(modDL.game)}&loader=${encodeURIComponent(modDL.loader)}`);
      v = dv.versions.find(x => x.environment !== 'client_only');
    } catch { continue; }
    if (!v) continue;
    // 前置依赖的最新版文件已存在（同名文件）则无需重复安装
    if (modDL.installedNames && modDL.installedNames.has(fileStem(v.filename))) continue;
    await queuePush({
      source: 'modrinth',
      projectId: d.project_id,
      projectName: v.filename,
      name: v.name,
      filename: v.filename,
      url: v.url,
      sha1: v.sha1 || '',
      dependencies: v.dependencies || [],
      dep: true,
    }, true);
    toast(`已自动添加前置依赖：${v.filename}`);
  }
  return 'added';
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
        <span class="muted small mono">${esc(q.filename)}</span>
        ${q.replaceFiles && q.replaceFiles.length && q.status !== 'done' ? `<span class="pill st-warn" title="下载完成后将删除：${esc(q.replaceFiles.join('、'))}">替换 ${q.replaceFiles.length} 个旧文件</span>` : ''}
        ${badge(q)}</span>
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
        body: { url: q.url, filename: q.filename, sha1: q.sha1 || '' },
      });
      q.status = 'done';
      q.size = r.size;
      // 替换语义：下载成功后删除该项目此前安装的旧文件，避免同模组多版本共存
      for (const f of q.replaceFiles || []) {
        if (f === q.filename) continue;
        try {
          await api(`/instances/${modDL.id}/mods/delete`, { method: 'POST', body: { file: f } });
        } catch {}
      }
      q.replaceFiles = [];
    } catch (e) {
      q.status = 'error';
      q.err = e.message;
    }
    renderModQueue();
  }
  const ok = modDL.queue.filter(q => q.status === 'done').length;
  const bad = modDL.queue.filter(q => q.status === 'error').length;
  toast(`批量下载完成：成功 ${ok}，失败 ${bad}`, bad === 0);
  // 刷新已安装标记与文件名集合，避免再次搜索时误判
  loadInstalledMods();
  // 实例运行中才提示重启
  try {
    const s = await api(`/instances/${modDL.id}/status`);
    if (s.status !== 'stopped') {
      const rb = $('#md-restart');
      if (rb) rb.style.display = '';
    }
  } catch {}
}
/* ---------------- 备份 ---------------- */
async function renderTabGameBackups(id, el, t) {
  el.innerHTML = `<div class="row between"><h2>游戏内备份</h2><button class="btn" onclick="loadGameBackups('${id}',routeToken)">刷新</button></div><div id="gb-body"><div class="empty">加载中…</div></div>`;
  await loadGameBackups(id, t);
}
async function loadGameBackups(id, t, resumeJob = true) {
  try {
    const d = await api(`/instances/${id}/game-backups`);
    if (t !== routeToken) return;
    const target = $('#gb-body'); if (!target) return;
    const provider = d.provider;
    target.innerHTML = `${provider ? `<div class="card"><b>ServerUtilities ${esc(provider.version)}</b><p class="muted small">${provider.enabled ? '已启用' : '未启用'}；自动保留最近 ${provider.keep} 份，备份目录 ${esc(provider.backup_dir)}。${provider.interval_hours ? `自动间隔 ${provider.interval_hours} 小时。` : ''}${provider.need_online_players ? '仅在线玩家数满足条件时自动备份。' : ''}${provider.only_claimed ? '仅备份已认领区域。' : ''}</p></div>` : '<div class="banner warn">当前实例未识别到支持的游戏内备份模组；目前支持 ServerUtilities。</div>'}
      ${provider && (!provider.enabled || !provider.command_enabled) ? `<div class="banner warn">${provider.enabled ? 'backup 命令已禁用，无法立即备份。' : 'ServerUtilities 配置已禁用，无法创建游戏内备份。'}</div>` : ''}
      <p class="muted small">立即备份需要实例完成启动；恢复需要先停止实例。恢复前会保留当前受影响的数据，完成后请手动启动。</p>
      <div class="row between" style="margin:12px 0"><span>状态：${statusPill(d.status)}${d.active_job ? ' · 任务进行中' : ''}</span><button id="gb-create" class="btn primary" ${!provider || !provider.enabled || !provider.command_enabled || d.status !== 'running' || d.active_job ? 'disabled' : ''} onclick="startGameBackup('${id}')">立即备份</button></div>
      <div id="gb-job"></div>${d.backups?.length ? `<div class="table-wrap"><table class="table"><thead><tr><th>名称</th><th>大小</th><th>创建时间</th><th>操作</th></tr></thead><tbody>${d.backups.map(b => {
        const n = encodeURIComponent(b.name).replace(/'/g, '%27');
        return `<tr><td class="mono">${esc(b.name)}${b.problem ? `<div class="muted small">${esc(b.problem)}</div>` : ''}</td><td>${fmtSize(b.size)}</td><td>${esc(b.created)}</td><td><button class="btn small" onclick="downloadGameBackup('${id}','${n}')">下载</button> <button class="btn small" ${b.problem ? 'disabled' : ''} onclick="previewGameBackup('${id}','${n}')">预览</button> <button class="btn small warn" ${b.problem || d.status !== 'stopped' || d.active_job ? 'disabled' : ''} onclick="restoreGameBackup('${id}','${n}')">恢复</button></td></tr>`;
      }).join('')}</tbody></table></div>` : '<div class="empty">暂无游戏内备份</div>'}`;
    if (resumeJob && (d.active_job || d.last_job)) watchGameBackupJob(d.active_job || d.last_job, t, id, !!d.active_job);
  } catch (e) { if (t === routeToken && $('#gb-body')) $('#gb-body').innerHTML = `<div class="empty">加载失败：${esc(e.message)}</div>`; }
}
async function startGameBackup(id) {
  const t = routeToken, button = $('#gb-create');
  if (button) button.disabled = true;
  try {
    const j = await api(`/instances/${id}/game-backups`, {method:'POST'});
    if (t === routeToken) watchGameBackupJob(j.job_id, t, id);
  } catch(e) { toast(e.message,false); if (t === routeToken) loadGameBackups(id, t); }
}
async function downloadGameBackup(id,name) { try { const r=await fetch(`/api/instances/${id}/game-backups/${name}`,{headers:headers()}); if(!r.ok)throw new Error(r.statusText); const a=document.createElement('a'); a.href=URL.createObjectURL(await r.blob()); a.download=decodeURIComponent(name); a.click(); URL.revokeObjectURL(a.href); } catch(e) { toast(e.message,false); } }
async function previewGameBackup(id,name) { try { const d=await api(`/instances/${id}/game-backups/${name}/preview`),p=d.preview; showModal(`<h2>游戏内备份预览</h2><p>${p.files} 个文件 · ${fmtSize(p.total_size)}</p><p>世界：${esc(p.world||'未知')}；包含：${(p.roots||[]).map(esc).join('、')}</p><p>恢复前副本目录：${esc(p.recovery_directory)}</p><div class="banner warn">恢复会使世界回退到备份时间点。恢复前会保留现有受影响的数据。</div><div class="row right"><button class="btn" onclick="closeModal()">关闭</button><button class="btn warn" ${currentInstanceInfo?.status!=='stopped'?'disabled':''} onclick="restoreGameBackup('${id}','${name}')">继续恢复</button></div>`); } catch(e) { toast(e.message,false); } }
async function restoreGameBackup(id,name) {
  if(currentInstanceInfo?.status!=='stopped') return toast('恢复要求实例已停止',false);
  const t = routeToken;
  try {
    const d=await api(`/instances/${id}/game-backups/${name}/preview`), p=d.preview;
    if(t !== routeToken) return;
    if(!await appConfirm(`备份：${decodeURIComponent(name)}。世界 ${p.world||'未知'} 将回退；原数据保留于 ${p.recovery_directory}。确认恢复？`,{title:'确认世界回退',okText:'恢复',danger:true})) return;
    const j=await api(`/instances/${id}/game-backups/${name}/restore`,{method:'POST'});
    closeModal();
    if(t === routeToken) watchGameBackupJob(j.job_id,t,id);
  } catch(e) { toast(e.message,false); }
}
function watchGameBackupJob(id, t, instanceId, refreshOnComplete = true) {
  const poll = async () => {
    if(t !== routeToken) return;
    try {
      const j = await api(`/jobs/${id}`);
      if(t !== routeToken) return;
      const logs = Array.isArray(j.logs) ? j.logs : [];
      const terminal = ['done','error','failed','cancelled'].includes(j.status);
      const html = `<div class="card"><b>任务：${esc(j.status)}</b><div class="job-log">${logs.map(esc).join('\n')||'暂无任务日志'}</div>${['error','failed'].includes(j.status)?'<div class="banner warn">操作失败，请检查任务日志和实例数据状态。</div>':''}</div>`;
      if(terminal && refreshOnComplete) await loadGameBackups(instanceId,t,false);
      if(t !== routeToken) return;
      const box = $('#gb-job'); if(box) box.innerHTML = html;
      if(!terminal) { const timer=setTimeout(poll,1500); timers.push(timer); }
    } catch(e) {
      if(t !== routeToken) return;
      const box = $('#gb-job'); if(box) box.innerHTML = `<div class="banner warn">任务状态查询失败：${esc(e.message)}</div>`;
    }
  };
  poll();
}
async function renderTabBackups(id, el, t) {
  el.innerHTML = `
    <div class="row between"><h2>备份</h2>
      <button class="btn primary" id="bk-go" onclick="createBackup('${id}')">⬆ 立即备份</button></div>
    <p class="muted small">全量 tar.gz 备份（运行中会先自动 save-all 落盘）；保留策略：最多 10 份 / 30 天，超出自动清理。恢复前请先停止实例，恢复会覆盖实例目录中的现有文件。</p>
    <div id="bk-body"><div class="empty">加载中…</div></div>`;
  await loadBackups(id, t);
}
async function loadBackups(id, t) {
  if (t !== undefined && t !== routeToken) return;
  try {
    const d = await api(`/instances/${id}/backups`);
    if (t !== undefined && t !== routeToken) return;
    $('#bk-body').innerHTML = d.backups.length ? `<div class="table-wrap"><table class="table">
      <thead><tr><th>备份文件</th><th>大小</th><th>创建时间</th><th>操作</th></tr></thead>
      <tbody>${d.backups.map(b => `<tr>
        <td class="mono">${esc(b.name)}</td>
        <td class="muted">${fmtSize(b.size)}</td>
        <td class="muted">${esc(b.created)}</td>
        <td>
          <button class="btn small" onclick="downloadBackup('${id}','${esc(b.name)}')">下载</button>
          <button class="btn small warn" onclick="restoreBackup('${id}','${esc(b.name)}')">恢复</button>
          <button class="btn small danger" onclick="deleteBackup('${id}','${esc(b.name)}')">删除</button>
        </td></tr>`).join('')}</tbody></table></div>`
      : '<div class="empty">暂无备份，点右上角「立即备份」创建</div>';
  } catch (e) {
    $('#bk-body').innerHTML = `<div class="empty">${esc(e.message)}</div>`;
  }
}
async function createBackup(id) {
  const btn = $('#bk-go');
  if (btn) { btn.disabled = true; btn.textContent = '备份中…'; }
  try {
    const r = await api(`/instances/${id}/backups`, { method: 'POST' });
    toast(`备份完成：${r.name}`);
    await loadBackups(id);
  } catch (e) { toast(e.message, false); }
  if (btn) { btn.disabled = false; btn.textContent = '⬆ 立即备份'; }
}
async function downloadBackup(id, name) {
  try {
    const r = await fetch(`/api/instances/${id}/backups/${encodeURIComponent(name)}/download`, { headers: headers() });
    if (!r.ok) throw new Error((await r.json().catch(() => ({}))).error || r.statusText);
    const blob = await r.blob();
    const a = document.createElement('a');
    a.href = URL.createObjectURL(blob);
    a.download = name;
    a.click();
    URL.revokeObjectURL(a.href);
  } catch (e) { toast(e.message, false); }
}
async function restoreBackup(id, name) {
  try {
    const p = await api(`/instances/${id}/backups/${encodeURIComponent(name)}/preview`);
    const pv = p.preview;
    const html = `
      <p class="small">备份共 <b>${pv.files}</b> 个文件（${fmtSize(pv.total_size)}），包含：<b>${esc(pv.top_entries.join('、'))}</b></p>
      ${pv.overwrite.length ? `<p class="small" style="color:var(--warn)">将覆盖 ${pv.overwrite.length}+ 个现有文件，包括：${esc(pv.overwrite.slice(0, 8).join('、'))}${pv.overwrite.length > 8 ? ' 等' : ''}</p>` : '<p class="small muted">不会覆盖任何现有文件（实例目录为空或无重合）。</p>'}
      <p class="muted small">恢复要求实例处于停止状态；恢复是覆盖式且不可撤销，请确认。</p>`;
    showModal(`<h2>恢复备份</h2><div style="margin:10px 0">${html}</div>
      <div class="row right"><button class="btn ghost" onclick="closeModal()">取消</button>
      <button class="btn warn" onclick="doRestore('${id}','${esc(name)}')">确认恢复</button></div>`);
  } catch (e) { toast(e.message, false); }
}
async function doRestore(id, name) {
  try {
    await api(`/instances/${id}/backups/${encodeURIComponent(name)}/restore`, { method: 'POST' });
    closeModal();
    toast('恢复完成');
    refresh();
  } catch (e) { toast(e.message, false); }
}
async function deleteBackup(id, name) {
  if (!(await appConfirm(`确定删除备份 ${name}？`, { danger: true, okText: '删除' }))) return;
  try {
    await api(`/instances/${id}/backups/${encodeURIComponent(name)}`, { method: 'DELETE' });
    toast('已删除');
    loadBackups(id);
  } catch (e) { toast(e.message, false); }
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
    $('#files-body').innerHTML = `<div class="table-wrap"><table class="table">
      <thead><tr><th>名称</th><th>大小</th><th>修改时间</th><th>操作</th></tr></thead>
      <tbody>${entries.map(f => `<tr>
        <td>${f.dir ? '📁' : '📄'} <a onclick="${f.dir
          ? `loadFiles('${id}',${t},'${esc(full(f))}')`
          : `editFile('${id}','${esc(full(f))}')`}">${esc(f.name)}</a></td>
        <td class="muted">${f.dir ? '-' : fmtSize(f.size)}</td>
        <td class="muted">${esc(f.modified)}</td>
        <td>
          <button class="btn small" onclick="${f.dir ? `downloadArchive('${id}','${esc(full(f))}')` : `downloadFile('${id}','${esc(full(f))}')`}">下载</button>
          <button class="btn small" onclick="renameFile('${id}','${esc(full(f))}','${esc(f.name)}')">重命名</button>
          <button class="btn small danger" onclick="deleteFile('${id}','${esc(full(f))}')">删除</button>
        </td></tr>`).join('') || '<tr><td colspan="4" class="muted">空目录</td></tr>'}</tbody></table></div>`;
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
  const name = await appPrompt('输入新文件夹名称', '', { title: '新建文件夹' });
  if (!name) return;
  try {
    await api(`/instances/${id}/files/mkdir`, { method: 'POST', body: { path: curPath ? curPath + '/' + name : name } });
    toast('已创建'); loadFiles(id, routeToken, curPath);
  } catch (e) { toast(e.message, false); }
}
async function renameFile(id, path, oldName) {
  const nn = await appPrompt('输入新名称', oldName, { title: '重命名' });
  if (!nn || nn === oldName) return;
  const parent = path.includes('/') ? path.slice(0, path.lastIndexOf('/')) : '';
  const to = parent ? parent + '/' + nn : nn;
  try {
    await api(`/instances/${id}/files/rename`, { method: 'POST', body: { from: path, to } });
    toast('已重命名'); loadFiles(id, routeToken, curPath);
  } catch (e) { toast(e.message, false); }
}
async function deleteFile(id, path) {
  if (!(await appConfirm(`确定删除 ${path}？\n如果是目录将被整个删除！`, { danger: true, okText: '删除' }))) return;
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
  $('#props-body').innerHTML = rows.length ? `<div class="table-wrap"><table class="table props">
    <thead><tr><th>配置项</th><th>值</th></tr></thead>
    <tbody>${rows.map(({ e, i }) => {
      const meta = propMetaFor(e.key, e.value);
      return `<tr>
        <td class="mono"><span class="tag t-${meta.cat}">${PROP_CATS[meta.cat]}</span>${esc(e.key)}
          <div class="muted small">${esc(meta.desc)}</div></td>
        <td>${propControl(e, i)}</td></tr>`;
    }).join('')}</tbody></table></div>`
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
        </div>
        <select id="java-picker" style="margin-top:6px" onchange="if(this.value){document.getElementById('f-java').value=this.value;detectJava();}">
          <option value="">加载缓存…</option>
        </select>
        <div class="muted small">从下拉列表选择本机扫描或面板安装的 Java；也可手动填写路径。安装 / 扫描请前往「面板设置」。</div>
        <div id="java-hint" class="muted small mono"></div>
      </label>
      <label>最小内存 (MB)<input id="f-min" type="number" min="512" step="512" value="${s.min_ram_mb}">
        <div class="muted small">JVM 初始堆大小（-Xms），一般与最大内存设为一致</div></label>
      <label>最大内存 (MB)<input id="f-max" type="number" min="512" step="512" value="${s.max_ram_mb}">
        <div class="muted small">JVM 最大堆大小（-Xmx）：原版服 2048~4096，模组服建议 6144 以上</div></label>
      <label class="full">JVM / 启动参数（空格分隔，支持 @argfile）<input id="f-jvm" value="${esc(s.jvm_args || '')}" placeholder="-XX:+UseG1GC -XX:ParallelGCThreads=4 -Dfile.encoding=UTF-8">
        <div class="muted small">追加在 -Xms/-Xmx 之后的 JVM 参数；主程序 JAR 为空时，这里就是完整的启动参数（支持 @user_jvm_args.txt 等 argfile 写法）。</div></label>
      <label>JVM 参数模板<div class="row"><button class="btn ghost small" onclick="fillAikar()">填入 Aikar's Flags</button></div></label>
      <label class="check"><input type="checkbox" id="f-auto" ${s.auto_restart ? 'checked' : ''}> 进程异常退出时自动重启（5 秒后）</label>
      <label class="check"><input type="checkbox" id="f-autoboot" ${s.auto_start_on_boot ? 'checked' : ''}> 面板启动时自动运行此实例（多个实例将间隔 5 秒依次拉起）</label>
      <div class="row right"><button class="btn primary" onclick="saveInstance('${id}')">保存设置</button></div>
    </div>
    <div class="card" style="margin-top:16px">
      <div class="row between"><h2 style="margin:0">告警推送（面板级）</h2><span class="muted small">实例异常退出 / 计划任务连败 / 磁盘不足时推送</span></div>
      <div class="form" style="margin-top:10px">
        <label>推送方式<select id="al-type">
          <option value="none">不推送</option>
          <option value="webhook">Webhook（POST JSON）</option>
          <option value="discord">Discord Webhook</option>
          <option value="telegram">Telegram Bot</option>
        </select></label>
        <label class="full">Webhook 地址<input id="al-url" placeholder="https://…">
          <div class="muted small">Discord 直接粘贴频道 Webhook URL。</div></label>
        <label>Telegram Bot Token<input id="al-tgt" placeholder="${window.__alertTgtSet ? '已设置（留空保持不变）' : '留空表示未设置'}"></label>
        <label>Telegram Chat ID<input id="al-tgc" value="${esc(window.__alertChatId || '')}" placeholder="例如 123456789"></label>
        <div class="row right"><button class="btn primary" onclick="saveAlerts('${id}')">保存告警设置</button></div>
      </div>
    </div>`;
  loadCachedJavas();
  loadConfigsList(id);
  // 回填告警设置（面板级；token 脱敏只显示是否已设置）
  api('/settings').then(c => {
    const t = document.getElementById('al-type');
    if (t) t.value = c.alert_type || 'none';
    const u = document.getElementById('al-url');
    if (u) u.value = c.alert_webhook_url || '';
    window.__alertTgtSet = !!c.telegram_bot_token_set;
    const tg = document.getElementById('al-tgt');
    if (tg) tg.placeholder = c.telegram_bot_token_set ? '已设置（留空保持不变）' : '留空表示未设置';
    const ci = document.getElementById('al-tgc');
    if (ci) ci.value = c.telegram_chat_id || '';
  }).catch(() => {});
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
function neededJavaMajor(v) {
  if (!v) return null;
  const p = String(v).split(/[.\-]/).map(Number);
  if (!p.length || isNaN(p[0])) return null;
  if (p[0] !== 1) return 25; // 年份制（26.x+）需要 Java 25
  const mi = p[1] || 0, pa = p[2] || 0;
  if (mi >= 21 || (mi === 20 && pa >= 5)) return 21;
  if (mi >= 17) return 17;
  return 8;
}
async function autoMatchJava() {
  const mc = currentInstanceInfo?.mc_version;
  const major = neededJavaMajor(mc);
  if (!major) return toast('未记录 MC 版本，无法自动匹配（可在创建实例时选择版本）', false);
  const d = await api('/javas');
  const match = (d.javas || []).filter(j => j.major === major).sort((a, b) => b.major - a.major)[0];
  if (!match) return toast(`本机没有 Java ${major}（${mc} 需要），可用上方按钮一键安装`, false);
  $('#f-java').value = match.path;
  detectJava();
  toast(`已选择 ${match.path}`);
}
async function installJava(major) {
  const log = $('#ji-log');
  if (log) { log.style.display = ''; log.textContent = `安装 Temurin JRE ${major}…`; }
  try {
    const r = await api(`/java-install/${major}`, { method: 'POST' });
    await pollJob(r.job_id, j => {
      if (log) {
        log.textContent = j.logs.join('\n');
        log.scrollTop = 1e6;
      }
    });
    toast(`Java ${major} 安装完成`);
    await loadCachedJavas();
  } catch (e) {
    toast(`安装失败: ${e.message}`, false);
    if (log) log.textContent += `\n[错误] ${e.message}`;
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
async function renderPanelSettings(t = ++routeToken) {
  const c = await api('/settings');
  if (t !== routeToken) return;
  $('#main').innerHTML = `<h1>面板设置</h1>
    <div class="form card">
      <label>监听地址<input id="ps-listen" value="${esc(c.listen)}" placeholder="127.0.0.1:8080">
        <div class="muted small">格式 IP:端口；127.0.0.1 仅本机访问，0.0.0.0 对局域网开放。修改后需重启面板生效。</div></label>
      <label>访问令牌<input id="ps-token" value="" placeholder="${c.token_set ? '已设置（留空保持不变）' : '留空则无需鉴权'}">
        <div class="muted small">设置后所有 API 与 WebSocket 控制台都需要令牌，保存后立即生效；对外暴露时建议设置。为安全起见已保存的令牌不回显，留空保存即保持不变。</div></label>
      <label>CurseForge API Key<input id="ps-cfkey" value="" placeholder="${c.curseforge_api_key_set ? '已设置（留空保持不变）' : '留空则模组下载仅支持 Modrinth'}">
        <div class="muted small">用于「模组下载」中 CurseForge 的搜索与文件列表；在 console.curseforge.com 可免费创建。已保存的 Key 不回显，留空保存即保持不变。</div></label>
      <label class="full">数据目录<input id="ps-dir" value="${esc(c.data_dir)}">
        <div class="muted small">实例存放的根目录（相对路径基于面板工作目录），重启面板后生效。</div></label>
      <div class="row right"><button class="btn primary" onclick="savePanelSettings()">保存</button></div>
    </div>
    <div class="card" style="margin-top:16px">
      <div class="row between"><h2 style="margin:0">Java 环境</h2>
        <div class="row">
          <button class="btn small" onclick="rescanJavasPanel()">扫描本机 Java</button>
          <button class="btn small ghost" onclick="installJavaPanel(8)">安装 JRE 8</button>
          <button class="btn small ghost" onclick="installJavaPanel(11)">安装 JRE 11</button>
          <button class="btn small ghost" onclick="installJavaPanel(17)">安装 JRE 17</button>
          <button class="btn small ghost" onclick="installJavaPanel(21)">安装 JRE 21</button>
          <button class="btn small ghost" onclick="installJavaPanel(25)">安装 JRE 25</button>
        </div></div>
      <div class="muted small" style="margin-top:8px">扫描会查找 PATH、Program Files、Prism Launcher / MultiMC、.jdks 等常见位置的 Java，结果持久化保存；安装按钮从 Adoptium 下载 Temurin JRE 到面板数据目录。各实例在「实例设置 → Java 路径」下拉框中选择。</div>
      <div id="java-panel-list" style="margin-top:12px;max-height:340px;overflow-y:auto"><div class="empty">加载中…</div></div>
      <pre id="ji-log" class="console-box" style="display:none;margin-top:12px;max-height:220px"></pre>
    </div>
    <div class="card" style="margin-top:16px">
      <div class="row between"><h2 style="margin:0">审计日志</h2>
        <div class="row">
          <input id="audit-q" placeholder="筛选路径 / 方法 / 状态码" style="width:230px" onkeydown="if(event.key==='Enter')loadAudit()">
          <button class="btn small" onclick="loadAudit()">刷新</button>
        </div></div>
      <div id="audit-body" style="margin-top:12px;max-height:340px;overflow-y:auto"><div class="empty">加载中…</div></div>
      <p class="muted small">记录所有写操作与失败请求（≥400），token / password / key 参数自动脱敏；内存保留最近 5000 条，全量写入 data/audit.log。</p>
    </div>`;
  await loadAudit();
  loadPanelJavas();
}
async function loadAudit() {
  const el = $('#audit-body');
  if (!el) return;
  try {
    const q = $('#audit-q')?.value.trim() || '';
    const d = await api(`/audit?limit=200&q=${encodeURIComponent(q)}`);
    const list = d.entries || [];
    el.innerHTML = list.length ? `<div class="table-wrap"><table class="table"><thead><tr><th>时间</th><th>方法</th><th>路径</th><th>状态码</th></tr></thead><tbody>` +
      list.map(e => `<tr>
        <td class="muted mono small">${esc(e.ts)}</td>
        <td class="mono small">${esc(e.method)}</td>
        <td class="mono small">${esc(e.path)}</td>
        <td>${e.status < 400 ? '<span class="pill st-running">OK</span>' : `<span class="pill st-stopped">${e.status}</span>`}</td>
      </tr>`).join('') + '</tbody></table></div>'
      : '<div class="empty" style="padding:14px">暂无记录</div>';
  } catch (e) { el.innerHTML = `<div class="empty">${esc(e.message)}</div>`; }
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

function consoleKeydown(e, id) {
  const input = document.getElementById('cmd-input');
  if (!input) return;
  if (e.key === 'Enter') { e.preventDefault(); const b = document.getElementById('cmd-send'); b && b.click(); return; }
  if (e.key === 'ArrowUp') {
    e.preventDefault();
    if (cmdHistIdx > 0) { cmdHistIdx--; input.value = cmdHistory[cmdHistIdx] || ''; }
    return;
  }
  if (e.key === 'ArrowDown') {
    e.preventDefault();
    if (cmdHistIdx < cmdHistory.length - 1) { cmdHistIdx++; input.value = cmdHistory[cmdHistIdx] || ''; }
    else { cmdHistIdx = cmdHistory.length; input.value = ''; }
    return;
  }
  if (e.key === 'Tab') {
    e.preventDefault();
    const text = input.value;
    const parts = text.split(' ');
    const last = parts[parts.length - 1].toLowerCase();
    if (parts.length === 1) {
      const cmds = ['list', 'say ', 'op ', 'deop ', 'kick ', 'ban ', 'ban-ip ', 'pardon ', 'pardon-ip ', 'whitelist ', 'stop', 'save-all', 'tps', 'difficulty ', 'gamemode ', 'time set ', 'weather '];
      const hit = cmds.find(c => c.startsWith(last) && last);
      if (hit !== undefined) input.value = hit;
      return;
    }
    // 玩家名补全（在线玩家）
    fetch('/api/instances/' + id + '/users', { headers: headers() })
      .then(r => r.json())
      .then(d => {
        const pool = (d.online || []).map(p => p).filter(n => n.toLowerCase().startsWith(last));
        if (pool.length) { parts[parts.length - 1] = pool[0]; input.value = parts.join(' '); }
      }).catch(() => {});
  }
}

/* ---------------- 世界管理 ---------------- */
async function renderTabWorlds(id, el, t) {
  el.innerHTML = `<div class="row between"><h2>世界管理</h2>
    <div class="row"><button class="btn" onclick="createWorld('${id}')">新建世界</button></div></div>
    <p class="muted small">列出实例目录中所有包含 level.dat 的存档。切换/创建/删除需要服务器停止。</p>
    <div id="worlds-body"><div class="empty">加载中…</div></div>`;
  await loadWorlds(id, t);
}
async function loadWorlds(id, t) {
  try {
    const d = await api(`/instances/${id}/worlds`);
    if (t !== routeToken) return;
    $('#worlds-body').innerHTML = d.worlds.length ? `<div class="table-wrap"><table class="table">
      <thead><tr><th>世界</th><th>大小</th><th>状态</th><th>操作</th></tr></thead>
      <tbody>${d.worlds.map(w => `<tr>
        <td><b>${esc(w.name)}</b></td><td>${fmtSize(w.size)}</td>
        <td>${w.current ? '<span class="pill st-running">当前</span>' : '<span class="muted">-</span>'}</td>
        <td>${w.current ? '<span class="muted small">使用中</span>' :
          `<button class="btn small" onclick="switchWorld('${id}','${esc(w.name)}')">切换</button>
           <button class="btn small danger" onclick="deleteWorld('${id}','${esc(w.name)}')">删除</button>`}</td>
      </tr>`).join('')}</tbody></table></div>` : '<div class="empty">暂无世界存档</div>';
  } catch (e) { $('#worlds-body').innerHTML = `<div class="empty">${esc(e.message)}</div>`; }
}
async function switchWorld(id, name) {
  try { await api(`/instances/${id}/worlds/switch`, { method: 'POST', body: { path: name } }); toast(`已切换到 ${name}`); loadWorlds(id); } catch (e) { toast(e.message, false); }
}
async function deleteWorld(id, name) {
  if (!(await appConfirm(`确定删除世界「${name}」？不可恢复！`, { danger: true, okText: '删除' }))) return;
  try { await api(`/instances/${id}/worlds/delete`, { method: 'POST', body: { path: name } }); toast('已删除'); loadWorlds(id); } catch (e) { toast(e.message, false); }
}
async function createWorld(id) {
  const name = await appPrompt('新世界名称（不含空格）', '', { title: '新建世界' });
  if (!name) return;
  const seed = await appPrompt('世界种子（留空为随机）', '', { title: '世界种子' });
  try { await api(`/instances/${id}/worlds/create`, { method: 'POST', body: { name, seed: seed || undefined } }); toast(`世界 ${name} 已创建（设为当前）`); loadWorlds(id); } catch (e) { toast(e.message, false); }
}

/* ---------------- 计划任务 ---------------- */
async function renderTabTasks(id, el, t) {
  el.innerHTML = `
    <div class="row between"><h2>计划任务</h2>
      <button class="btn" onclick="createTask('${id}')">＋ 新建任务</button></div>
    <p class="muted small">支持三种类型：执行命令 / 备份 / 重启。到期自动执行并记录结果；连续失败 3 次推送告警。</p>
    <div id="tasks-body"><div class="empty">加载中…</div></div>`;
  await loadTasks(id, t);
}
async function loadTasks(id, t) {
  try {
    const d = await api(`/instances/${id}/tasks`);
    if (t !== routeToken) return;
    const tasks = d.tasks || [];
    $('#tasks-body').innerHTML = tasks.length ? `<div class="table-wrap"><table class="table">
      <thead><tr><th>任务</th><th>类型</th><th>间隔</th><th>状态</th><th>上次结果</th><th>操作</th></tr></thead>
      <tbody>${tasks.map(x => `<tr>
        <td><b>${esc(x.name)}</b>${x.value ? `<div class="muted small mono">${esc(x.value)}</div>` : ''}</td>
        <td>${x.kind === 'command' ? '命令' : x.kind === 'backup' ? '备份' : '重启'}</td>
        <td>${x.interval_mins} 分钟</td>
        <td>${x.enabled ? '<span class="pill st-running">启用</span>' : '<span class="pill st-stopped">停用</span>'}
            ${x.consecutive_failures >= 3 ? '<span class="pill st-warn">连败</span>' : ''}</td>
        <td class="muted small">${esc(x.last_result || '-')}</td>
        <td>
          <button class="btn small" onclick="taskOp('${id}','${x.id}','run')">立即运行</button>
          <button class="btn small ${x.enabled ? 'warn' : 'primary'}" onclick="taskOp('${id}','${x.id}','${x.enabled ? 'disable' : 'enable'}')">${x.enabled ? '停用' : '启用'}</button>
          <button class="btn small danger" onclick="taskOp('${id}','${x.id}','delete')">删除</button>
        </td></tr>`).join('')}</tbody></table></div>`
      : '<div class="empty">暂无计划任务，点右上角「新建任务」创建</div>';
  } catch (e) { $('#tasks-body').innerHTML = `<div class="empty">${esc(e.message)}</div>`; }
}
async function createTask(id) {
  showModal(`<h2>新建计划任务</h2>
    <label>任务名称<input id="tk-name" placeholder="例如：定时备份"></label>
    <label>类型<select id="tk-kind"><option value="command">执行命令</option><option value="backup">备份</option><option value="restart">重启服务器</option></select></label>
    <label>命令内容<input id="tk-value" placeholder="例如 say hello（仅命令类型需要）"></label>
    <label>间隔（分钟）<input id="tk-interval" type="number" value="60" min="1"></label>
    <div class="row right"><button class="btn ghost" onclick="closeModal()">取消</button><button class="btn primary" onclick="doCreateTask('${id}')">创建</button></div>`);
}
async function doCreateTask(id) {
  const name = $('#tk-name')?.value.trim();
  const kind = $('#tk-kind')?.value;
  const value = $('#tk-value')?.value.trim() || '';
  const interval = +($('#tk-interval')?.value) || 0;
  if (!name) return toast('请输入名称', false);
  if (interval <= 0) return toast('间隔必须大于 0', false);
  try {
    await api(`/instances/${id}/tasks`, { method: 'POST', body: { name, kind, value, interval_mins: interval } });
    closeModal(); toast('任务已创建'); route();
  } catch (e) { toast(e.message, false); }
}
async function taskOp(id, taskId, op) {
  if (op === 'delete' && !(await appConfirm('确定删除此计划任务？', { danger: true, okText: '删除' }))) return;
  try { await api(`/instances/${id}/tasks/update`, { method: 'POST', body: { id: taskId, op } }); toast('已操作'); route(); } catch (e) { toast(e.message, false); }
}

/* ---------------- 文件下载 / 打包 / 图标 / 克隆 / Aikar / 告警 / 导入导出 ---------------- */
async function batchStart() {
  const ids = $$('.dash-check:checked').map(c => c.dataset.id);
  if (!ids.length) return toast('请先勾选实例', false);
  for (const x of ids) { try { await api(`/instances/${x}/start`, { method: 'POST' }); } catch {} }
  toast(`已启动 ${ids.length} 个实例`); refresh();
}
async function batchStop() {
  const ids = $$('.dash-check:checked').map(c => c.dataset.id);
  if (!ids.length) return toast('请先勾选实例', false);
  for (const x of ids) { try { await api(`/instances/${x}/stop`, { method: 'POST' }); } catch {} }
  toast(`已停止 ${ids.length} 个实例`); refresh();
}
async function cloneInstance(id, name) {
  const nn = await appPrompt('输入克隆后的实例名称', name + ' 副本', { title: '克隆实例' });
  if (!nn) return;
  try {
    const r = await api(`/instances/${id}/clone`, { method: 'POST', body: { name: nn } });
    toast(`克隆完成，新端口 ${r.port}`); refresh();
  } catch (e) { toast(e.message, false); }
}
function fillAikar() {
  const el = $('#f-jvm');
  if (el) el.value = '-XX:+UseG1GC -XX:+ParallelRefProcEnabled -XX:MaxGCPauseMillis=200 -XX:+UnlockExperimentalVMOptions -XX:+DisableExplicitGC -XX:+AlwaysPreTouch -XX:G1NewSizePercent=30 -XX:G1MaxNewSizePercent=40 -XX:G1HeapRegionSize=8M -XX:G1ReservePercent=20 -XX:G1HeapWastePercent=5 -XX:G1MixedGCCountTarget=4 -XX:InitiatingHeapOccupancyPercent=15 -XX:G1MixedGCTargetRatio=4 -XX:G1OldCSetRegionThresholdPercent=5';
}
async function uploadIcon(id) {
  const f = $('#icon-file')?.files[0];
  if (!f) return toast('请选择 PNG 文件', false);
  const fd = new FormData(); fd.append('file', f);
  try { await api(`/instances/${id}/icon`, { method: 'POST', body: fd }); toast('图标已上传'); refresh(); } catch (e) { toast(e.message, false); }
}
async function downloadFile(id, path) {
  const a = document.createElement('a');
  a.href = `/api/instances/${id}/files/download?path=${encodeURIComponent(path)}${TOKEN ? '&token=' + encodeURIComponent(TOKEN) : ''}`;
  a.download = path.split('/').pop(); a.click();
}
async function downloadArchive(id, path) {
  const name = (path.split('/').pop() || 'archive') + '.tar.gz';
  try {
    await api(`/instances/${id}/files/archive`, { method: 'POST', body: { paths: [path], name } });
    const a = document.createElement('a');
    a.href = `/api/instances/${id}/files/archive-download?name=${encodeURIComponent(name)}`;
    a.download = name; a.click();
  } catch (e) { toast(e.message, false); }
}
async function saveAlerts(id) {
  var at = document.getElementById('al-type'), au = document.getElementById('al-url'),
      atk = document.getElementById('al-tgt'), ach = document.getElementById('al-tgc');
  try { await api('/settings', { method: 'PUT', body: {
    alert_type: at ? at.value : 'none',
    alert_webhook_url: au ? au.value : '',
    telegram_bot_token: atk ? atk.value : '',
    telegram_chat_id: ach ? ach.value : '',
  }}); toast('告警设置已保存'); } catch (e) { toast(e.message, false); }
}

/* ---------------- 配置导出导入 / 重装 ---------------- */
async function exportConfig() {
  try {
    var d = await api('/config/export');
    var blob = new Blob([JSON.stringify(d, null, 2)], { type: 'application/json' });
    var a = document.createElement('a'); a.href = URL.createObjectURL(blob); a.download = 'mcsp-config.json'; a.click(); URL.revokeObjectURL(a.href);
  } catch (e) { toast(e.message, false); }
}
async function importConfig() {
  var inp = document.getElementById('cfg-file');
  if (!inp || !inp.files[0]) return toast('请选择 JSON 文件', false);
  try {
    var text = await inp.files[0].text();
    var data = JSON.parse(text);
    var r = await api('/config/import', { method: 'POST', body: data });
    toast(r.note || ('已导入 ' + r.instances + ' 个实例配置')); refresh();
  } catch (e) { toast(e.message, false); }
}
async function reinstallServer(id) {
  var t = document.getElementById('ri-type'); var g = document.getElementById('ri-game');
  var l = document.getElementById('ri-lver'); var bk = document.getElementById('ri-backup');
  if (!t || !g || !g.value.trim()) return toast('请填写 MC 版本', false);
  if (!(await appConfirm('确定重装？世界和配置保留，服务端 jar 会被替换。', { danger: true, okText: '重装' }))) return;
  try { await api('/instances/' + id + '/reinstall', { method: 'POST', body: {
    server_type: t.value, mc_version: g.value.trim(), loader_version: l ? l.value.trim() : undefined, backup_first: bk ? bk.checked : true
  }}); toast('重装任务已启动'); } catch (e) { toast(e.message, false); }
}
async function loadConfigsList(id) {
  try {
    var d = await api('/instances/' + id + '/configs');
    var el = document.getElementById('configs-list');
    if (el) el.innerHTML = d.configs.length
      ? d.configs.map(function(c) { return '<button class="btn small ghost" style="margin:2px" onclick="editFile(\'' + id + '\',\'' + esc(c) + '\')">' + esc(c) + '</button>'; }).join('')
      : '<span class="muted small">暂无已知配置文件</span>';
  } catch {}
}

async function loadPanelJavas() {
  try {
    var d = await api('/javas');
    var el = document.getElementById('java-panel-list');
    if (!el) return;
    var javas = d.javas || [];
    el.innerHTML = javas.length
      ? '<div class="table-wrap"><table class="table"><thead><tr><th>版本</th><th>来源</th><th>路径</th></tr></thead><tbody>' +
        javas.map(function(j) { return '<tr><td>Java ' + j.major + '</td><td>' + esc(j.source) + '</td><td class="mono small">' + esc(j.path) + '</td></tr>'; }).join('') +
        '</tbody></table></div>'
      : '<div class="empty" style="padding:14px">尚未扫描，点击上方「扫描本机 Java」</div>';
  } catch (e) {
    var el2 = document.getElementById('java-panel-list');
    if (el2) el2.innerHTML = '<div class="empty">' + esc(e.message) + '</div>';
  }
}
async function rescanJavasPanel() {
  try {
    await api('/javas/scan', { method: 'POST' });
    toast('扫描完成');
    loadPanelJavas();
  } catch (e) { toast(e.message, false); }
}
async function installJavaPanel(major) {
  var log = document.getElementById('ji-log');
  if (log) { log.style.display = ''; log.textContent = '安装 Temurin JRE ' + major + '…'; }
  try {
    var r = await api('/java-install/' + major, { method: 'POST' });
    await pollJob(r.job_id, function(j) {
      if (log) { log.textContent = j.logs.join('\n'); log.scrollTop = 1e6; }
    });
    toast('Java ' + major + ' 安装完成');
    loadPanelJavas();
  } catch (e) { toast(e.message, false); if (log) log.textContent += '\n[错误] ' + e.message; }
}

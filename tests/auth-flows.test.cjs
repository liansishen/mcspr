const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

const source = fs.readFileSync(path.join(__dirname, '../web/app.js'), 'utf8');
const html = fs.readFileSync(path.join(__dirname, '../web/index.html'), 'utf8');
const css = fs.readFileSync(path.join(__dirname, '../web/style.css'), 'utf8');

function extractFn(name) {
  const match = source.match(new RegExp(`(?:async )?function ${name}\\([^)]*\\) \\{[\\s\\S]*?\\n\\}`));
  assert.ok(match, `${name} should exist`);
  return match[0];
}

function run(names, context) {
  const ctx = vm.createContext({ routeToken: 1, sessionGeneration: 1, currentUser: null, pendingApprovals: 0, taskJobs: new Map(), opInFlight: new Map(), ...context });
  vm.runInContext(names.map(extractFn).join('\n'), ctx);
  return ctx;
}

const plain = v => JSON.parse(JSON.stringify(v));
const busy = async (key, control, fn) => fn();

test('api sends a stable X-Operation-ID header on writes only', async () => {
  const calls = [];
  const ctx = run(['api'], {
    csrfToken: 'c', FormData: class {}, onSessionExpired() {},
    newOperationId: () => 'op-auto',
    fetch: async (url, opts) => { calls.push({ url, opts }); return { status: 200, ok: true, headers: { get: () => 'application/json' }, json: async () => ({}) }; },
  });
  await ctx.api('/instances', { method: 'POST', body: { a: 1 } });
  await ctx.api('/stats');
  await ctx.api('/instances/x/start', { method: 'POST', operationId: 'op-fixed' });
  assert.equal(calls[0].opts.headers['X-Operation-ID'], 'op-auto');
  assert.equal(calls[1].opts.headers['X-Operation-ID'], undefined);
  assert.equal(calls[2].opts.headers['X-Operation-ID'], 'op-fixed');
});

test('api queries the operation status after a network error', async () => {
  const queries = [];
  const ctx = run(['api'], {
    csrfToken: '', FormData: class {}, onSessionExpired() {},
    newOperationId: () => 'op-net',
    queryOperation: async id => { queries.push(id); return { confirmed: true }; },
    fetch: async () => { throw new TypeError('network down'); },
  });
  const result = await ctx.api('/instances/x/start', { method: 'POST' });
  assert.deepEqual(plain(result), { confirmed: true });
  assert.deepEqual(queries, ['op-net']);
});

test('api surfaces an ambiguous-result error when no operation record is found', async () => {
  const ctx = run(['api'], {
    csrfToken: '', FormData: class {}, onSessionExpired() {},
    newOperationId: () => 'op-none', queryOperation: async () => null,
    fetch: async () => { throw new TypeError('network down'); },
  });
  await assert.rejects(() => ctx.api('/instances/x/stop', { method: 'POST' }), /结果待确认/);
});

test('withBusy blocks duplicate submissions and restores the control', async () => {
  const ctx = run(['withBusy', 'beginBusy', 'endBusy', 'isBusy'], { opInFlight: new Map() });
  const control = { disabled: false, hasAttribute: () => false, getAttribute: () => null, setAttribute() {}, removeAttribute() {} };
  let runs = 0;
  const pending = ctx.withBusy('k', control, async () => { runs++; await new Promise(resolve => setImmediate(resolve)); return 1; });
  const second = await ctx.withBusy('k', control, async () => { runs++; return 2; });
  await pending;
  assert.equal(runs, 1);
  assert.equal(second, undefined);
  assert.equal(control.disabled, false);
  assert.equal(ctx.isBusy('k'), false);
});

test('task center counts active jobs and formats every status', () => {
  const ctx = run(['taskStatusText', 'taskActiveCount', 'upsertTask', 'renderTaskBadge'], {
    taskJobs: new Map(), taskCenterOpen: false, TASK_ACTIVE: ['running', 'queued', 'pending', 'starting'],
    renderTaskCenter() {}, document: { getElementById: () => null },
  });
  ctx.upsertTask({ id: 'j1', status: 'running' });
  ctx.upsertTask({ id: 'j2', status: 'done' });
  ctx.upsertTask({ id: 'j3', status: 'interrupted' });
  assert.equal(ctx.taskActiveCount(), 1);
  assert.equal(ctx.taskStatusText('done'), '已完成');
  assert.equal(ctx.taskStatusText('interrupted'), '已中断');
  assert.equal(ctx.taskStatusText('error'), '失败');
});

test('task polling uses an independent timer cleared on teardown', () => {
  const ctx = run(['startTaskPolling', 'stopTaskPolling'], {
    currentUser: { role: 'admin' }, taskJobs: new Map(), taskPollTimer: null,
    refreshTasks() {}, renderTaskBadge() {}, closeTaskCenter() {},
    setInterval: () => 42, clearInterval: id => { ctx.cleared = id; },
  });
  ctx.startTaskPolling();
  assert.equal(ctx.taskPollTimer, 42);
  ctx.stopTaskPolling();
  assert.equal(ctx.taskPollTimer, null);
  assert.equal(ctx.cleared, 42);
});

test('navigation exposes icon/label entries and the admin approval badge', () => {
  const box = { innerHTML: '' };
  const ctx = run(['navItems', 'renderNav'], {
    currentUser: { role: 'admin' }, pendingApprovals: 3, esc: s => String(s), $: () => box,
  });
  ctx.renderNav();
  assert.match(box.innerHTML, /仪表盘/);
  assert.match(box.innerHTML, /nav-icon/);
  assert.match(box.innerHTML, /nav-label/);
  assert.match(box.innerHTML, /nav-badge/);
  assert.equal(ctx.navItems()[0][0], 'dashboard');
  assert.equal(ctx.navItems().length, 4);
});

test('account box shows the immutable username plus profile and name-change entry', () => {
  const box = { innerHTML: '' };
  const ctx = run(['renderAccountBox'], {
    currentUser: { username: 'alice', role: 'user', minecraft_name: 'Steve', status: 'approved' },
    pendingApprovals: 0, esc: s => String(s), $: () => box,
  });
  ctx.renderAccountBox();
  assert.match(box.innerHTML, /alice/);
  assert.match(box.innerHTML, /Steve/);
  assert.match(box.innerHTML, /#\/profile/);
  assert.match(box.innerHTML, /退出登录/);
  assert.doesNotMatch(box.innerHTML, /注册后不可修改/);
});

test('registration validates the offline-compatible game name and posts the captcha token', async () => {
  const els = {
    '#reg-user': { value: 'bob' }, '#reg-pass': { value: 'password1' }, '#reg-pass2': { value: 'password1' },
    '#reg-mc': { value: 'bad name!' }, '#reg-reason': { value: 'because' }, '#reg-err': { textContent: '' }, '#reg-go': { disabled: false },
  };
  const calls = [];
  const ctx = run(['doRegister'], {
    MC_NAME_RE: /^[A-Za-z0-9_]{1,16}$/,
    $: s => els[s], esc: s => String(s), withBusy: busy,
    captchaTokenOrNull: () => 'tok', showModal() {}, resetTurnstile() {},
    api: async (p, o) => { calls.push({ p, o }); return {}; },
  });
  await ctx.doRegister();
  assert.equal(calls.length, 0);
  assert.match(els['#reg-err'].textContent, /游戏名/);

  els['#reg-mc'].value = 'Steve_1';
  await ctx.doRegister();
  assert.equal(calls[0].p, '/auth/register');
  assert.equal(calls[0].o.method, 'POST');
  assert.equal(calls[0].o.noCsrf, true);
  assert.equal(calls[0].o.body.minecraft_name, 'Steve_1');
  assert.equal(calls[0].o.body.captcha_token, 'tok');
});

test('registration rejects mismatched confirmation passwords', async () => {
  const els = {
    '#reg-user': { value: 'bob' }, '#reg-pass': { value: 'password1' }, '#reg-pass2': { value: 'password2' },
    '#reg-mc': { value: 'Steve' }, '#reg-reason': { value: 'because' }, '#reg-err': { textContent: '' }, '#reg-go': { disabled: false },
  };
  let posted = 0;
  const ctx = run(['doRegister'], {
    MC_NAME_RE: /^[A-Za-z0-9_]{1,16}$/, $: s => els[s], esc: s => String(s), withBusy: busy,
    captchaTokenOrNull: () => 'tok', showModal() {}, resetTurnstile() {},
    api: async () => { posted++; return {}; },
  });
  await ctx.doRegister();
  assert.equal(posted, 0);
  assert.match(els['#reg-err'].textContent, /不一致/);
});

test('status query stores credentials in memory and offers resubmission after rejection', async () => {
  const els = { '#st-user': { value: 'bob' }, '#st-pass': { value: 'pw' }, '#st-err': { textContent: '' }, '#st-go': { disabled: false }, '#status-result': { innerHTML: '' } };
  const calls = [];
  const ctx = run(['doStatusQuery', 'renderApplicationResult'], {
    $: s => els[s], esc: s => String(s), toast() {}, withBusy: busy, turnstileSiteKey: '',
    document: { getElementById: id => els['#' + id] || null }, mountTurnstile() {},
    api: async (p, o) => { calls.push({ p, o }); return { application: { id: 'a1', username: 'bob', minecraft_name: 'Steve', reason: 'r', status: 'rejected', revision: 2, rejection_reason: 'no' } }; },
  });
  await ctx.doStatusQuery();
  assert.equal(calls[0].p, '/auth/application/status');
  assert.equal(calls[0].o.noCsrf, true);
  assert.equal(ctx.statusCredentials.username, 'bob');
  assert.match(els['#status-result'].innerHTML, /拒绝理由/);
  assert.match(els['#status-result'].innerHTML, /重新提交/);
});

test('resubmission reuses the in-memory credentials against the resubmit endpoint', async () => {
  const els = { '#rs-mc': { value: 'Steve_2' }, '#rs-reason': { value: 'again' }, '#rs-err': { textContent: '' }, '#rs-go': { disabled: false }, '#status-result': { innerHTML: '' } };
  const calls = [];
  const ctx = run(['doResubmit', 'renderApplicationResult'], {
    statusCredentials: { username: 'bob', password: 'pw' }, MC_NAME_RE: /^[A-Za-z0-9_]{1,16}$/,
    $: s => els[s], esc: s => String(s), toast() {}, withBusy: busy,
    captchaTokenOrNull: () => 't', resetTurnstile() {}, mountTurnstile() {},
    document: { getElementById: id => els['#' + id] || null },
    api: async (p, o) => { calls.push({ p, o }); return { application: { status: 'pending' } }; },
  });
  await ctx.doResubmit();
  assert.equal(calls[0].p, '/auth/application/resubmit');
  assert.equal(calls[0].o.body.username, 'bob');
  assert.equal(calls[0].o.body.minecraft_name, 'Steve_2');
});

test('profile keeps the old game name active while a change is pending', async () => {
  const el = { innerHTML: '' };
  const calls = [];
  const els = { '#prof-mc': { value: 'NewName' }, '#prof-reason': { value: 'reason' }, '#prof-err': { textContent: '' }, '#prof-go': {} };
  const ctx = run(['renderProfileBody', 'doRequestNameChange'], {
    esc: s => String(s), toast() {}, withBusy: busy, MC_NAME_RE: /^[A-Za-z0-9_]{1,16}$/,
    $: s => els[s],
    document: { getElementById: id => (id === 'profile-body' ? el : null) },
    api: async (p, o) => { calls.push({ p, o }); return {}; },
    renderProfile() {},
  });
  ctx.renderProfileBody({ user: { username: 'bob', role: 'user', minecraft_name: 'OldName', status: 'approved' }, name_request: { id: 'r1', minecraft_name: 'NewName' } });
  assert.match(el.innerHTML, /OldName/);
  assert.match(el.innerHTML, /旧游戏名继续生效/);
  assert.match(el.innerHTML, /withdrawNameRequest\('r1'\)/);
  await ctx.doRequestNameChange();
  assert.equal(calls[0].p, '/auth/minecraft-name-requests');
  assert.equal(calls[0].o.body.minecraft_name, 'NewName');
});

test('instance permissions render grants and save with the revision', async () => {
  const body = { innerHTML: '' };
  const calls = [];
  const saveBtn = { disabled: false, hasAttribute: () => false, getAttribute: () => null, setAttribute() {}, removeAttribute() {} };
  const ctx = run(['renderPermissionRows', 'savePermissions', 'readPermissionChecks', 'filterPermissionRows'], {
    permState: { revision: 7, users: [
      { id: 'u1', username: 'alice', role: 'user', enabled: true, status: 'approved', granted: true },
      { id: 'u2', username: 'root', role: 'admin', enabled: true, granted: false },
    ] },
    esc: s => String(s), toast() {}, withBusy: busy,
    $$: () => [{ value: 'u1' }],
    $: s => (s === '#perm-save' ? saveBtn : null),
    document: { getElementById: id => (id === 'perm-body' ? body : null) },
    api: async (p, o) => { calls.push({ p, o }); return { revision: 8, users: [], whitelist_enabled: true }; },
    refreshApprovalCount() {}, renderTabPermissions() {},
  });
  ctx.renderPermissionRows();
  assert.match(body.innerHTML, /alice/);
  assert.match(body.innerHTML, /已授权/);
  assert.match(body.innerHTML, /disabled/);
  await ctx.savePermissions('i1');
  assert.equal(calls[0].p, '/instances/i1/permissions');
  assert.equal(calls[0].o.method, 'PUT');
  assert.deepEqual(plain(calls[0].o.body), { revision: 7, user_ids: ['u1'] });
});

test('admin approvals list pending applications and approve with revision + instances', async () => {
  const body = { innerHTML: '' };
  const calls = [];
  const els = { '#approvals-body': body, '#appr-err': { textContent: '' }, '#appr-go': { disabled: false, hasAttribute: () => false, getAttribute: () => null, setAttribute() {}, removeAttribute() {} } };
  const ctx = run(['renderApprovals', 'applicationById', 'doApproveApplication'], {
    accountsTab: 'accounts',
    applicationsData: { applications: [{ id: 'a1', kind: 'registration', username: 'bob', minecraft_name: 'Steve', reason: 'r', status: 'pending', revision: 3 }] },
    accountsData: { instances: [{ id: 'i1', name: 'Srv' }] },
    esc: s => String(s), toast() {}, closeModal() {}, withBusy: busy,
    $$: () => [{ value: 'i1' }],
    document: { getElementById: id => els['#' + id] || null },
    api: async (p, o) => { calls.push({ p, o }); return {}; },
    loadApprovals() {}, loadAccounts() {}, refreshApprovalCount() {}, refreshTasks() {},
  });
  ctx.renderApprovals();
  assert.match(body.innerHTML, /bob/);
  assert.match(body.innerHTML, /批准/);
  await ctx.doApproveApplication('a1');
  assert.equal(calls[0].p, '/applications/a1/approve');
  assert.deepEqual(plain(calls[0].o.body), { revision: 3, instance_ids: ['i1'] });
});

test('rejecting an application requires a reason', async () => {
  const calls = [];
  const toasts = [];
  const ctx = run(['rejectApplication'], {
    applicationsData: { applications: [{ id: 'a1', revision: 3, status: 'pending' }] },
    applicationById: id => ({ id, revision: 3 }),
    appPrompt: async () => '', toast: (m, ok) => toasts.push([m, ok]),
    api: async (p, o) => { calls.push({ p, o }); return {}; },
    loadApprovals() {}, refreshApprovalCount() {},
  });
  await ctx.rejectApplication('a1');
  assert.equal(calls.length, 0);
  assert.equal(toasts.at(-1)[1], false);
});

test('whitelist sync status surfaces a warning when pending', async () => {
  const box = { innerHTML: '' };
  const ctx = run(['loadWhitelistSync'], {
    esc: s => String(s), routeToken: 1,
    document: { getElementById: () => box },
    api: async () => ({ pending: true, detail: '实例 B 未同步' }),
  });
  await ctx.loadWhitelistSync('i1', 1);
  assert.match(box.innerHTML, /白名单同步待处理/);
  assert.match(box.innerHTML, /实例 B 未同步/);
});

test('xhrUpload sends the operation id, reports 100% then a processing phase', async () => {
  const instances = [];
  class FakeXHR {
    constructor() { this.upload = {}; instances.push(this); }
    open(method, url) { this.method = method; this.url = url; }
    setRequestHeader(key, value) { (this.headers ||= {})[key] = value; }
    send(body) {
      this.sent = body;
      this.upload.onprogress?.({ lengthComputable: true, loaded: 5, total: 10 });
      this.upload.onload?.();
      this.status = 200;
      this.responseText = '{"ok":true}';
      this.onload?.();
    }
  }
  const phases = [], progresses = [];
  const ctx = run(['xhrUpload'], {
    XMLHttpRequest: FakeXHR, sessionGeneration: 1, csrfToken: 'c', newOperationId: () => 'op-x', onSessionExpired() {},
  });
  const result = await ctx.xhrUpload({ path: '/instances/x/files/upload', body: 'fd', onProgress: p => progresses.push(p), onPhase: ph => phases.push(ph) });
  assert.equal(instances[0].headers['X-Operation-ID'], 'op-x');
  assert.equal(instances[0].headers['X-CSRF-Token'], 'c');
  assert.deepEqual(progresses, [50, 100]);
  assert.deepEqual(phases, ['processing']);
  assert.deepEqual(plain(result), { ok: true });
});

test('all upload entry points use the shared progress uploader with a processing phase', () => {
  assert.match(source, /xhrUpload\(\{ path: `\/instances\/\$\{id\}\/mods\/upload`/);
  assert.match(source, /xhrUpload\(\{ path: `\/instances\/\$\{id\}\/files\/upload/);
  assert.match(source, /path: '\/instances\/import\/upload'/);
  assert.match(source, /onPhase: phase =>/);
  assert.match(source, /服务器处理中/);
  assert.match(source, /captured|捕获开始时的目录/);
});

test('session teardown stops task polling and clears private credentials', () => {
  const main = { innerHTML: '', dataset: {}, classList: { remove() {} } };
  let stopped = 0;
  const ctx = run(['teardownSession', 'clearTimers'], {
    timers: [], activeWS: null, clearInterval() {}, currentUser: { role: 'user' }, csrfToken: 'c', loginAttempt: 0,
    currentInstanceInfo: null, usersData: null, usersInstanceId: null, modsCache: null, modDL: null, mpUp: null,
    filesEntriesCache: null, currentGameBackupProvider: null, accountsData: null,
    applicationsData: {}, permState: {}, profileData: {}, statusCredentials: { username: 'a', password: 'b' },
    stopTaskPolling: () => stopped++, closeDrawer() {}, closeAccountMenu() {},
    $: () => main, document: { body: { classList: { remove() {} } } },
  });
  ctx.teardownSession();
  assert.equal(stopped, 1);
  assert.equal(ctx.statusCredentials, null);
  assert.equal(ctx.profileData, null);
  assert.equal(ctx.permState, null);
  assert.equal(ctx.applicationsData, null);
});

test('registration and application credentials are never written to browser storage', () => {
  assert.doesNotMatch(source, /localStorage\.setItem\([^)]*password/i);
  assert.doesNotMatch(source, /sessionStorage\.setItem\([^)]*password/i);
  assert.match(source, /statusCredentials = \{ username, password \}/);
});

test('index.html exposes the shared topbar, footer, task panel and drawer', () => {
  assert.match(html, /id="topbar"/);
  assert.match(html, /id="app-footer"/);
  assert.match(html, /id="task-panel"/);
  assert.match(html, /id="task-btn"/);
  assert.match(html, /id="sidebar-backdrop"/);
  assert.match(html, /id="sidebar-toggle"/);
  assert.match(html, /id="menu-btn"/);
  assert.match(html, /id="account-box"/);
  assert.match(html, /id="nav-links"/);
  assert.match(html, /id="ver"/);
  assert.match(html, /rel="noopener"/);
  assert.match(html, /github\.com\/liansishen\/mcspr/);
  assert.doesNotMatch(html, /TOKEN|showTokenModal|auth_required/);
});

test('styles keep viewport-constrained detail layout, sticky heads and drawer states', () => {
  assert.match(css, /#main\.detail-layout\s*\{[^}]*display:\s*flex;[^}]*overflow:\s*hidden;/s);
  assert.match(css, /#tab-body\s*\{[^}]*min-height:\s*0;[^}]*overflow-y:\s*auto;/s);
  assert.match(css, /body\.drawer-open #sidebar/);
  assert.match(css, /html\.sidebar-collapsed #sidebar/);
  assert.match(css, /\.table-wrap thead/);
  assert.match(css, /\.console-input input/);
  assert.match(source, /const THEME_ORDER = \['dark', 'light', 'mc', 'claude'\]/);
});

function routeCtx({ role, hash }) {
  const calls = [];
  const main = { classList: { toggle() {}, remove() {} } };
  const ctx = run(['route'], {
    currentUser: role ? { role } : null,
    location: { hash },
    document: { body: { classList: { toggle() {}, remove() {}, contains: () => false } } },
    clearTimers() {}, closeDrawer() {}, closeAccountMenu() {},
    navActiveKey: () => 'x', $: () => main, $$: () => [], esc: s => String(s),
    ADMIN_TABS: [['console', '控制台']],
    USER_VIEW_TABS: [['announcement', '公告'], ['overview', '运行信息']],
    setTopbarTitle() {}, PAGE_TITLES: {},
    renderLogin: () => calls.push(['login']),
    renderRegister: () => calls.push(['register']),
    renderApplicationStatus: () => calls.push(['status']),
    renderProfile: () => calls.push(['profile']),
    renderDashboard() {}, renderInstances() {}, renderAccounts() {}, renderPanelSettings() {},
    renderMyInstances() {}, renderInstance() {}, renderUserInstance() {},
  });
  return { ctx, calls };
}

test('route serves register and status pages to unauthenticated visitors', async () => {
  const reg = routeCtx({ role: null, hash: '#/register' });
  await reg.ctx.route();
  assert.deepEqual(reg.calls, [['register']]);

  const status = routeCtx({ role: null, hash: '#/status' });
  await status.ctx.route();
  assert.deepEqual(status.calls, [['status']]);

  const login = routeCtx({ role: null, hash: '#/whatever' });
  await login.ctx.route();
  assert.deepEqual(login.calls, [['login']]);
});

test('route serves the profile page to signed-in users', async () => {
  const { ctx, calls } = routeCtx({ role: 'user', hash: '#/profile' });
  await ctx.route();
  assert.deepEqual(calls, [['profile']]);
});

test('route closes the mobile drawer and account menu on navigation', async () => {
  let drawerClosed = 0, menuClosed = 0;
  const ctx = run(['route'], {
    currentUser: { role: 'user' }, location: { hash: '#/my-instances' },
    document: { body: { classList: { toggle() {}, remove() {}, contains: () => false } } },
    clearTimers() {}, closeDrawer: () => drawerClosed++, closeAccountMenu: () => menuClosed++,
    navActiveKey: () => 'x', $: () => ({ classList: { toggle() {}, remove() {} } }), $$: () => [], esc: s => String(s),
    ADMIN_TABS: [], USER_VIEW_TABS: [], setTopbarTitle() {}, PAGE_TITLES: {},
    renderMyInstances() {},
  });
  await ctx.route();
  assert.equal(drawerClosed, 1);
  assert.equal(menuClosed, 1);
});

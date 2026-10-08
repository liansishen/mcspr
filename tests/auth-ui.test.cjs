const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

const source = fs.readFileSync(path.join(__dirname, '../web/app.js'), 'utf8');
const html = fs.readFileSync(path.join(__dirname, '../web/index.html'), 'utf8');

function extractFn(name) {
  const match = source.match(new RegExp(`(?:async )?function ${name}\\([^)]*\\) \\{[\\s\\S]*?\\n\\}`));
  assert.ok(match, `${name} should exist`);
  return match[0];
}

function run(names, context) {
  const ctx = vm.createContext({ sessionGeneration: 0, loginAttempt: 0, routeToken: 0, ...context });
  vm.runInContext(names.map(extractFn).join('\n'), ctx);
  return ctx;
}
const plain = v => JSON.parse(JSON.stringify(v));

test('api attaches a CSRF header to writes and uses same-origin credentials, but not for login', async () => {
  const calls = [];
  const ctx = run(['api'], {
    csrfToken: 'csrf-1',
    FormData: class {},
    onSessionExpired: () => { throw new Error('should not expire'); },
    fetch: async (url, opts) => {
      calls.push({ url, opts });
      return { status: 200, ok: true, headers: { get: () => 'application/json' }, json: async () => ({ ok: true }) };
    },
  });

  await ctx.api('/instances', { method: 'POST', body: { a: 1 } });
  await ctx.api('/auth/login', { method: 'POST', body: { u: 1 }, noCsrf: true });
  await ctx.api('/stats');

  assert.equal(calls[0].url, '/api/instances');
  assert.equal(calls[0].opts.headers['X-CSRF-Token'], 'csrf-1');
  assert.equal(calls[0].opts.headers['Content-Type'], 'application/json');
  assert.equal(calls[0].opts.body, '{"a":1}');
  assert.equal(calls[0].opts.credentials, 'same-origin');
  assert.equal(calls[1].opts.headers['X-CSRF-Token'], undefined);
  assert.equal(calls[2].opts.headers['X-CSRF-Token'], undefined);
});

test('api tears the session down on 401 but keeps login failures local', async () => {
  let expired = 0;
  const ctx = run(['api'], {
    csrfToken: 'c',
    FormData: class {},
    onSessionExpired: () => { expired++; },
    fetch: async () => ({ status: 401, ok: false, statusText: 'Unauthorized', headers: { get: () => 'application/json' }, json: async () => ({ error: '未登录' }) }),
  });
  await assert.rejects(() => ctx.api('/instances'), e => { assert.equal(e.status, 401); return true; });
  assert.equal(expired, 1);

  let loginExpired = 0;
  const login = run(['api'], {
    csrfToken: '',
    FormData: class {},
    onSessionExpired: () => { loginExpired++; },
    fetch: async () => ({ status: 401, ok: false, statusText: 'Unauthorized', headers: { get: () => 'application/json' }, json: async () => ({ error: '用户名或密码错误' }) }),
  });
  await assert.rejects(
    () => login.api('/auth/login', { method: 'POST', body: {}, skipAuthRedirect: true, noCsrf: true }),
    /用户名或密码错误/);
  assert.equal(loginExpired, 0);
});

test('api surfaces 403/404 status on the thrown error', async () => {
  for (const status of [403, 404]) {
    const ctx = run(['api'], {
      csrfToken: '', FormData: class {}, onSessionExpired: () => {},
      fetch: async () => ({ status, ok: false, statusText: 'x', headers: { get: () => 'application/json' }, json: async () => ({ error: 'nope' }) }),
    });
    await assert.rejects(() => ctx.api('/instances/secret'), e => e.status === status);
  }
});

test('doLogin stores the session and routes to the role home page', async () => {
  const els = {
    '#login-user': { value: 'alice' },
    '#login-pass': { value: 'pw' },
    '#login-err': { textContent: '' },
    '#login-btn': { disabled: false },
  };
  const calls = [];
  const ctx = run(['doLogin'], {
    currentUser: null,
    csrfToken: '',
    location: { hash: '' },
    $: sel => els[sel],
    document: { body: { classList: { add() {}, remove() {} } } },
    api: async (p, o) => { calls.push({ p, o }); return { user: { id: '1', username: 'alice', role: 'user' }, csrf_token: 'tok' }; },
    renderNav() {}, renderAccountBox() {}, route() {},
  });

  await ctx.doLogin();

  assert.equal(calls[0].p, '/auth/login');
  assert.deepEqual(plain(calls[0].o.body), { username: 'alice', password: 'pw' });
  assert.equal(calls[0].o.noCsrf, true);
  assert.equal(calls[0].o.skipAuthRedirect, true);
  assert.equal(ctx.currentUser.role, 'user');
  assert.equal(ctx.csrfToken, 'tok');
  assert.equal(ctx.location.hash, '#/my-instances');
});

test('doLogin sends an admin to the dashboard and reports failures without leaving the form', async () => {
  const els = {
    '#login-user': { value: 'root' },
    '#login-pass': { value: 'pw' },
    '#login-err': { textContent: '' },
    '#login-btn': { disabled: false },
  };
  const ctx = run(['doLogin'], {
    currentUser: null,
    csrfToken: '',
    location: { hash: '#/my-instances' },
    $: sel => els[sel],
    document: { body: { classList: { add() {}, remove() {} } } },
    api: async () => ({ user: { role: 'admin' }, csrf_token: 'a' }),
    renderNav() {}, renderAccountBox() {}, route() {},
  });
  await ctx.doLogin();
  assert.equal(ctx.location.hash, '#/dashboard');

  const fail = run(['doLogin'], {
    currentUser: null,
    csrfToken: '',
    location: { hash: '' },
    $: sel => els[sel],
    document: { body: { classList: { add() {}, remove() {} } } },
    api: async () => { const e = new Error('用户名或密码错误'); e.status = 401; throw e; },
    renderNav() {}, renderAccountBox() {}, route() {},
  });
  els['#login-err'].textContent = '';
  els['#login-btn'].disabled = false;
  await fail.doLogin();
  assert.equal(els['#login-err'].textContent, '用户名或密码错误');
  assert.equal(els['#login-btn'].disabled, false);
});

function routeCtx({ role, hash }) {
  const calls = [];
  const main = { classList: { toggle() {} } };
  const ctx = run(['route'], {
    routeToken: 0,
    currentUser: role ? { role } : null,
    location: { hash },
    document: { body: { classList: { toggle() {} } } },
    clearTimers() {},
    navActiveKey: () => 'x',
    $: () => main,
    $$: () => [],
    esc: s => String(s),
    ADMIN_TABS: [['console', '控制台'], ['announcement', '公告']],
    USER_VIEW_TABS: [['announcement', '公告'], ['overview', '运行信息']],
    renderLogin: () => calls.push(['login']),
    renderDashboard: () => calls.push(['dashboard']),
    renderInstances: () => calls.push(['instances']),
    renderAccounts: () => calls.push(['accounts']),
    renderPanelSettings: () => calls.push(['settings']),
    renderMyInstances: () => calls.push(['my']),
    renderInstance: (id, tab) => calls.push(['instance', id, tab]),
    renderUserInstance: (id, tab) => calls.push(['uinstance', id, tab]),
  });
  return { ctx, calls };
}

test('route sends unauthenticated visitors to the login page', async () => {
  const { ctx, calls } = routeCtx({ role: null, hash: '#/dashboard' });
  await ctx.route();
  assert.deepEqual(calls, [['login']]);
});

test('route rejects admin pages for ordinary users and falls back to 我的实例', async () => {
  for (const hash of ['#/dashboard', '#/instances', '#/settings', '#/accounts']) {
    const { ctx, calls } = routeCtx({ role: 'user', hash });
    await ctx.route();
    assert.equal(ctx.location.hash, '#/my-instances', hash);
    assert.deepEqual(calls, []);
  }
});

test('route rejects forbidden ordinary instance tabs and keeps announcement/overview', async () => {
  const forbidden = routeCtx({ role: 'user', hash: '#/instance/i1/console' });
  await forbidden.ctx.route();
  assert.equal(forbidden.ctx.location.hash, '#/instance/i1/announcement');
  assert.deepEqual(forbidden.calls, []);

  const overview = routeCtx({ role: 'user', hash: '#/instance/i1/overview' });
  await overview.ctx.route();
  assert.deepEqual(overview.calls, [['uinstance', 'i1', 'overview']]);

  const fallback = routeCtx({ role: 'user', hash: '#/instance/i1' });
  await fallback.ctx.route();
  assert.deepEqual(fallback.calls, [['uinstance', 'i1', 'announcement']]);
});

test('route lets admins reach accounts and rejects unknown tabs/pages safely', async () => {
  const accounts = routeCtx({ role: 'admin', hash: '#/accounts' });
  await accounts.ctx.route();
  assert.deepEqual(accounts.calls, [['accounts']]);

  const bogusTab = routeCtx({ role: 'admin', hash: '#/instance/i1/bogus' });
  await bogusTab.ctx.route();
  assert.deepEqual(bogusTab.calls, [['instance', 'i1', 'console']]);

  const bogusPage = routeCtx({ role: 'admin', hash: '#/nope' });
  await bogusPage.ctx.route();
  assert.equal(bogusPage.ctx.location.hash, '#/dashboard');
});

test('navItems exposes admin-only entries and a single ordinary entry', () => {
  const admin = run(['navItems'], { currentUser: { role: 'admin' } });
  assert.deepEqual(plain(admin.navItems().map(x => x[0])), ['dashboard', 'instances', 'accounts', 'settings']);
  const user = run(['navItems'], { currentUser: { role: 'user' } });
  assert.deepEqual(plain(user.navItems().map(x => x[0])), ['my-instances']);
});

test('teardownSession clears timers, websocket, cached DOM data and role state', () => {
  const main = {
    innerHTML: 'stale',
    dataset: { instanceId: 'i1', viewRole: 'admin' },
    classList: { remove() { main.removed = true; } },
  };
  const modal = { innerHTML: 'stale' };
  const ws = { close() { ws.closed = true; } };
  const cleared = [];
  const ctx = run(['clearTimers', 'teardownSession'], {
    timers: [1, 2, 3],
    activeWS: ws,
    clearInterval: id => cleared.push(id),
    currentUser: { role: 'admin' },
    csrfToken: 'c',
    currentInstanceInfo: { id: 'i1' },
    usersData: {}, usersInstanceId: 'i1', modsCache: {}, modDL: {}, mpUp: {},
    filesEntriesCache: [], currentGameBackupProvider: {}, accountsData: {},
    $: sel => ({ '#main': main, '#modal-root': modal }[sel]),
    document: { body: { classList: { remove() {} } } },
  });

  ctx.teardownSession();

  assert.deepEqual(cleared, [1, 2, 3]);
  assert.equal(ctx.timers.length, 0);
  assert.equal(ctx.activeWS, null);
  assert.equal(ws.closed, true);
  assert.equal(ctx.currentUser, null);
  assert.equal(ctx.csrfToken, '');
  assert.equal(ctx.currentInstanceInfo, null);
  assert.equal(main.dataset.instanceId, undefined);
  assert.equal(main.dataset.viewRole, undefined);
  assert.equal(main.innerHTML, '');
  assert.equal(modal.innerHTML, '');
});

test('frontend no longer authenticates with the legacy token', () => {
  assert.doesNotMatch(source, /mcspr\.token/);
  assert.doesNotMatch(source, /Authorization/);
  assert.doesNotMatch(source, /showTokenModal|saveToken/);
  assert.doesNotMatch(source, /\/ws\?token/);
  assert.doesNotMatch(source, /files\/download\?path=[^`]*token/);
  assert.doesNotMatch(source, /id="ps-token"/);
  assert.match(source, /X-CSRF-Token/);
  assert.match(source, /credentials: 'same-origin'/);
});

test('index.html renders role-aware nav and account boxes without token prompts', () => {
  assert.match(html, /id="nav-links"/);
  assert.match(html, /id="account-box"/);
  assert.doesNotMatch(html, /TOKEN|showTokenModal|auth_required/);
});

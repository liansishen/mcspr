const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

const source = fs.readFileSync(path.join(__dirname, '../web/app.js'), 'utf8');
function extractFn(name) {
  const match = source.match(new RegExp(`(?:async )?function ${name}\\([^)]*\\) \\{[\\s\\S]*?\\n\\}`));
  assert.ok(match, `${name} should exist`);
  return match[0];
}
function run(names, context) {
  const ctx = vm.createContext({ sessionGeneration: 1, loginAttempt: 0, routeToken: 1, newOperationId: () => 'op-test', resolveOperation: async () => null, replayPublicOperation: async () => null, beginGlobalBusy() {}, endGlobalBusy() {}, renderGlobalBusy() {}, writeKey: (m, p) => m + ' ' + p, apiInFlight: new Map(), globalBusyCount: 0, setTopbarTitle() {}, upsertTask() {}, stopTaskPolling() {}, ...context });
  vm.runInContext(names.map(extractFn).join('\n'), ctx);
  return ctx;
}
function deferred() {
  let resolve, reject;
  const promise = new Promise((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}
const unauthorized = () => ({ status: 401, ok: false, statusText: 'Unauthorized', json: async () => ({ error: 'expired' }) });

test('late 401 from an old session preserves the newly established session', async () => {
  for (const name of ['api', 'rawFetch']) {
    const response = deferred();
    let expired = 0;
    const ctx = run([name], {
      csrfToken: '', FormData: class {}, fetch: () => response.promise,
      onSessionExpired: () => expired++,
    });
    const pending = ctx[name]('/instances');
    ctx.sessionGeneration++;
    response.resolve(unauthorized());
    if (name === 'api') await assert.rejects(pending, error => error.status === 401);
    else assert.equal((await pending).status, 401);
    assert.equal(expired, 0);
  }
});

test('current-session download failure expires the session and prevents native download', async () => {
  let expired = 0, clicked = 0;
  const ctx = run(['rawFetch', 'downloadUrl'], {
    fetch: async () => unauthorized(),
    onSessionExpired: () => expired++,
    document: { createElement: () => ({ click: () => clicked++ }) },
  });
  await assert.rejects(() => ctx.downloadUrl('/api/instances/a/files/download?path=test', 'test'));
  assert.equal(expired, 1);
  assert.equal(clicked, 0);
});

test('native downloads check authentication with HEAD and keep the original stream URL', async () => {
  const calls = [], links = [];
  const ctx = run(['rawFetch', 'downloadUrl', 'downloadFile', 'downloadArchive', 'downloadGameBackup'], {
    fetch: async (url, options) => { calls.push({ url, options }); return { status: 200, ok: true }; },
    api: async () => ({}), toast: message => { throw new Error(message); }, onSessionExpired() {},
    document: { createElement: () => { const link = { click() { links.push({ url: link.href, name: link.download }); } }; return link; } },
  });
  await ctx.downloadFile('a', '中文 文件.txt');
  await ctx.downloadArchive('a', 'world');
  await ctx.downloadGameBackup('a', 'backup.zip');
  assert.equal(calls.length, 3);
  for (const call of calls) {
    assert.equal(call.options.method, 'HEAD');
    assert.equal(call.options.credentials, 'same-origin');
  }
  assert.equal(links[0].name, '中文 文件.txt');
  assert.equal(links[0].url, '/api/instances/a/files/download?path=' + encodeURIComponent('中文 文件.txt'));
  assert.equal(links[1].name, 'world.tar.gz');
  assert.equal(links[2].url, '/api/instances/a/game-backups/backup.zip');
});

test('administrator backup download uses the authenticated registered route', async () => {
  let url;
  const ctx = run(['downloadBackup'], {
    rawFetch: async target => { url = target; return { ok: true, blob: async () => ({}) }; },
    document: { createElement: () => ({ click() {} }) },
    URL: { createObjectURL: () => 'blob:test', revokeObjectURL() {} },
    toast: message => { throw new Error(message); },
  });
  await ctx.downloadBackup('a', 'backup one.tar.gz');
  assert.equal(url, '/api/instances/a/backups/' + encodeURIComponent('backup one.tar.gz'));
});

for (const name of ['renderDashboard', 'renderInstances', 'renderMyInstances', 'renderTabUserOverview', 'renderTabMonitor']) {
  for (const status of [401, 403, 404]) {
    test(`${name} stops polling after ${status}`, async () => {
      let scheduled = 0, cleared = 0;
      const main = { innerHTML: '', dataset: { instanceId: 'a', viewRole: 'user' }, classList: { remove() {} } };
      const el = { innerHTML: '' };
      let ctx;
      ctx = run([name, 'stopViewOnAccessError', 'invalidateView', 'showInstanceError'], {
        currentInstanceInfo: { id: 'a' },
        $: () => main,
        document: { getElementById: () => el, body: { classList: { remove() {} } } },
        currentUser: { role: 'user' },
        esc: value => String(value),
        clearTimers: () => cleared++, every: () => scheduled++,
        api: async () => {
          if (status === 401 && ctx.routeToken === 1) ctx.invalidateView();
          const error = new Error('denied'); error.status = status; throw error;
        },
      });
      if (['renderTabUserOverview', 'renderTabMonitor'].includes(name)) await ctx[name]('a', el, 1);
      else await ctx[name](1);
      assert.equal(scheduled, 0);
      assert.equal(ctx.routeToken, 2);
      assert.equal(main.dataset.instanceId, undefined);
      if (status !== 401) assert.ok(cleared > 0);
    });
  }
}

function editorContext() {
  const elements = {
    '#ann-input': { value: 'old markdown' }, '#ann-save-btn': { disabled: false },
    '#ann-msg': { textContent: '' }, '#ann-meta': { innerHTML: '' }, '#ann-preview': { innerHTML: '' },
  };
  const requests = [];
  const ctx = run(['announcementEditorActive', 'previewAnnouncement', 'saveAnnouncement'], {
    annState: { id: 'a', updatedAt: 'version-a', ready: true },
    $: selector => elements[selector],
    api: (url, options) => { const pending = deferred(); requests.push({ url, options, pending }); return pending.promise; },
    announcementMeta: data => data.updated_at, toast() {},
  });
  return { ctx, elements, requests };
}

test('announcement save and preview responses preserve another instance editor', async () => {
  for (const operation of ['saveAnnouncement', 'previewAnnouncement']) {
    const { ctx, elements, requests } = editorContext();
    const pending = ctx[operation]('a');
    ctx.routeToken++;
    ctx.annState = { id: 'b', updatedAt: 'version-b', ready: true };
    elements['#ann-input'] = { value: 'new editor' };
    elements['#ann-preview'].innerHTML = 'new preview';
    requests[0].pending.resolve({ updated_at: 'old-response', html: 'old preview' });
    await pending;
    assert.equal(ctx.annState.updatedAt, 'version-b');
    assert.equal(elements['#ann-preview'].innerHTML, 'new preview');
  }
});

test('late announcement preview cannot overwrite the saved preview', async () => {
  const { ctx, elements, requests } = editorContext();
  const preview = ctx.previewAnnouncement('a');
  elements['#ann-input'].value = 'new markdown';
  const save = ctx.saveAnnouncement('a');
  requests[1].pending.resolve({ updated_at: 'new-version', html: 'saved preview' });
  await save;
  requests[0].pending.resolve({ html: 'old preview' });
  await preview;
  assert.match(elements['#ann-preview'].innerHTML, /saved preview/);
  assert.equal(ctx.annState.updatedAt, 'new-version');
});

test('announcement save retains new typing made while the request is pending', async () => {
  const { ctx, elements, requests } = editorContext();
  const pending = ctx.saveAnnouncement('a');
  elements['#ann-input'].value = 'unsaved typing';
  requests[0].pending.resolve({ updated_at: 'saved-version', html: 'saved preview' });
  await pending;
  assert.equal(elements['#ann-input'].value, 'unsaved typing');
  assert.equal(ctx.annState.updatedAt, 'saved-version');
  assert.equal(ctx.annState.original, 'old markdown');
  assert.match(elements['#ann-msg'].textContent, /尚未保存/);
});

test('TPS monitor and ordinary overview share state formatting and local sample times', async () => {
  const nodes = {};
  const ctx = run(['renderTabMonitor', 'tpsDisplay', 'formatSampleTime', 'renderUserOverview'], {
    routeToken: 1, esc: value => String(value), STATUS_TEXT: { running: '运行中' },
    fmtUptime: value => String(value), playerStatsRows: () => '', renderChart() {}, every() {},
    document: { getElementById: id => nodes[id] ||= {} },
    api: async url => url.endsWith('/status')
      ? { status: 'running', tps: { state: 'stale', stale: true, tps: 19.9, sampled_at: '2026-10-08T12:00:00Z' } }
      : {},
  });
  await ctx.renderTabMonitor('a', {}, 1);
  assert.equal(nodes['mon-tps'].textContent, '采样过期');
  const local = new Date('2026-10-08T12:00:00Z').toLocaleString();
  assert.match(nodes['mon-tps-hint'].textContent, new RegExp(local.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')));
  const view = {};
  ctx.renderUserOverview(view, { status: 'running', tps: { tps: 20, sampled_at: '2026-10-08T12:00:00Z' }, player_stats: [] });
  assert.ok(view.innerHTML.includes(local));
  assert.equal(ctx.formatSampleTime('invalid'), '');
});

test('password modal teardown handles expired requests without a missing-button error', async () => {
  const elements = { '#pw-old': { value: 'old' }, '#pw-new': { value: 'new' }, '#pw-confirm': { value: 'new' }, '#pw-go': { disabled: false }, '#pw-err': {} };
  const ctx = run(['doChangePassword'], {
    $: selector => elements[selector],
    api: async () => { const error = new Error('expired'); error.status = 401; for (const key of Object.keys(elements)) delete elements[key]; throw error; },
  });
  await ctx.doChangePassword();
});

test('console completion reads the current online player snapshot', () => {
  const input = { value: 'kick Al', selectionStart: 7, selectionEnd: 7 };
  const ctx = run(['consoleTabComplete'], {
    document: { getElementById: () => input }, usersData: null, currentInstanceInfo: { player_names: ['Alex'] },
  });
  ctx.consoleTabComplete('a');
  assert.equal(input.value, 'kick Alex');
});
test('administrator instance polling stops and clears cached identity after access is revoked', async () => {
  const main = { innerHTML: '', dataset: {}, classList: { add() {}, remove() {} } };
  const elements = { '#main': main };
  let scheduled = 0, requests = 0;
  const ctx = run(['renderInstance', 'stopViewOnAccessError', 'invalidateView', 'showInstanceError'], {
    currentInstanceInfo: null, ADMIN_TABS: [['console', '控制台']],
    currentUser: { role: 'admin' }, document: { body: { classList: { remove() {} } } },
    $: selector => elements[selector] ||= {}, $$: () => [],
    esc: value => String(value), statusPill: () => '', instanceRuntimeText: () => '', actionButtons: () => '',
    renderTabConsole() {}, clearTimers() {}, every: () => scheduled++,
    api: async () => {
      if (++requests === 1) return { id: 'a', name: 'test', status: 'running' };
      const error = new Error('revoked'); error.status = 404; throw error;
    },
  });
  await ctx.renderInstance('a', 'console', 1);
  assert.equal(scheduled, 0);
  assert.equal(ctx.currentInstanceInfo, null);
  assert.equal(main.dataset.instanceId, undefined);
  assert.match(main.innerHTML, /未授权/);
});

test('account form errors remain safe when session expiry removes the modal', async () => {
  for (const name of ['doCreateAccount', 'saveEditAccount', 'doResetPassword']) {
    const elements = { '#acct-name': { value: 'test' }, '#acct-pass': { value: 'password' }, '#acct-role': { value: 'user' }, '#acct-go': {}, '#acct-err': {} };
    const ctx = run([name], {
      $: selector => elements[selector], accountById: () => ({ enabled: true }), readInstanceChecks: () => ['a'],
      api: async () => { for (const key of Object.keys(elements)) delete elements[key]; const error = new Error('expired'); error.status = 401; throw error; },
    });
    await ctx[name]('test');
  }
});

test('login submission is single-flight even when Enter is pressed repeatedly', async () => {
  const response = deferred();
  let calls = 0;
  const elements = { '#login-user': { value: 'test' }, '#login-pass': { value: 'password' }, '#login-btn': { disabled: false }, '#login-err': {} };
  const ctx = run(['doLogin'], {
    $: selector => elements[selector], api: () => { calls++; return response.promise; },
    currentUser: null, csrfToken: '', location: { hash: '' },
    document: { body: { classList: { remove() {} } } }, renderNav() {}, renderAccountBox() {},
  });
  const pending = ctx.doLogin();
  await ctx.doLogin();
  response.resolve({ user: { role: 'user' }, csrf_token: 'new' });
  await pending;
  assert.equal(calls, 1);
  assert.equal(ctx.location.hash, '#/my-instances');
});

test('late logout completion preserves a later session', async () => {
  const response = deferred();
  let tornDown = 0;
  const ctx = run(['doLogout'], { api: () => response.promise, teardownSession: () => tornDown++, renderNav() {}, renderAccountBox() {}, renderLogin() {} });
  const pending = ctx.doLogout();
  ctx.sessionGeneration++;
  response.resolve({});
  await pending;
  assert.equal(tornDown, 0);
});

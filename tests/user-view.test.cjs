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
  const ctx = vm.createContext({ routeToken: 0, sessionGeneration: 0, ...context });
  if (names.includes('saveAnnouncement') || names.includes('previewAnnouncement')) names = ['announcementEditorActive', ...names];
  vm.runInContext(names.map(extractFn).join('\n'), ctx);
  return ctx;
}
const plain = v => JSON.parse(JSON.stringify(v));

test('tpsDisplay covers every runtime and sampling state', () => {
  const ctx = run(['tpsDisplay'], {});
  assert.equal(ctx.tpsDisplay(null, 'stopped').value, '已停止');
  assert.equal(ctx.tpsDisplay(null, 'starting').value, '采样中…');
  assert.equal(ctx.tpsDisplay(null, 'stopping').value, '采样中…');
  assert.equal(ctx.tpsDisplay({ needs_rcon: true }, 'running').value, '未配置采集');
  assert.equal(ctx.tpsDisplay({ error: 'boom' }, 'running').value, '采样失败');
  assert.equal(ctx.tpsDisplay({ stale: true, tps: 19.9 }, 'running').value, '采样过期');
  assert.equal(ctx.tpsDisplay({ tps: null }, 'running').value, '采样中…');
  assert.equal(ctx.tpsDisplay({ tps: 19.94, mspt: 2.3 }, 'running').value, '19.9');
  assert.equal(ctx.tpsDisplay({ tps: 19.94, mspt: 2.3 }, 'running').hint, 'MSPT 2.3ms');
});

test('overviewPlayerRows sorts by total time and zeroes offline current sessions', () => {
  const ctx = run(['overviewPlayerRows'], {});
  const rows = ctx.overviewPlayerRows([
    { name: 'b', online: true, total_secs: 100, current_session_secs: 50 },
    { name: 'a', online: false, total_secs: 300, current_session_secs: 999 },
    { name: 'c', online: true, total_secs: 200, current_session_secs: 10 },
  ]);
  assert.deepEqual(plain(rows.map(r => r.name)), ['a', 'c', 'b']);
  assert.equal(rows.find(r => r.name === 'a').current_session_secs, 0);
  assert.equal(rows.find(r => r.name === 'c').current_session_secs, 10);
  assert.deepEqual(plain(ctx.overviewPlayerRows(null)), []);
});

test('playerStatsRows marks online state and exposes live session baselines', () => {
  const ctx = run(['overviewPlayerRows', 'playerStatsRows'], { esc: s => String(s), fmtUptime: s => `${s}s` });
  const rows = ctx.playerStatsRows([
    { name: 'Steve', online: true, total_secs: 120, current_session_secs: 30 },
    { name: 'Alex', online: false, total_secs: 60, current_session_secs: 5 },
  ]);
  assert.match(rows, /Steve/);
  assert.match(rows, /在线/);
  assert.match(rows, /data-online="1" data-base="30"/);
  assert.match(rows, /Alex/);
  assert.match(rows, /离线/);
  assert.match(rows, /data-online="0" data-base="0"/);
  assert.match(rows, /120s/);
  assert.match(ctx.playerStatsRows([]), /暂无玩家记录/);
});

test('announcementMeta and announcementViewHtml describe empty announcements', () => {
  const ctx = run(['announcementMeta', 'announcementViewHtml'], { esc: s => String(s) });
  assert.match(ctx.announcementMeta(null), /管理员暂未发布公告/);
  assert.match(ctx.announcementMeta({ updated_at: '2026-10-08 10:00', updated_by: 'admin' }), /2026-10-08 10:00/);
  assert.match(ctx.announcementViewHtml({ html: '<p>hi</p>' }), /<p>hi<\/p>/);
  assert.match(ctx.announcementViewHtml({ html: '' }), /管理员暂未发布公告/);
});

test('announcement save sends the version token and refreshes state on success', async () => {
  const els = {
    '#ann-input': { value: '公告正文' },
    '#ann-msg': { textContent: '', className: '' },
    '#ann-save-btn': { disabled: false },
    '#ann-meta': { innerHTML: '' },
    '#ann-preview': { innerHTML: '' },
  };
  const calls = [];
  const ctx = run(['saveAnnouncement'], {
    annState: { id: 'i1', updatedAt: 't1', original: '' },
    $: sel => els[sel],
    api: async (p, o) => { calls.push({ p, o }); return { markdown: '公告正文', html: '<p>x</p>', updated_at: 't2', updated_by: 'admin' }; },
    toast() {},
    announcementMeta: d => `meta:${d.updated_at}`,
  });

  await ctx.saveAnnouncement('i1');

  assert.equal(calls[0].p, '/instances/i1/announcement');
  assert.equal(calls[0].o.method, 'PUT');
  assert.deepEqual(plain(calls[0].o.body), { markdown: '公告正文', expected_updated_at: 't1' });
  assert.equal(ctx.annState.updatedAt, 't2');
  assert.match(els['#ann-preview'].innerHTML, /<p>x<\/p>/);
  assert.match(els['#ann-meta'].innerHTML, /t2/);
  assert.equal(els['#ann-save-btn'].disabled, false);
});

test('announcement save conflict keeps the editor input intact', async () => {
  const els = {
    '#ann-input': { value: '正在编辑的内容' },
    '#ann-msg': { textContent: '', className: '' },
    '#ann-save-btn': { disabled: false },
    '#ann-meta': { innerHTML: '' },
    '#ann-preview': { innerHTML: '' },
  };
  const toasts = [];
  const ctx = run(['saveAnnouncement'], {
    annState: { id: 'i1', updatedAt: 't1', original: '' },
    $: sel => els[sel],
    api: async () => { const e = new Error('conflict'); e.status = 409; throw e; },
    toast: (m, ok) => toasts.push([m, ok]),
    announcementMeta: () => 'meta',
  });

  await ctx.saveAnnouncement('i1');

  assert.equal(els['#ann-input'].value, '正在编辑的内容');
  assert.match(els['#ann-msg'].textContent, /冲突/);
  assert.equal(els['#ann-save-btn'].disabled, false);
  assert.equal(toasts[0][1], false);
});

test('ordinary instance list shows the empty-grant hint when nothing is assigned', async () => {
  const main = { innerHTML: '' };
  const list = { innerHTML: '' };
  const ctx = run(['renderMyInstances'], {
    routeToken: 1,
    $: sel => ({ '#main': main, '#my-list': list }[sel]),
    api: async () => ({ instances: [] }),
    every() {},
    statusPill: () => '', fmtUptime: () => '', esc: s => String(s),
  });
  await ctx.renderMyInstances(1);
  assert.match(list.innerHTML, /管理员尚未分配实例/);
});

test('ordinary instance list renders granted instances with announcement entry links', async () => {
  const main = { innerHTML: '' };
  const list = { innerHTML: '' };
  const ctx = run(['renderMyInstances'], {
    routeToken: 1,
    $: sel => ({ '#main': main, '#my-list': list }[sel]),
    api: async () => ({ instances: [{ id: 'i1', name: '生存服', status: 'running', uptime_secs: 100, players: 2 }] }),
    every() {},
    statusPill: s => `<span>${s}</span>`, fmtUptime: s => `${s}s`, esc: s => String(s),
  });
  await ctx.renderMyInstances(1);
  assert.match(list.innerHTML, /生存服/);
  assert.match(list.innerHTML, /#\/instance\/i1\/announcement/);
  assert.match(list.innerHTML, /2 名玩家在线/);
});

test('admin account operations call the documented endpoints', async () => {
  const els = {
    '#acct-name': { value: 'bob' },
    '#acct-pass': { value: 'pw' },
    '#acct-role': { value: 'user' },
    '#acct-err': { textContent: '' },
    '#acct-go': { disabled: false },
  };
  const calls = [];
  const ctx = run(['doCreateAccount', 'saveEditAccount', 'toggleAccount', 'doResetPassword', 'deleteAccount'], {
    routeToken: 1,
    accountById: id => (id === 'a1' ? { id: 'a1', username: 'bob', role: 'user', enabled: true, instance_ids: [] } : null),
    $: sel => els[sel],
    api: async (p, o) => { calls.push({ p, o }); return {}; },
    appConfirm: async () => true,
    closeModal() {}, toast() {}, loadAccounts() {},
    readInstanceChecks: () => ['i1'],
  });

  await ctx.doCreateAccount();
  assert.equal(calls.at(-1).p, '/accounts');
  assert.equal(calls.at(-1).o.method, 'POST');
  assert.deepEqual(plain(calls.at(-1).o.body), { username: 'bob', password: 'pw', role: 'user', instance_ids: ['i1'] });

  await ctx.saveEditAccount('a1');
  assert.equal(calls.at(-2).p, '/accounts/a1');
  assert.equal(calls.at(-2).o.method, 'PATCH');
  assert.deepEqual(plain(calls.at(-2).o.body), { enabled: true, role: 'user' });
  assert.equal(calls.at(-1).p, '/accounts/a1/instances');
  assert.equal(calls.at(-1).o.method, 'PUT');
  assert.deepEqual(plain(calls.at(-1).o.body), { instance_ids: ['i1'] });

  await ctx.toggleAccount('a1', false);
  assert.equal(calls.at(-1).p, '/accounts/a1');
  assert.equal(calls.at(-1).o.method, 'PATCH');
  assert.deepEqual(plain(calls.at(-1).o.body), { enabled: false });

  await ctx.doResetPassword('a1');
  assert.equal(calls.at(-1).p, '/accounts/a1/password');
  assert.equal(calls.at(-1).o.method, 'PUT');
  assert.deepEqual(plain(calls.at(-1).o.body), { password: 'pw' });

  await ctx.deleteAccount('a1', 'bob');
  assert.equal(calls.at(-1).p, '/accounts/a1');
  assert.equal(calls.at(-1).o.method, 'DELETE');
});

test('instance detail is split into role-specific tab sets with dataset cleanup', () => {
  assert.match(source, /const USER_VIEW_TABS = \[\['announcement', '公告'\], \['overview', '运行信息'\]\]/);
  assert.match(source, /main\.dataset\.viewRole = 'user'/);
  assert.match(source, /main\.dataset\.viewRole = 'admin'/);
  assert.match(source, /delete main\.dataset\.viewRole/);
  assert.match(source, /e\.status === 403 \|\| e\.status === 404/);
});

const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

const source = fs.readFileSync(path.join(__dirname, '../web/app.js'), 'utf8');
const functions = ['loadGameBackups', 'watchGameBackupJob', 'startGameBackup'].map(name => {
  const match = source.match(new RegExp(`(?:async )?function ${name}\\([^)]*\\) \\{[\\s\\S]*?\\n\\}`));
  assert.ok(match, `${name} exists`);
  return match[0];
}).join('\n');
const tick = () => new Promise(resolve => setImmediate(resolve));
function setup(data, job = {status: 'done', logs: ['finished']}) {
  const body = {innerHTML: ''}, box = {innerHTML: ''}, button = {disabled: false}, calls = [], timers = [];
  const context = vm.createContext({
    routeToken: 1, timers,
    $: selector => ({'#gb-body': body, '#gb-job': box, '#gb-create': button})[selector],
    esc: s => String(s).replaceAll('<', '&lt;').replaceAll('>', '&gt;'),
    fmtSize: s => String(s), statusPill: s => String(s), toast() {},
    setTimeout: fn => { timers.push(fn); return timers.length; },
    api: async (url, options) => { calls.push({url, options}); return url.startsWith('/jobs/') ? job : options?.method === 'POST' ? {job_id: 'new'} : data; },
  });
  vm.runInContext(functions, context);
  return {context, body, box, button, calls, timers};
}
const supported = {provider: {version: '2.4.14', enabled: true, command_enabled: true, keep: 12, backup_dir: 'backups'}, status: 'running', backups: []};

test('completed task reopens once without recursive list refresh', async () => {
  const f = setup({...supported, last_job: 'old'});
  await f.context.loadGameBackups('instance', 1);
  await tick();
  assert.deepEqual(f.calls.map(c => c.url), ['/instances/instance/game-backups', '/jobs/old']);
  assert.match(f.box.innerHTML, /finished/);
  assert.equal(f.timers.length, 0);
});

test('a task completing after leaving the tab refreshes the list and retains its logs', async () => {
  const f = setup({...supported, active_job: 'active', last_job: 'active'});
  await f.context.loadGameBackups('instance', 1);
  await tick();
  assert.deepEqual(f.calls.map(c => c.url), ['/instances/instance/game-backups', '/jobs/active', '/instances/instance/game-backups']);
  assert.match(f.box.innerHTML, /finished/);
});

test('damaged archives show their problem and disable preview and restore', async () => {
  const f = setup({...supported, status: 'stopped', backups: [{name: 'bad.zip', size: 7, created: 'today', problem: '<broken>'}]});
  await f.context.loadGameBackups('instance', 1);
  assert.match(f.body.innerHTML, /&lt;broken&gt;/);
  assert.match(f.body.innerHTML, /disabled onclick="previewGameBackup/);
  assert.match(f.body.innerHTML, /disabled onclick="restoreGameBackup/);
  assert.match(f.body.innerHTML, /id="gb-create"[^>]+disabled/);
});

test('unsupported providers display the supported scope and disable creation', async () => {
  const f = setup({provider: null, status: 'running', backups: []});
  await f.context.loadGameBackups('instance', 1);
  assert.match(f.body.innerHTML, /目前支持 ServerUtilities/);
  assert.match(f.body.innerHTML, /id="gb-create"[^>]+disabled/);
});

test('stale list responses leave the new route untouched', async () => {
  const f = setup(supported);
  f.context.routeToken = 2;
  await f.context.loadGameBackups('instance', 1);
  assert.equal(f.body.innerHTML, '');
});

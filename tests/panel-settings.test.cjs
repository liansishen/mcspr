const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

const source = fs.readFileSync(path.join(__dirname, '../web/app.js'), 'utf8');

class FakeEl {
  constructor(id = '') {
    this.id = id;
    this.innerHTML = '';
    this.value = '';
    this.style = {};
  }
}

function setup() {
  const elements = new Map();
  const getEl = id => {
    if (!elements.has(id)) elements.set(id, new FakeEl(id));
    return elements.get(id);
  };
  const calls = [];

  const context = vm.createContext({
    routeToken: 1,
    consoleMaxLines: 800,
    $: sel => getEl(sel.replace(/^[#.]/, '')),
    $$: () => [],
    esc: s => String(s),
    toast: () => {},
    api: async (url, options) => {
      calls.push({ url, options });
      return {};
    },
  });

  const code = ['savePanelSettings'].map(fnName => {
    const match = source.match(new RegExp(`(?:async )?function ${fnName}\\([^)]*\\) \\{[\\s\\S]*?\\n\\}`));
    assert.ok(match, `${fnName} should exist`);
    return match[0];
  }).join('\n');
  vm.runInContext(code, context);
  return { context, getEl, calls };
}

test('console rendering uses the configured maximum line count', () => {
  assert.match(source, /let consoleMaxLines = 800;/);
  assert.match(source, /const maxLines = consoleMaxLines;/);
  assert.match(source, /consoleMaxLines = n;/);
});

test('panel settings form exposes both console line settings', () => {
  assert.match(source, /id="ps-console-lines"/);
  assert.match(source, /id="ps-console-buffer"/);
  assert.match(source, /console_max_lines/);
  assert.match(source, /console_buffer_lines/);
});

test('saving panel settings sends the console limits and refreshes the cache', async () => {
  const { context, getEl, calls } = setup();
  getEl('ps-listen').value = '127.0.0.1:8080';
  getEl('ps-dir').value = 'data';
  getEl('ps-cfkey').value = '';
  getEl('ps-console-lines').value = '1500';
  getEl('ps-console-buffer').value = '9000';

  await context.savePanelSettings();

  assert.equal(calls.length, 1);
  assert.equal(calls[0].url, '/settings');
  assert.equal(calls[0].options.method, 'PUT');
  assert.equal(calls[0].options.body.console_max_lines, 1500);
  assert.equal(calls[0].options.body.console_buffer_lines, 9000);
  assert.equal('token' in calls[0].options.body, false);
  assert.equal(context.consoleMaxLines, 1500);
});

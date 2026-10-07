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

  const context = vm.createContext({
    routeToken: 1,
    curPath: '',
    filesEntriesCache: null,
    $: sel => getEl(sel.replace(/^[#.]/, '')),
    $$: () => [],
    esc: s => String(s).replaceAll('<', '&lt;').replaceAll('>', '&gt;'),
    fmtSize: n => `${n} B`,
    api: async () => ({
      entries: [
        { name: 'config', dir: true, size: 4096, modified: '2026-10-07' },
        { name: 'server.properties', dir: false, size: 800, modified: '2026-10-07' },
        { name: 'eula.txt', dir: false, size: 180, modified: '2026-10-07' },
      ],
    }),
  });

  const code = [
    'renderTabFiles',
    'loadFiles',
    'filterFiles',
    'renderFilesList',
  ].map(fnName => {
    const match = source.match(new RegExp(`(?:async )?function ${fnName}\\([^)]*\\) \\{[\\s\\S]*?\\n\\}`));
    assert.ok(match, `${fnName} should exist`);
    return match[0];
  }).join('\n');

  vm.runInContext(code, context);
  return { context, elements, getEl };
}

test('renderTabFiles initializes files-crumb and files-body without error', async () => {
  const { context, getEl } = setup();
  const tabEl = new FakeEl('tab-body');
  await context.renderTabFiles('inst_1', tabEl, 1);

  assert.match(tabEl.innerHTML, /id="files-crumb"/);
  assert.match(tabEl.innerHTML, /id="files-body"/);
  assert.match(tabEl.innerHTML, /id="files-search"/);

  const crumb = getEl('files-crumb');
  assert.match(crumb.innerHTML, /根目录/);

  const body = getEl('files-body');
  assert.match(body.innerHTML, /server\.properties/);
  assert.match(body.innerHTML, /eula\.txt/);
  assert.match(body.innerHTML, /config/);
});

test('filterFiles filters entries based on search input', async () => {
  const { context, getEl } = setup();
  const tabEl = new FakeEl('tab-body');
  await context.renderTabFiles('inst_1', tabEl, 1);

  const searchInput = getEl('files-search');
  searchInput.value = 'server';
  context.filterFiles();

  const body = getEl('files-body');
  assert.match(body.innerHTML, /server\.properties/);
  assert.doesNotMatch(body.innerHTML, /eula\.txt/);

  searchInput.value = 'nonexistent';
  context.filterFiles();
  assert.match(body.innerHTML, /未找到匹配的文件/);
});

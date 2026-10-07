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
    currentInstanceInfo: { mod_loader: 'forge' },
    modsCache: null,
    modsTabReload: null,
    $: sel => getEl(sel.replace(/^[#.]/, '')),
    $$: () => [],
    esc: s => String(s).replaceAll('<', '&lt;').replaceAll('>', '&gt;'),
    fmtSize: n => `${n} B`,
    api: async () => ({
      mods: [
        { file: 'jei-1.7.10.jar', display_name: 'Just Enough Items', version: '1.0', loader: 'forge', size: 1024, enabled: true, description: 'Recipe viewer', authors: 'mezz' },
        { file: 'gregtech-5.0.jar', display_name: 'GregTech', version: '5.0', loader: 'forge', size: 2048, enabled: true, description: 'Industrial mod', authors: 'Greg' },
        { file: 'applied-energistics.jar', display_name: 'AE2', version: '2.0', loader: 'forge', size: 4096, enabled: false, description: 'Storage system', authors: 'Algorithm' },
      ],
    }),
  });

  const code = [
    'renderTabMods',
    'filterMods',
    'renderModsList',
  ].map(fnName => {
    const match = source.match(new RegExp(`(?:async )?function ${fnName}\\([^)]*\\) \\{[\\s\\S]*?\\n\\}`));
    assert.ok(match, `${fnName} should exist`);
    return match[0];
  }).join('\n');

  vm.runInContext(code, context);
  return { context, elements, getEl };
}

test('renderTabMods renders mods-search input before download button', async () => {
  const { context, getEl } = setup();
  const tabEl = new FakeEl('tab-body');
  await context.renderTabMods('inst_1', tabEl, 1);

  assert.match(tabEl.innerHTML, /id="mods-search"/);
  assert.match(tabEl.innerHTML, /id="mods-body"/);
  assert.match(tabEl.innerHTML, /placeholder="搜索模组…"/);

  const searchIndex = tabEl.innerHTML.indexOf('id="mods-search"');
  const dlIndex = tabEl.innerHTML.indexOf('showModDownload');
  assert.ok(searchIndex < dlIndex, 'mods-search must appear before download button');

  const body = getEl('mods-body');
  assert.match(body.innerHTML, /Just Enough Items/);
  assert.match(body.innerHTML, /GregTech/);
  assert.match(body.innerHTML, /AE2/);
});

test('filterMods filters entries by display_name, file, and description', async () => {
  const { context, getEl } = setup();
  const tabEl = new FakeEl('tab-body');
  await context.renderTabMods('inst_1', tabEl, 1);

  const searchInput = getEl('mods-search');
  searchInput.value = 'greg';
  context.filterMods();

  const body = getEl('mods-body');
  assert.match(body.innerHTML, /GregTech/);
  assert.doesNotMatch(body.innerHTML, /Just Enough Items/);
  assert.doesNotMatch(body.innerHTML, /AE2/);

  searchInput.value = 'Recipe'; // matches description
  context.filterMods();
  assert.match(body.innerHTML, /Just Enough Items/);
  assert.doesNotMatch(body.innerHTML, /GregTech/);

  searchInput.value = 'nonexistent';
  context.filterMods();
  assert.match(body.innerHTML, /未找到匹配的模组/);
});

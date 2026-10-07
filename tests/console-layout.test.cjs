const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const path = require('node:path');

const source = fs.readFileSync(path.join(__dirname, '../web/app.js'), 'utf8');
const extract = (start, end) => {
  const from = source.indexOf(start);
  const to = source.indexOf(end, from);
  assert.notEqual(from, -1, `${start} should exist`);
  assert.notEqual(to, -1, `${end} should exist`);
  return source.slice(from, to);
};

class FakeElement {
  constructor(id = '') {
    this.id = id;
    this.dataset = {};
    this.style = {};
    this.className = '';
    this.classList = { contains: name => this.className.split(/\s+/).includes(name) };
    this.children = [];
    this.scrollTop = 0;
    this.scrollHeight = 0;
    this.clientHeight = 100;
    this.offsetHeight = 100;
    this.value = '';
    this._text = '';
  }
  set innerHTML(html) {
    this._html = html;
    for (const [, id] of html.matchAll(/id="([^"]+)"/g)) FakeElement.elements.set(id, new FakeElement(id));
  }
  get innerHTML() { return this._html || ''; }
  get textContent() { return this._text || this.children.map(child => child.textContent || '').join(''); }
  set textContent(value) { this._text = String(value); }
  appendChild(child) {
    if (child.fragment) this.children.push(...child.children);
    else this.children.push(child);
    this.scrollHeight = this.children.length * this.offsetHeight;
    return child;
  }
  removeChild(child) { this.children.splice(this.children.indexOf(child), 1); }
  replaceChildren(...children) { this.children = children; this.scrollHeight = children.length * this.offsetHeight; }
  get firstChild() { return this.children[0]; }
  getAttribute() { return ''; }
}
FakeElement.elements = new Map();

class FakeWebSocket {
  static instances = [];
  constructor(url) { this.url = url; FakeWebSocket.instances.push(this); }
  send(data) { this.onmessage?.({ data: JSON.stringify(data) }); }
}

function setup({ api = async () => ({ lines: [], cursor: 0 }) } = {}) {
  const elements = FakeElement.elements = new Map();
  const timers = [];
  const main = new FakeElement('main');
  elements.set('main', main);
  const context = vm.createContext({
    console, URLSearchParams, encodeURIComponent,
    location: { protocol: 'http:', host: 'localhost' },
    document: {
      getElementById: id => elements.get(id) || null,
      createElement: () => new FakeElement(),
      createTextNode: text => ({ textContent: text }),
      createDocumentFragment: () => ({ fragment: true, children: [], appendChild(x) { this.children.push(x); } }),
    },
    WebSocket: FakeWebSocket,
    api,
    $: selector => elements.get(selector.slice(1)) || null,
    $$: () => [],
    setTimeout: fn => { timers.push(fn); return timers.length; },
    clearTimeout: () => {},
    timers: [], routeToken: 1, TOKEN: 'test', consoleMaxLines: 800,
    loadUsers: () => {}, every: () => {},
  });
  const helpers = extract('function consoleLineParts', 'function renderTabConsole');
  const renderer = extract('function renderTabConsole', 'const USER_TABS');
  const filters = extract('function applyConsoleFilter', 'async function downloadConsole');
  vm.runInContext(`${helpers}\n${filters}\n${renderer}`, context);
  return { context, elements, timers };
}

const history = Array.from({ length: 850 }, (_, i) => ({ seq: i + 1, line: `line-${i + 1}` }));

test('initial history renders immediately at bottom and keeps only the latest 800 lines', async () => {
  const { context, elements } = setup({ api: async () => ({ lines: history, cursor: 850 }) });
  context.renderTabConsole('one', new FakeElement(), 1);
  FakeWebSocket.instances.at(-1).onclose();
  await new Promise(resolve => setImmediate(resolve));
  const log = elements.get('console-log');
  assert.equal(log.children.length, 800);
  assert.equal(log.children[0].textContent, 'line-51');
  assert.equal(log.children.at(-1).textContent, 'line-850');
  assert.equal(log.scrollTop, log.scrollHeight);
});

test('GET fallback starts at the bottom and caps initial history', async () => {
  const requests = [];
  const { context, elements } = setup({ api: async url => { requests.push(url); return { lines: history, cursor: 850 }; } });
  context.renderTabConsole('one', new FakeElement(), 1);
  FakeWebSocket.instances.at(-1).onclose();
  await new Promise(resolve => setImmediate(resolve));
  const log = elements.get('console-log');
  assert.deepEqual(requests, ['/instances/one/console?after=0']);
  assert.equal(log.children.length, 800);
  assert.equal(log.children.at(-1).textContent, 'line-850');
  assert.equal(log.scrollTop, log.scrollHeight);
});

test('polling fallback resets sequence after cursor rollback and refetches from zero', async () => {
  const pending = [];
  const requests = [];
  const api = url => { requests.push(url); return new Promise(resolve => pending.push(resolve)); };
  const { context, elements, timers } = setup({ api });
  context.renderTabConsole('one', new FakeElement(), 1);
  const socket = FakeWebSocket.instances.at(-1);
  socket.onclose();
  pending.shift()({ lines: [{ seq: 120, line: 'old-run' }], cursor: 120 });
  await new Promise(resolve => setImmediate(resolve));
  timers.at(-1)();
  await new Promise(resolve => setImmediate(resolve));
  pending.shift()({ lines: [{ seq: 1, line: 'new-run' }], cursor: 1 });
  await new Promise(resolve => setImmediate(resolve));
  assert.deepEqual(requests, [
    '/instances/one/console?after=0',
    '/instances/one/console?after=120',
    '/instances/one/console?after=0',
  ]);
  assert.deepEqual(elements.get('console-log').children.map(line => line.textContent), []);
  pending.shift()({ lines: [{ seq: 1, line: 'new-run' }], cursor: 1 });
  await new Promise(resolve => setImmediate(resolve));
  assert.deepEqual(elements.get('console-log').children.map(line => line.textContent), ['new-run']);
});

test('a pending fallback GET is discarded when WebSocket recovery wins the race', async () => {
  let resolveGet;
  const { context, elements } = setup({ api: () => new Promise(resolve => { resolveGet = resolve; }) });
  context.renderTabConsole('one', new FakeElement(), 1);
  const socket = FakeWebSocket.instances.at(-1);
  socket.onclose();
  socket.onopen();
  socket.send({ type: 'history', lines: [{ seq: 1, line: 'from-ws' }], cursor: 1 });
  resolveGet({ lines: [{ seq: 1, line: 'from-get' }], cursor: 1 });
  await new Promise(resolve => setImmediate(resolve));
  assert.deepEqual(elements.get('console-log').children.map(line => line.textContent), ['from-ws']);
});

test('stale socket messages are ignored after another tab becomes current', () => {
  FakeWebSocket.instances = [];
  const { context, elements } = setup();
  context.renderTabConsole('one', new FakeElement(), 1);
  const oldSocket = FakeWebSocket.instances[0];
  context.routeToken = 2;
  context.renderTabConsole('one', new FakeElement(), 2);
  oldSocket.send({ type: 'history', lines: [{ seq: 1, line: 'stale' }], cursor: 1 });
  assert.equal(elements.get('console-log').children.length, 0);
});

test('history with a reset cursor replaces old lines and resets sequence tracking', async () => {
  const { context, elements } = setup();
  context.renderTabConsole('one', new FakeElement(), 1);
  const socket = FakeWebSocket.instances.at(-1);
  socket.send({ type: 'history', lines: [{ seq: 120, line: 'old-run' }], cursor: 120 });
  socket.send({ type: 'history', lines: [{ seq: 1, line: 'new-run' }], cursor: 1 });
  assert.deepEqual(elements.get('console-log').children.map(x => x.textContent), ['new-run']);
});

test('warn/error filtering and append preserve an upward scroll position', () => {
  const { context, elements } = setup();
  context.renderTabConsole('one', new FakeElement(), 1);
  FakeWebSocket.instances.at(-1).onclose();
  const log = elements.get('console-log');
  FakeWebSocket.instances.at(-1).send({ type: 'history', cursor: 3, lines: [
    { seq: 1, line: '[12:00] [Server thread/INFO]: normal' },
    { seq: 2, line: '[12:00] [Server thread/WARN]: warning' },
    { seq: 3, line: '[12:00] [Server thread/ERROR]: failure' },
  ] });
  log.scrollTop = 20;
  elements.get('console-level').value = 'warn';
  context.applyConsoleFilter();
  assert.deepEqual(log.children.map(x => x.style.display), ['none', '', '']);
  elements.get('console-level').value = 'err';
  context.applyConsoleFilter();
  assert.deepEqual(log.children.map(x => x.style.display), ['none', 'none', '']);
  FakeWebSocket.instances.at(-1).send({ seq: 4, line: 'another line' });
  assert.equal(log.scrollTop, 20);
});

test('details header and tab body retain the viewport-constrained layout contract', () => {
  const css = fs.readFileSync(path.join(__dirname, '../web/style.css'), 'utf8');
  assert.match(source, /class="page-head detail-head"/);
  assert.match(source, /class="tabs detail-tabs"/);
  assert.match(source, /<div id="tab-body"><\/div>/);
  assert.match(css, /#main\.detail-layout\s*\{[^}]*display:\s*flex;[^}]*overflow:\s*hidden;/s);
  assert.match(css, /#tab-body\s*\{[^}]*min-height:\s*0;[^}]*overflow-y:\s*auto;/s);
});

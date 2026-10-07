const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const path = require('node:path');

const source = fs.readFileSync(path.join(__dirname, '../web/app.js'), 'utf8');
const functions = ['consoleLineParts', 'consoleLineMeta', 'consoleBatch'].map(name => {
  const match = source.match(new RegExp(`function ${name}\\([^)]*\\) \\{[\\s\\S]*?\\n\\}`));
  assert.ok(match, `${name} should exist`);
  return match[0];
}).join('\n');
const context = vm.createContext({});
vm.runInContext(functions, context);

test('parses Forge timestamp, thread/level, mod source and message', () => {
  assert.deepEqual(Array.from(context.consoleLineParts('[14:02:03] [Server thread/INFO] [examplemod]: Ready')),
    ['[14:02:03]', '[Server thread/INFO]', 'examplemod', ': Ready']);
});

test('returns unparsed plain text safely without treating markup as HTML', () => {
  const malicious = '<img src=x onerror=alert(1)>';
  assert.equal(context.consoleLineParts(malicious), null);
  assert.equal(String(malicious), '<img src=x onerror=alert(1)>');
});
test('console rendering writes log data as text, never interpolated markup', () => {
  assert.match(source, /span\.textContent\s*=\s*text/);
  assert.match(source, /createTextNode\(line\)/);
});

test('preserves warn/error filters and highlights success/player events', () => {
  assert.ok(Array.from(context.consoleLineMeta('[12:00] [Server thread/WARN] [mod]: warning').classes).includes('warn'));
  assert.ok(Array.from(context.consoleLineMeta('[12:00] [Server thread/ERROR] [mod]: failure').classes).includes('err'));
  assert.ok(Array.from(context.consoleLineMeta('[12:00] [Server thread/INFO]: Done (1.2s)!').classes).includes('log-success'));
  assert.ok(Array.from(context.consoleLineMeta('[12:00] Player joined the game').classes).includes('log-player'));
});
test('batches initial history and accepts legacy realtime messages without duplicates', () => {
  const history = context.consoleBatch({type:'history', lines:[{seq:2,line:'two'},{seq:3,line:'three'}], cursor:3}, 1);
  assert.equal(history.replace, true);
  assert.equal(history.cursor, 3);
  assert.equal(history.lastSeq, 3);
  assert.equal(history.lines.length, 2);
  assert.equal(context.consoleBatch({seq:3,line:'duplicate'}, 3).lines.length, 0);
  const legacy = context.consoleBatch({ts:'now',line:'legacy'}, 3);
  assert.equal(legacy.replace, false);
  assert.equal(legacy.lines[0].line, 'legacy');
});

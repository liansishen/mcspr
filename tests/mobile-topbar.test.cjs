const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');

const cssSource = fs.readFileSync(path.join(__dirname, '../web/style.css'), 'utf8');
const htmlSource = fs.readFileSync(path.join(__dirname, '../web/index.html'), 'utf8');

test('mobile topbar: .menu-btn is declared once in base styles and once in mobile media query', () => {
  const baseMatch = cssSource.match(/(?<![#.\w])\.menu-btn\s*\{[^}]*display:\s*none;?[^}]*\}/);
  assert.ok(baseMatch, 'Base .menu-btn { display: none } must exist');

  const mobileBlocks = [...cssSource.matchAll(/@media\s*\([^)]*max-width:\s*900px[^)]*\)\s*\{([\s\S]*?)\n\}/g)];
  assert.ok(mobileBlocks.length > 0, 'Mobile media query (max-width: 900px) must exist');

  const flexMatches = [...cssSource.matchAll(/(?<![#.\w])\.menu-btn\s*\{[^}]*display:\s*inline-flex;?[^}]*\}/g)];
  assert.equal(flexMatches.length, 1, 'Exactly one .menu-btn { display: inline-flex } rule should exist');

  const authHideMatch = cssSource.match(/body\.auth-view\s+[^{]*\.menu-btn[^{]*\{[^}]*display:\s*none/);
  assert.ok(authHideMatch, 'body.auth-view must keep .menu-btn hidden');
});

test('mobile topbar: responsive compression rules exist for small viewports', () => {
  const mobileSection = [...cssSource.matchAll(/@media\s*\([^)]*max-width:\s*900px[^)]*\)\s*\{([\s\S]*?)\n\}/g)]
    .map(m => m[1]).join('\n');
  assert.ok(mobileSection.length > 0, 'Mobile media query block must exist');
  const css = mobileSection;

  assert.ok(css.includes('.topbar-title { display: none; }'), 'topbar-title should be hidden on <= 900px to free horizontal room');
  assert.ok(css.includes('.task-label { display: none; }'), 'task-label should be hidden on <= 900px to keep task trigger compact');
  assert.ok(/\.theme-select\s*\{[^}]*max-width:\s*96px/.test(css), 'theme-select should be constrained to prevent pushing account trigger');
  assert.ok(/\.acct-trigger\s*\{[^}]*max-width:\s*130px/.test(css), 'acct-trigger should have max-width constraint on mobile');
  assert.ok(/\.acct-name\s*\{[^}]*max-width:\s*90px/.test(css), 'acct-name should be clamped on mobile');
  assert.match(css, /\.topbar-right\s*\{[^}]*min-width:\s*0;/);
  assert.match(css, /\.acct-trigger\s*\{[^}]*width:\s*100%;/);
});

test('mobile topbar: task trigger preserves aria-label and accessible icon', () => {
  assert.ok(/id="task-btn"[^>]*aria-label="任务中心"/.test(htmlSource), 'task-btn must have aria-label');
  assert.ok(/id="menu-btn"[^>]*aria-label="打开菜单"/.test(htmlSource), 'menu-btn must have aria-label');
  assert.ok(/id="theme-select"[^>]*aria-label="主题选择"/.test(htmlSource), 'theme-select must have aria-label');
});

test('mobile topbar: account trigger and dropdown support flexible shrinking and bounds clamping', () => {
  assert.ok(/\.acct-trigger\s*\{[^}]*min-width:\s*0/.test(cssSource), 'acct-trigger should have min-width: 0 for flex shrinking');
  assert.ok(/\.acct-dropdown\s*\{[^}]*max-width:\s*min\(320px,\s*calc\(100vw\s*-\s*20px\)\)/.test(cssSource),
    'acct-dropdown should clamp to viewport width on narrow screens');
});

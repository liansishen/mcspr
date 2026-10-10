const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

const appSource = fs.readFileSync(path.join(__dirname, '../web/app.js'), 'utf8');
const cssSource = fs.readFileSync(path.join(__dirname, '../web/style.css'), 'utf8');
const htmlSource = fs.readFileSync(path.join(__dirname, '../web/index.html'), 'utf8');

function extractFn(name) {
  const match = appSource.match(new RegExp(`(?:async )?function ${name}\\([^)]*\\) \\{[\\s\\S]*?\\n\\}`));
  assert.ok(match, `${name} should exist in web/app.js`);
  return match[0];
}

function run(names, context = {}) {
  const ctx = vm.createContext({ routeToken: 0, sessionGeneration: 0, currentUser: null, setTopbarTitle: () => {}, document: { body: { classList: { remove() {} } } }, ...context });
  vm.runInContext(names.map(extractFn).join('\n'), ctx);
  return ctx;
}

test('style.css defines base .menu-btn before media query display override', () => {
  const baseMatch = cssSource.match(/\.menu-btn\s*\{[^}]*display:\s*none;?[^}]*\}/);
  assert.ok(baseMatch, 'Base .menu-btn { display: none } rule must exist');
  const baseIndex = baseMatch.index;

  const mediaMatches = [...cssSource.matchAll(/@media\s*\([^)]*max-width:\s*900px[^)]*\)[\s\S]*?\.menu-btn\s*\{[^}]*display:\s*inline-flex;?[^}]*\}/g)];
  assert.ok(mediaMatches.length > 0, 'Media query for .menu-btn display: inline-flex must exist');

  assert.ok(baseIndex < mediaMatches[0].index, 'Base display: none must precede media query display: inline-flex');

  const afterMedia = cssSource.slice(mediaMatches[mediaMatches.length - 1].index);
  const trailingOverride = afterMedia.match(/(?<![#.\w])\.menu-btn\s*\{[^}]*display:\s*none/);
  assert.equal(trailingOverride, null, 'No trailing unqualified .menu-btn display: none should override media query');
});

test('drawer open/close updates classes, ARIA attributes, and manages focus', () => {
  let focused = null;
  const navLink = { focus: () => { focused = 'nav'; } };
  const menuBtn = {
    attrs: {},
    setAttribute(k, v) { this.attrs[k] = v; },
    focus: () => { focused = 'menu-btn'; },
  };
  const backdrop = { hidden: true };
  const bodyClasses = new Set();
  const doc = {
    body: {
      classList: {
        add: c => bodyClasses.add(c),
        remove: c => bodyClasses.delete(c),
        contains: c => bodyClasses.has(c),
      },
    },
    getElementById: id => {
      if (id === 'sidebar-backdrop') return backdrop;
      if (id === 'menu-btn') return menuBtn;
      return null;
    },
    querySelector: sel => {
      if (sel === '#nav a') return navLink;
      return null;
    },
  };

  const ctx = run(['openDrawer', 'closeDrawer', 'toggleDrawer'], {
    document: doc,
  });

  ctx.openDrawer();
  assert.ok(bodyClasses.has('drawer-open'), 'Body should have drawer-open');
  assert.equal(backdrop.hidden, false, 'Backdrop should not be hidden');
  assert.equal(menuBtn.attrs['aria-expanded'], 'true', 'aria-expanded should be true');
  assert.equal(focused, 'nav', 'First nav link should be focused');

  ctx.toggleDrawer();
  assert.ok(!bodyClasses.has('drawer-open'), 'Body should not have drawer-open');
  assert.equal(backdrop.hidden, true, 'Backdrop should be hidden');
  assert.equal(menuBtn.attrs['aria-expanded'], 'false', 'aria-expanded should be false');
  assert.equal(focused, 'menu-btn', 'Menu button should receive focus');

  ctx.toggleDrawer();
  assert.ok(bodyClasses.has('drawer-open'), 'Drawer should be re-opened');
  assert.equal(backdrop.hidden, false);
});

test('navigation closes the mobile drawer even when the active link is clicked', () => {
  const handlers = {};
  let drawerCloses = 0;
  const ctx = run(['initGlobalUi'], {
    document: {
      addEventListener: (type, fn) => { handlers[type] = fn; },
      getElementById: () => null,
      documentElement: { classList: { contains: () => false } },
    },
    window: { addEventListener() {} },
    closeDrawer: () => { drawerCloses++; },
    closeAccountMenu() {}, closeTaskCenter() {}, renderTaskBadge() {},
  });
  ctx.initGlobalUi();
  const navTarget = { closest: selector => selector === '#nav a' ? {} : null };
  handlers.click({ target: navTarget });
  handlers.click({ target: navTarget });
  assert.equal(drawerCloses, 2);
  handlers.click({ target: { closest: () => null } });
  assert.equal(drawerCloses, 2);
});

test('shared card and resource bar styles remain available after grid resizing', () => {
  assert.match(cssSource, /\.card\s*\{[^}]*padding:\s*18px 20px;/);
  assert.match(cssSource, /\.card \.big\s*\{[^}]*font-size:\s*24px;/);
  assert.match(cssSource, /\.bar\s*\{[^}]*height:\s*6px;/);
  assert.match(cssSource, /\.inst-card a\s*\{[^}]*font-size:\s*16px;/);
});

test('renderAccountsTable renders a single table DOM with card-compatible classes and all actions', () => {
  let innerHTML = '';
  const container = { set innerHTML(val) { innerHTML = val; }, get innerHTML() { return innerHTML; } };
  const accountsData = {
    instances: [{ id: 'inst-1', name: '生存服' }, { id: 'inst-2', name: '创造服' }],
    accounts: [
      { id: 'u1', username: 'alice', role: 'admin', enabled: true, instance_ids: [] },
      { id: 'u2', username: 'bob', role: 'user', enabled: false, instance_ids: ['inst-1'] },
    ],
  };

  const ctx = run(['renderAccountsTable'], {
    $: sel => (sel === '#accounts-body' ? container : null),
    accountsData,
    esc: s => String(s),
  });

  ctx.renderAccountsTable();

  assert.match(innerHTML, /class="table-wrap acct-table-wrap"/, 'Wrapper has acct-table-wrap');
  assert.match(innerHTML, /<table class="table acct-table">/, 'Table has acct-table class');
  assert.match(innerHTML, /<thead><tr><th>用户名<\/th><th>角色<\/th><th>状态<\/th><th>授权实例<\/th><th>操作<\/th><\/tr><\/thead>/, 'Standard thead preserved for desktop');

  assert.match(innerHTML, /alice/, 'Username alice rendered');
  assert.match(innerHTML, /class="acct-card-head"/, 'Card head exists for mobile');
  assert.match(innerHTML, /showEditAccount\('u1'\)/, 'Edit button for u1 exists');
  assert.match(innerHTML, /showResetPassword\('u1','alice'\)/, 'Reset password button for u1 exists');
  assert.match(innerHTML, /toggleAccount\('u1',false\)/, 'Disable button for enabled u1 exists');
  assert.match(innerHTML, /deleteAccount\('u1','alice'\)/, 'Delete button for u1 exists');
  assert.match(innerHTML, /全部实例/, 'Admin has 全部实例');

  assert.match(innerHTML, /bob/, 'Username bob rendered');
  assert.match(innerHTML, /toggleAccount\('u2',true\)/, 'Enable button for disabled u2 exists');
  assert.match(innerHTML, /生存服/, 'Authorized instance name mapped');

  assert.match(cssSource, /@media\s*\([^)]*max-width:\s*600px[^)]*\)[\s\S]*?\.acct-table[\s\S]*?display:\s*block;/, 'Table becomes block at <=600px');
  assert.match(cssSource, /@media\s*\([^)]*max-width:\s*600px[^)]*\)[\s\S]*?\.acct-table thead\s*\{[^}]*display:\s*none;/, 'Thead hidden at <=600px');
  assert.match(cssSource, /\.acct-label\s*\{[^}]*display:\s*none;/, 'Desktop hides acct-label');
  assert.match(cssSource, /@media\s*\([^)]*max-width:\s*600px[^)]*\)[\s\S]*?\.acct-label\s*\{[^}]*display:\s*inline-block;/, 'Mobile shows acct-label');
});

test('.cards-grid is narrow-screen adaptive and does not overflow on mobile', () => {
  assert.match(cssSource, /\.cards-grid\s*\{[^}]*grid-template-columns:\s*repeat\(auto-fill,\s*minmax\(min\([^)]+\),\s*1fr\)\);/);
  assert.doesNotMatch(cssSource, /\.cards-grid\s*\{[^}]*minmax\(360px,\s*1fr\)/, 'Rigid 360px minimum width should be removed');

  assert.match(cssSource, /\.cards-grid\s*>\s*\.empty\s*\{[^}]*grid-column:\s*1\s*\/\s*-1;/, 'Empty state in cards-grid must span 1 / -1');

  assert.match(cssSource, /\.stats-grid\s*\{[^}]*grid-template-columns:\s*repeat\(auto-fit,\s*minmax\(300px,\s*1fr\)\);/, 'stats-grid must be preserved');

  assert.match(cssSource, /\.inst-head\s*>\s*label,\s*\.inst-head\s*>\s*a\s*\{[^}]*overflow-wrap:\s*anywhere;/, 'Instance name links handle overflow');
  assert.match(cssSource, /\.inst-head\s*\.pill\s*\{[^}]*flex-shrink:\s*0;/, 'Pill does not get crushed by long instance titles');

  assert.match(cssSource, /@media\s*\([^)]*max-width:\s*600px[^)]*\)[\s\S]*?\.cards-grid\s*\{[^}]*grid-template-columns:\s*1fr;/, 'cards-grid becomes 1fr on narrow viewports');
});

test('showInstanceError provides role-aware return links and safe guidance', () => {
  let mainContent = '';
  const main = { classList: { remove() {} }, set innerHTML(val) { mainContent = val; }, get innerHTML() { return mainContent; } };

  {
    const ctx = run(['showInstanceError'], {
      $: sel => (sel === '#main' ? main : null),
      currentUser: { role: 'admin' },
      esc: s => String(s),
    });
    ctx.showInstanceError({ status: 403, message: 'Forbidden' });
    assert.match(mainContent, /没有访问权限/, 'Preserves 403 error text');
    assert.match(mainContent, /如需访问请联系管理员/, 'Contains contact admin hint');
    assert.match(mainContent, /href="#\/instances"/, 'Admin links back to #/instances');
    assert.match(mainContent, /返回实例列表/, 'Admin link text is 返回实例列表');
  }

  {
    const ctx = run(['showInstanceError'], {
      $: sel => (sel === '#main' ? main : null),
      currentUser: { role: 'user' },
      esc: s => String(s),
    });
    ctx.showInstanceError({ status: 404, message: 'Not Found' });
    assert.match(mainContent, /实例不存在或未授权/, 'Preserves 404 error text');
    assert.match(mainContent, /如需访问请联系管理员/, 'Contains contact admin hint');
    assert.match(mainContent, /href="#\/my-instances"/, 'User links back to #/my-instances');
    assert.match(mainContent, /返回我的实例/, 'User link text is 返回我的实例');
  }

  {
    const ctx = run(['showInstanceError'], {
      $: sel => (sel === '#main' ? main : null),
      currentUser: null,
      esc: s => String(s),
    });
    ctx.showInstanceError({ status: 403, message: 'Forbidden' });
    assert.match(mainContent, /没有访问权限/);
    assert.match(mainContent, /href="#\/login"/, 'Unauthenticated links back to #/login');
    assert.match(mainContent, /返回登录/, 'Unauthenticated link text is 返回登录');
  }

  {
    const ctx = run(['showInstanceError'], {
      $: sel => (sel === '#main' ? main : null),
      currentUser: { role: 'user' },
      esc: s => String(s),
    });
    ctx.showInstanceError({ status: 404, message: 'SQL syntax error at line 42 internal table leak' });
    assert.doesNotMatch(mainContent, /SQL syntax error/, 'Does not leak sensitive error message on 404');
    assert.match(mainContent, /实例不存在或未授权/);
  }
});

test('administrator instance lookup uses the shared access error view and ignores stale routes', async () => {
  const shown = [];
  const error = Object.assign(new Error('Forbidden'), { status: 403 });
  const ctx = run(['renderInstance'], {
    routeToken: 2,
    api: async () => { throw error; },
    showInstanceError: e => shown.push(e),
  });
  await ctx.renderInstance('unavailable', 'console', 2);
  assert.equal(shown.length, 1);
  assert.equal(shown[0], error);
  await ctx.renderInstance('unavailable', 'console', 1);
  assert.equal(shown.length, 1);
});

test('mobile account card selectors override the table cell display rule', () => {
  assert.match(cssSource, /\.acct-table \.acct-col-status\s*\{\s*display:\s*none;/);
  assert.match(cssSource, /\.acct-table \.acct-col-role,\s*\.acct-table \.acct-col-inst\s*\{\s*display:\s*flex;/);
  assert.match(cssSource, /@media\s*\(max-width:\s*600px\)\s*\{\s*:root\.mc \.acct-table tr/);
});

test('dashboard storage ranking wraps the same long names shown in instance cards', () => {
  assert.match(appSource, /class="muted small instance-size-name"/);
  assert.match(cssSource, /\.instance-size-name\s*\{[^}]*min-width:\s*0;[^}]*overflow-wrap:\s*anywhere;/);
});

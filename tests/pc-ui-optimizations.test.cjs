const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

const appSource = fs.readFileSync(path.join(__dirname, '../web/app.js'), 'utf8');
const cssSource = fs.readFileSync(path.join(__dirname, '../web/style.css'), 'utf8');

function extractFn(name) {
  const match = appSource.match(new RegExp(`(?:async )?function ${name}\\([^)]*\\) \\{[\\s\\S]*?\\n\\}`));
  assert.ok(match, `${name} should exist in web/app.js`);
  return match[0];
}

function run(names, context = {}) {
  const ctx = vm.createContext({ routeToken: 0, sessionGeneration: 0, setTopbarTitle: () => {}, ...context });
  vm.runInContext(names.map(extractFn).join('\n'), ctx);
  return ctx;
}

// ---------------- 1. 控制台日志区伸缩与首屏可见性 ----------------
test('PC opt 1: console height adapts to remaining viewport with internal scroll and first-fold input', () => {
  assert.match(cssSource, /\.console-wrap\s*\{[^}]*height:\s*100%;/, 'Console wrap takes container height');
  assert.match(cssSource, /\.console-wrap\s*\{[^}]*max-height:\s*100%;/, 'Console wrap does not exceed container height');
  assert.match(cssSource, /\.console\s*\{[^}]*flex:\s*1\s+1\s+auto;/, 'Console flexes within wrap');
  assert.match(cssSource, /\.console\s*\{[^}]*min-height:\s*120px;/, 'Console maintains minimum usable height');
  assert.match(cssSource, /\.console\s*\{[^}]*overflow-y:\s*auto;/, 'Console logs scroll internally');
  assert.doesNotMatch(cssSource, /@media\s*\([^)]*max-width:\s*760px[^)]*\)[\s\S]*?\.console\s*\{[^}]*height:\s*420px;/, 'Mobile does not override with fixed 420px');
});

// ---------------- 2. 实例详情顶栏角色面包屑导航与普通页面重置 ----------------
test('PC opt 2: setTopbarTitle renders safe role-aware breadcrumbs for instances and resets for normal pages', () => {
  let innerHTML = '';
  const topbarEl = {
    children: [],
    replaceChildren(...nodes) { this.children = nodes; },
    append(...nodes) { this.children.push(...nodes); },
    set textContent(val) { this._text = val; this.children = []; },
    get textContent() {
      if (this.children.length) return this.children.map(c => c.textContent || '').join('');
      return this._text || '';
    },
  };

  const doc = {
    getElementById: id => (id === 'topbar-title' ? topbarEl : null),
    createElement: tag => ({
      tagName: tag.toUpperCase(),
      attributes: {},
      className: '',
      setAttribute(k, v) { this.attributes[k] = v; },
      textContent: '',
    }),
  };

  const ctx = run(['setTopbarTitle'], {
    document: doc,
    currentUser: { role: 'admin' },
  });

  // Admin breadcrumb
  ctx.setTopbarTitle('生存服-GTNH', { breadcrumb: true, role: 'admin' });
  assert.equal(topbarEl.children.length, 3, 'Breadcrumb consists of link, separator, and title');
  assert.equal(topbarEl.children[0].href, '#/instances', 'Admin breadcrumb links to #/instances');
  assert.equal(topbarEl.children[0].textContent, '实例管理');
  assert.equal(topbarEl.children[1].textContent, ' / ');
  assert.equal(topbarEl.children[2].textContent, '生存服-GTNH');

  // User breadcrumb
  ctx.currentUser = { role: 'user' };
  ctx.setTopbarTitle('休闲服', { breadcrumb: true, role: 'user' });
  assert.equal(topbarEl.children[0].href, '#/my-instances', 'User breadcrumb links to #/my-instances');
  assert.equal(topbarEl.children[0].textContent, '我的实例');
  assert.equal(topbarEl.children[2].textContent, '休闲服');

  // Normal page reset
  ctx.setTopbarTitle('仪表盘');
  assert.equal(topbarEl.children.length, 0, 'Normal page resets breadcrumb DOM');
  assert.equal(topbarEl.textContent, '仪表盘');
});

// ---------------- 3. 统一弹窗底部操作按钮间距与主按钮最小宽度 ----------------
test('PC opt 3: modal action bars unify right alignment, 10px gap, 20px top margin and 80px min-width', () => {
  assert.match(cssSource, /\.modal\s+\.row\.right,\s*\.modal-footer,\s*\.modal-actions\s*\{[^}]*justify-content:\s*flex-end;/, 'Modal actions are right aligned');
  assert.match(cssSource, /\.modal\s+\.row\.right,\s*\.modal-footer,\s*\.modal-actions\s*\{[^}]*gap:\s*10px;/, 'Modal actions have 10px gap');
  assert.match(cssSource, /\.modal\s+\.row\.right,\s*\.modal-footer,\s*\.modal-actions\s*\{[^}]*margin-top:\s*20px;/, 'Modal actions have uniform margin-top 20px');
  assert.match(cssSource, /min-width:\s*80px;/, 'Primary modal buttons have at least 80px min-width');
  assert.doesNotMatch(appSource, /class="row right"[^>]*style="margin-top:\s*1[024]px"/, 'All modal footers have inline margins removed');
});

// ---------------- 4. 服务器属性表格名称与值列宽及长说明换行优化 ----------------
test('PC opt 4: server properties table allocates 30% for key name, allows description wrap and contains scroll', () => {
  assert.match(cssSource, /\.table\.props\s+th:first-child,\s*\.table\.props\s+td:first-child\s*\{[^}]*width:\s*30%;/, 'Key column width is 30%');
  assert.match(cssSource, /\.table\.props\s+th:first-child,\s*\.table\.props\s+td:first-child\s*\{[^}]*min-width:\s*190px;/, 'Key column has reasonable min-width');
  assert.match(cssSource, /\.table\.props\s+th:last-child,\s*\.table\.props\s+td:last-child\s*\{[^}]*width:\s*70%;/, 'Value column width is 70%');
  assert.match(cssSource, /\.table\.props\s+td:first-child\s+\.muted\.small\s*\{[^}]*overflow-wrap:\s*anywhere;/, 'Property description wraps properly');
  assert.match(cssSource, /\.prop-input\s*\{[^}]*width:\s*100%;\s*max-width:\s*100%;/, 'Input utilizes available width');
});

// ---------------- 5. 双列表单说明区域间距统一与移动自适应 ----------------
test('PC opt 5: form labels use flex column with min-height on desktop and reset on mobile without breaking checkboxes', () => {
  assert.match(cssSource, /\.settings-form\s*>\s*label:not\(\.check\)\s*\{[^}]*display:\s*flex;\s*flex-direction:\s*column;/, 'Settings form labels use flex column');
  assert.match(cssSource, /label\.check\s*\{[^}]*display:\s*flex;\s*align-items:\s*center;\s*gap:\s*9px;/, 'Checkboxes stay in row direction with flex');
  assert.match(cssSource, /label\.check\s+input\s*\{[^}]*flex-shrink:\s*0;/, 'Checkbox inputs do not shrink');
  assert.doesNotMatch(cssSource, /\.form label\.check/, 'Duplicate form label.check override removed');
  assert.doesNotMatch(cssSource, /label\.check[^{]*\{[^}]*!important/, 'Checkbox rules do not use !important');
  assert.doesNotMatch(cssSource, /\.settings-form[^{]*\{[^}]*!important/, 'Settings form rules do not use !important');
  assert.match(cssSource, /\.settings-form\s*>\s*label:not\(\.check\)\s*>\s*\.muted\.small\s*\{[^}]*min-height:\s*38px;/, 'Desktop labels align adjacent fields');
  assert.match(cssSource, /@media\s*\([^)]*max-width:\s*900px[^)]*\)[\s\S]*?\.settings-form\s*>\s*label:not\(\.check\)\s*>\s*\.muted\.small\s*\{[^}]*min-height:\s*0;/, 'Mobile stack removes artificial spacing');

  // Verify renderPanelSettings and renderTabSettings include settings-form
  const panelFn = extractFn('renderPanelSettings');
  assert.match(panelFn, /class="form card settings-form"/, 'renderPanelSettings includes settings-form');
  const tabFn = extractFn('renderTabSettings');
  assert.match(tabFn, /class="form card settings-form"/, 'renderTabSettings includes settings-form');

  // Verify auth/modal forms do not use settings-form
  const regFn = extractFn('renderRegister');
  assert.doesNotMatch(regFn, /settings-form/, 'renderRegister does not use settings-form');
});

// ---------------- 6. 无数据状态统一居中约束与明确指引 ----------------
test('PC opt 6: empty state uses centered constrained container with icon and clear guidance without fake flows', () => {
  assert.match(cssSource, /\.empty-state\s*\{[^}]*max-width:\s*520px;[^}]*text-align:\s*center;/, 'Empty state is constrained and centered');
  assert.match(cssSource, /\.empty-state\s+\.empty-icon\s*\{[^}]*font-size:\s*36px;/, 'Empty state displays prominent icon');

  // Verify app.js dashboard empty state has link to instances
  assert.match(appSource, /class="empty empty-state"[^>]*><div class="empty-icon">📊<\/div><p>还没有实例<\/p>[\s\S]*?href="#\/instances"/, 'Dashboard guides to instances');

  // Verify app.js instances empty state has create and import buttons
  assert.match(appSource, /class="empty empty-state"[^>]*><div class="empty-icon">🗂<\/div><p>暂无实例<\/p>[\s\S]*?showImportModal\(\)[\s\S]*?showCreateModal\(\)/, 'Instances management has import and create buttons');

  // Verify app.js my-instances empty state directs ungranted user to contact administrator
  assert.match(appSource, /class="empty empty-state"[^>]*><div class="empty-icon">📂<\/div><p>管理员尚未分配实例<\/p><p class="muted small">当前账户暂无可查看的实例，如需开通访问请联系管理员<\/p>/, 'User instance list guides contacting admin');
});

// ---------------- 7. MC主题次要文字加深与状态可读性 ----------------
test('PC opt 7: MC theme darkens muted text to #2b2b2b with high-contrast pills and labels', () => {
  assert.match(cssSource, /:root\.mc\s*\{[^}]*--muted:\s*#2b2b2b;/, 'MC theme sets --muted to #2b2b2b');
  assert.match(cssSource, /:root\.mc\s+\.muted,\s*:root\.mc\s+\.small\.muted,\s*:root\.mc\s+label\s*\{[^}]*color:\s*#2b2b2b;/, 'MC labels and muted text use #2b2b2b');
  assert.match(cssSource, /:root\.mc\s+\.pill\.st-running\s*\{[^}]*color:\s*#1e5210;/, 'MC running pill has dark green text');
  assert.match(cssSource, /:root\.mc\s+\.pill\.st-stopped\s*\{[^}]*color:\s*#2b2b2b;/, 'MC stopped pill has dark text');
});

// ---------------- 8. 亮色主题卡片柔和阴影与表格紧凑布局 ----------------
test('PC opt 8: light theme cards have gentle shadow and hover depth while maintaining compact table', () => {
  assert.match(cssSource, /:root\.light\s+\.card\s*\{[^}]*box-shadow:\s*0\s+1px\s+3px\s+rgba\(31,\s*45,\s*70,\s*\.07\)/, 'Light theme cards have subtle soft shadow');
  assert.match(cssSource, /:root\.light\s+\.card:hover\s*\{[^}]*box-shadow:\s*0\s+4px\s+16px\s+rgba\(31,\s*45,\s*70,\s*\.12\)/, 'Light theme cards have hover elevation');
});

// ---------------- 9. 真实行为：Tab 补全与 showInstanceError 纯函数调用 ----------------
test('PC opt 9: consoleKeydown delegates Tab to consoleTabComplete with preventDefault and completes sa->say , hel->help, op Al/tp Al->Alex', () => {
  const input = { value: '' };
  const doc = { getElementById: id => (id === 'cmd-input' ? input : null) };
  const tabCommandsMatch = appSource.match(/const TAB_COMMANDS = (\[[^\]]+\]);/);
  const parsedCommands = JSON.parse(tabCommandsMatch[1].replace(/'/g, '"'));

  const ctx = run(['consoleKeydown', 'consoleTabComplete'], {
    document: doc,
    TAB_COMMANDS: parsedCommands,
    usersData: { online: ['Alex', 'Bob'] },
    currentInstanceInfo: { player_names: ['Alex', 'Bob'] },
  });

  // 1. sa -> say  (含尾空格) 且触发 preventDefault
  input.value = 'sa';
  let prevented = false;
  ctx.consoleKeydown({ key: 'Tab', preventDefault() { prevented = true; } }, 'inst-1');
  assert.equal(prevented, true, 'Tab event must trigger preventDefault');
  assert.equal(input.value, 'say ', 'sa completes to "say " via command pool with trailing space');

  // 2. hel -> help 且触发 preventDefault
  input.value = 'hel';
  prevented = false;
  ctx.consoleKeydown({ key: 'Tab', preventDefault() { prevented = true; } }, 'inst-1');
  assert.equal(prevented, true, 'Tab event must trigger preventDefault');
  assert.equal(input.value, 'help', 'hel completes to "help"');

  // 3. op Al -> op Alex 且触发 preventDefault
  input.value = 'op Al';
  prevented = false;
  ctx.consoleKeydown({ key: 'Tab', preventDefault() { prevented = true; } }, 'inst-1');
  assert.equal(prevented, true, 'Tab event must trigger preventDefault');
  assert.equal(input.value, 'op Alex', 'op Al completes to "op Alex" via online players');

  // 4. tp Al -> tp Alex 且触发 preventDefault
  input.value = 'tp Al';
  prevented = false;
  ctx.consoleKeydown({ key: 'Tab', preventDefault() { prevented = true; } }, 'inst-1');
  assert.equal(prevented, true, 'Tab event must trigger preventDefault');
  assert.equal(input.value, 'tp Alex', 'tp Al completes to "tp Alex" via online players');
});

test('PC opt 9: showInstanceError calls setTopbarTitle directly without conditional guards', () => {
  let titleCalled = null;
  const main = { innerHTML: '', classList: { remove() {} } };
  const doc = { body: { classList: { remove() {} } } };
  const ctx = run(['showInstanceError'], {
    $: sel => (sel === '#main' ? main : null),
    document: doc,
    currentUser: { role: 'admin' },
    setTopbarTitle: title => { titleCalled = title; },
    esc: s => String(s),
  });

  ctx.showInstanceError({ status: 404, message: 'Not found' });
  assert.equal(titleCalled, '实例管理', 'setTopbarTitle is invoked with 实例管理 for admin 404');
  assert.match(main.innerHTML, /实例不存在或未授权/);
});

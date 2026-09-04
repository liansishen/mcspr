
/* ---------------- Commit3 前端补充 ---------------- */
// 控制台增强：命令历史 / Tab 补全 / 搜索过滤 / 日志下载
let consoleLines = [];
let cmdHistory = [];
let cmdHistIdx = -1;
let consoleFilter = '';

function applyConsoleFilter() {
  const logEl = document.getElementById('console-log');
  if (!logEl) return;
  const q = (document.getElementById('console-search')?.value || '').toLowerCase();
  const lvl = document.getElementById('console-level')?.value || '';
  for (const div of logEl.children) {
    const text = div.textContent.toLowerCase();
    let show = true;
    if (q && !text.includes(q)) show = false;
    if (lvl === 'err' && !div.classList.contains('err')) show = false;
    if (lvl === 'warn' && !div.classList.contains('warn')) show = false;
    div.style.display = show ? '' : 'none';
  }
}
async function downloadConsole(id) {
  try {
    const r = await fetch(`/api/instances/${id}/console/download`, { headers: headers() });
    if (!r.ok) throw new Error(r.statusText);
    const blob = await r.blob();
    const a = document.createElement('a');
    a.href = URL.createObjectURL(blob);
    a.download = `console-${id}.log`;
    a.click();
    URL.revokeObjectURL(a.href);
  } catch (e) { toast(e.message, false); }
}
const TAB_COMMANDS = ['list', 'say ', 'op ', 'deop ', 'kick ', 'ban ', 'ban-ip ', 'pardon ', 'pardon-ip ', 'whitelist add ', 'whitelist remove ', 'whitelist list', 'stop', 'save-all', 'save-on', 'save-off', 'tps', 'restart ', 'difficulty ', 'gamemode ', 'time set ', 'weather ', 'give ', 'tp ', 'fill ', 'setworldspawn ', 'defaultgamemode '];
function consoleTabComplete(id) {
  const input = document.getElementById('cmd-input');
  if (!input) return;
  const text = input.value;
  const parts = text.split(' ');
  const last = parts[parts.length - 1].toLowerCase();
  // 第一个词：补全命令；之后：补全在线玩家名
  let pool = null, prefix = '';
  if (parts.length === 1) {
    pool = TAB_COMMANDS.filter(c => !c.endsWith(' ')).concat(['help']);
    prefix = last;
  } else if (parts.length >= 2 && ['op', 'deop', 'kick', 'ban', 'pardon', 'whitelist'].includes(parts[0])) {
    pool = (modDL ? [] : []).concat((usersData?.online || []), (playersData?.players || []));
    prefix = last;
  }
  if (!pool) return;
  const hit = pool.find(c => c.toLowerCase().startsWith(prefix) && prefix);
  if (hit !== undefined && prefix) {
    parts[parts.length - 1] = hit;
    input.value = parts.join(' ');
  }
}

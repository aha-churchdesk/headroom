// The popover, rendered from states captured in real headless runs
// (tests/ui/fixtures, written by `capture_ui_states` in headroom/tests/e2e.rs),
// and the intents it sends back when clicked.

'use strict';
const { loadUI } = require('./harness');

let pass = 0, fail = 0;
const failures = [];

function test(name, fn) {
  ui.sent.length = 0;
  ui.UI.page = 'main';
  ui.UI.cmdDays = 7;
  ui.UI.cmdSort = 'run_s';
  ui.UI.confirm = null;
  ui.UI.expanded.clear();
  ui.UI.drafts.pattern = '';
  ui.UI.drafts.test = '';
  document.activeElement = null;
  try {
    fn();
    pass++;
  } catch (e) {
    fail++;
    failures.push(`  ✗ ${name}\n      ${e.message}`);
  }
}
function eq(got, want, what = '') {
  const g = JSON.stringify(got), w = JSON.stringify(want);
  if (g !== w) throw new Error(`${what || 'value'}: got ${g}, want ${w}`);
}
function has(hay, needle) {
  if (!hay.includes(needle)) throw new Error(`expected "${needle}" in:\n      ${hay.slice(0, 400)}`);
}
function lacks(hay, needle) {
  if (hay.includes(needle)) throw new Error(`did not expect "${needle}"`);
}

const ui = loadUI();
const busy = ui.fixture('busy');
const held = ui.fixture('held');
const frozen = ui.fixture('frozen');
const history = ui.fixture('history');
const tuned = ui.fixture('tuned');
const tunedHistory = ui.fixture('tuned-history');
const backfill = ui.fixture('backfill');
const others = ui.fixture('others');

test('state updates preserve the selected text and direction in both settings inputs', () => {
  ui.setState(busy);
  ui.UI.page = 'settings';
  for (const [draft, id] of [['pattern', 'patIn'], ['test', 'testIn']]) {
    ui.type(draft, 'custom test');
    const input = document.getElementById(id);
    input.focus();
    for (const [start, end, direction] of [[0, 11, 'backward'], [2, 7, 'forward'], [4, 4, 'none']]) {
      input.setSelectionRange(start, end, direction);
      ui.setState(busy);
      eq(document.activeElement.id, id);
      eq([input.selectionStart, input.selectionEnd, input.selectionDirection],
         [start, end, direction], draft);
    }
  }
});

// ── the main view ─────────────────────────────────────────────────────────

test('running and queued jobs are listed with why the queue waits', () => {
  ui.setState(busy);
  const v = ui.view();
  has(v, 'Running 2');
  // Where each runs, with the command on the line below.
  has(ui.html(), '<b>sc-49123</b><span class="cmd">headroom-hog</span>');
  has(v, 'browser-cd sleep 31');
  const hog = busy.running.find(j => j.label === 'sc-49123');
  has(v, `RAM ${(hog.footprint / 2 ** 30).toFixed(1).replace(/\.0$/, '')} GB CPU ${Math.round(hog.cpu_pct)}%`);
  has(v, 'Queued 2');
  has(v, 'next needs 1.5 GB, 0 GB free');
  has(ui.html(), '<b>fix-main</b><span class="cmd">sleep 30</span>');
  has(v, 'form-service sleep 32');
  has(v, 'new est 1.5 GB');
  // The estimate, and how long it has waited on the line below.
  has(ui.html(), 'est 1.5 GB</span><span title="Time in the queue so far">waiting ');
});

test('a hold shows in the header button and as the queue reason', () => {
  ui.setState(held);
  has(ui.head(), 'Held');
  has(ui.view(), 'held by you');
});

test('a guard freeze is a banner with resume and kill', () => {
  ui.setState(frozen);
  const v = ui.view();
  has(v, 'Froze “headroom-hog” (sc-49201)');
  if (!/memory critical for \d+ s/.test(v)) throw new Error('freeze reason missing: ' + v);
  has(v, '❄︎ frozen');
  eq(ui.find({ act: 'resume', id: '4' }).length, 2, 'banner and row both offer resume');
  has(ui.head(), 'CRITICAL');
});

test('kills since the app started are banners; requeued offers a pin', () => {
  ui.setState(history);
  const v = ui.view();
  has(v, 'Gave up on “headroom-hog” (sc-49201)');
  has(v, 'Killed and requeued “headroom-hog”');
  has(v, 'exit 137');
  eq(ui.find({ act: 'pin', id: '4', on: '1' }).length, 1);
  has(v, 'Nothing running');
  has(v, 'Queue empty');
});

test('a running job shows the largest process inside it, when there is one', () => {
  ui.setState(busy);
  lacks(ui.html(), 'class="now"');
  const [first, ...rest] = busy.running;
  ui.setState({ ...busy, running: [{ ...first, cmd: 'git commit', now: 'tsc --noEmit --project tsconfig.json' }, ...rest] });
  has(ui.view(), `${first.label} git commit ↳ tsc --noEmit --project tsconfig.json`);
});

test('a missing hook is called out in the header', () => {
  ui.setState(busy);
  has(ui.head(), "Codex: heavy commands aren't gated yet");
});

test('a job started with fewer workers says so in its row, detail and history', () => {
  ui.setState(tuned);
  const j = tuned.running.find(r => r.tuned);
  const { workers, default: dflt } = j.tuned;
  has(ui.view(), `${workers}/${dflt} workers`);
  eq(tuned.running.filter(r => !r.tuned).length, 1, 'the untouched job beside it');
  ui.click({ act: 'expand', id: j.id });
  has(ui.view(), `${workers} instead of ${dflt}, so it fits in memory`);
  ui.setState(tunedHistory);
  ui.click({ act: 'goto', page: 'history' });
  has(ui.view(), `${workers}/${dflt} workers`);
  eq((ui.view().match(/workers/g) || []).length, 1, 'only the tuned run mentions workers');
});

test('a head passed over says how often and until when; the job that went first says so', () => {
  ui.setState(backfill);
  const head = backfill.queued[0];
  has(ui.view(), `passed over ${head.skipped}× · strict in ${Math.floor(head.strict_in_s / 60)}m`);
  ui.click({ act: 'goto', page: 'history' });
  has(ui.view(), `started ahead of ${head.label} · ${head.cmd}`);
});

test('a strip above what is running sums up the last 24 hours', () => {
  ui.setState(history);
  has(ui.view(), 'Last 24h 5 runs 3 failed 0 waited 0s wait p50 0s wait p95 0 went first');
  has(ui.html(), '1 exited with an error, 2 killed to free memory');
  ui.setState(tunedHistory);
  has(ui.view(), '2 runs 0 failed');
  lacks(ui.view(), 'shrunk');
  ui.setState(backfill);
  has(ui.view(), '1 went first');
});

test('the strip shows how many jobs waited, and the waits of those', () => {
  const day = (d) => ui.setState({ ...history, last_day: { ...history.last_day, ...d } });
  day({ runs: 426, failed: 0, killed: 0, waited: 44,
    waited_s: 254, longest_wait_s: 131, wait_p50_s: 12, wait_p95_s: 131 });
  has(ui.view(), '426 runs 0 failed 10% waited 12s wait p50 2m11 wait p95');
  has(ui.html(), '44 of 426 jobs waited in the queue');
  has(ui.html(), 'Of the 44 that waited. Time in the queue before starting: 4m14 in all, longest 2m11');
  day({ runs: 1000, waited: 3 });
  has(ui.view(), '1000 runs 3 failed <1% waited');
  day({ runs: 1, waited: 1 });
  has(ui.view(), '100% waited');
  has(ui.html(), '1 of 1 job waited in the queue');
  day({ runs: 2000, more: true, waited: 20 });
  has(ui.html(), '20 of 2000+ jobs waited');
  ui.setState(history);
  has(ui.view(), '0 waited');
  has(ui.html(), 'No job had to wait');
  day({ runs: 0, failed: 0, killed: 0, waited: 0 });
  has(ui.view(), '0 runs 0 failed 0 waited');
});

test('the commands page ranks what ran by time, with runs, memory and failures', () => {
  ui.setState(history);
  ui.click({ act: 'goto', page: 'commands' });
  const week = history.commands[0];
  const t = week.total;
  has(ui.view(), `${t.runs} runs ${t.failed} failed`);
  const rows = ui.view().split(' running · ').length - 1;
  eq(rows, week.rows.length, 'one row per command');
  // Most time running first: the hog ran, the shell one-liners didn't.
  const first = week.rows[0];
  eq(first.key.split('|').pop(), 'headroom-hog');
  has(ui.view(), 'headroom-hog · work-scheduler-mcp · headroom 1 run');
  has(ui.view(), '<0.1 GB·h');
  has(ui.view(), '1 failed');
  lacks(ui.html(), 'class="bad"');
  lacks(ui.view(), 'of 4 commands');
});

test('the commands page re-sorts and switches windows without asking the daemon', () => {
  ui.setState(history);
  ui.click({ act: 'goto', page: 'commands' });
  const order = () => ui.view().match(/(headroom-hog|exit 3|echo ok|sleep 60) ·/g).map(m => m.slice(0, -2));
  eq(order()[0], 'headroom-hog');
  ui.click({ act: 'cmd_sort', val: 'runs' });
  // All ran once: ties keep the time order.
  eq(order()[0], 'headroom-hog');
  ui.click({ act: 'cmd_days', val: '30' });
  eq(ui.UI.cmdDays, 30);
  has(ui.html(), 'class="on" data-act="cmd_days" data-val="30"');
  eq(ui.sent.length, 0, 'sorting and windows stay in the page');
});

test('a command opens its profile, and back returns to the commands page', () => {
  ui.setState(history);
  ui.click({ act: 'goto', page: 'commands' });
  const key = history.commands[0].rows[0].key;
  ui.click({ act: 'profile', key });
  eq(ui.UI.page, 'profile');
  eq(ui.find({ act: 'goto', page: 'commands' }).length, 1, 'back goes to commands');
});

test('a command page lists what ran inside its runs, when that was recorded', () => {
  ui.setState(history);
  ui.click({ act: 'goto', page: 'commands' });
  const key = history.commands[0].rows[0].key;
  const p = history.profiles[key];
  ui.click({ act: 'profile', key });
  lacks(ui.view(), 'Runs inside it');
  const parts = [{ cmd: 'tsc --noEmit', runs: 3, peak: 3 * 2 ** 30, procs: 1, median_s: 45 },
                 { cmd: 'forks.js', runs: 1, peak: 2 ** 30, procs: 8, median_s: 90 }];
  ui.setState({ ...history, profiles: { ...history.profiles, [key]: { ...p, runs: 4, parts } } });
  has(ui.view(), 'Runs inside it tsc --noEmit in 3 of 4 runs · peak 3 GB · 45s forks.js in 1 of 4 runs · peak 1 GB · 1m30 · 8 processes');
});

test('the commands page says when there is nothing to show, or more than it lists', () => {
  const { commands, ...older } = history;
  ui.setState(older);
  ui.click({ act: 'goto', page: 'commands' });
  has(ui.view(), 'No runs yet');
  const empty = { days: 7, count: 0, rows: [], total: { runs: 0, failed: 0, run_s: 0, wait_s: 0, gb_h: 0, peak: 0 } };
  ui.setState({ ...history, commands: [empty] });
  has(ui.view(), 'No runs in the last 7 days');
  const week = history.commands[0];
  ui.setState({ ...history, commands: [{ ...week, count: week.rows.length + 60 }] });
  has(ui.view(), `The ${week.rows.length} with the most time running, of ${week.rows.length + 60} commands.`);
});

test('an empty week keeps the controls for reaching older command activity', () => {
  const week = { ...history.commands[0], count: 0, rows: [] };
  ui.setState({ ...history, commands: [week, ...history.commands.slice(1)] });
  ui.click({ act: 'goto', page: 'commands' });
  has(ui.view(), 'No runs in the last 7 days');
  ui.click({ act: 'cmd_days', val: '30' });
  lacks(ui.view(), 'No runs');
  has(ui.view(), 'headroom-hog');
  ui.click({ act: 'cmd_days', val: '7' });
  has(ui.view(), 'No runs in the last 7 days');
  eq(ui.sent, [], 'changing the period stays in the page');
});

test('zeros are dimmed, failures are not coloured, and every stat explains itself on hover', () => {
  ui.setState(history);
  const html = ui.html();
  has(html, 'class="" data-tip="1 exited with an error');
  has(html, 'class="zero" data-tip="Short jobs that started ahead');
  eq((html.match(/data-tip=/g) || []).length, 6, 'one tooltip per stat');
  lacks(html, 'class="bad"');
  ui.setState(busy);
  eq(busy.last_day.runs, 0, 'the capture has nothing finished yet');
  has(ui.view(), 'Last 24h 0 runs 0 failed 0 waited 0s wait p50');
});

test('the pressure level is coloured text with a tooltip, and so are the header stats', () => {
  ui.setState(busy);
  const head = ui.headHtml();
  has(head, `class="level ${busy.system.pressure}" data-tip="Memory pressure ${busy.system.pressure}`);
  lacks(head, 'pill');
  has(head, 'data-tip="Memory moved to disk.');
  has(head, 'data-tip="CPU in use across all cores"');
});

test('a busier day than the history holds is a floor', () => {
  ui.setState({ ...history, last_day: { ...history.last_day, runs: 2000, more: true } });
  has(ui.view(), '2000+ runs');
  has(ui.html(), 'History keeps 2,000 runs');
});

test('a head never passed over shows no count', () => {
  ui.setState(busy);
  lacks(ui.view(), 'passed over');
});

test('other apps expand to what each process runs, where, and who started it', () => {
  ui.setState(others);
  ui.click({ act: 'goto', page: 'others' });
  const hog = others.others.find(o => o.name === 'headroom-hog');
  const p = hog.top[0];
  lacks(ui.view(), `pid ${p.pid}`);
  ui.click({ act: 'expand_app', name: 'headroom-hog' });
  const v = ui.view();
  has(v, hog.path);
  has(v, `pid ${p.pid}`);
  has(v, p.cmd);
  has(v, `in ${p.cwd}`);
  has(v, `started by ${p.started_by.join(' › ')}`);
  eq(p.started_by[0], 'sh', 'its parent shell, by the name it was started as');
  lacks(v, 'smaller', 'one process: nothing hidden');
  // Collapsing hides it again; the other app was never opened.
  ui.click({ act: 'expand_app', name: 'headroom-hog' });
  lacks(ui.view(), `pid ${p.pid}`);
  eq(ui.sent, [], 'expanding stays in the page');
});

test('other apps open with what the listed apps add up to, and the rest', () => {
  ui.setState(others);
  ui.click({ act: 'goto', page: 'others' });
  const sum = others.others.reduce((n, o) => n + o.bytes, 0);
  const gb = b => +(b / 2 ** 30).toFixed(1);
  has(ui.view(), `Other apps ${gb(sum)} GB System ${gb(others.system.mem_used - sum)} GB`);
  const sys = { ...others.system, mem_used: 2 ** 30 * 10 };
  const two = [{ ...others.others[0], bytes: 2 ** 30 * 3 }, { ...others.others[1], bytes: 2 ** 30 * 1.5 }];
  ui.setState({ ...others, system: sys, others: two });
  has(ui.view(), 'Other apps 4.5 GB System 5.5 GB');
  ui.setState({ ...others, system: sys, others: [] });
  has(ui.view(), 'Other apps 0 GB System 10 GB');
  // Compressed and swapped memory counts in a footprint but not in use.
  ui.setState({ ...others, system: sys, others: [{ ...two[0], bytes: 2 ** 30 * 12 }] });
  has(ui.view(), 'Other apps 12 GB System 0 GB');
});

test('an app with more processes than listed says how many are left out', () => {
  const many = { ...others.others[0], procs: others.others[0].top.length + 4 };
  ui.setState({ ...others, others: [many] });
  ui.click({ act: 'goto', page: 'others' });
  ui.click({ act: 'expand_app', name: many.name });
  has(ui.view(), 'and 4 smaller');
});

// ── intents ───────────────────────────────────────────────────────────────

test('hold toggles from whatever the daemon says now', () => {
  ui.setState(busy);
  ui.click({ act: 'hold' });
  ui.setState(held);
  ui.click({ act: 'hold' });
  eq(ui.sent, [{ cmd: 'hold', on: true }, { cmd: 'hold', on: false }]);
});

test('kill needs a second click; the first only arms it', () => {
  ui.setState(busy);
  ui.click({ act: 'kill', id: '1' });
  eq(ui.sent, [], 'nothing sent on the first click');
  has(ui.view(), 'Kill?');
  ui.click({ act: 'kill', id: '1' });
  eq(ui.sent, [{ cmd: 'kill', id: '1' }]);
});

test('arming one kill does not arm another', () => {
  ui.setState(busy);
  ui.click({ act: 'kill', id: '1' });
  ui.click({ act: 'kill', id: '3' });
  eq(ui.sent, [], 'a different job needs its own second click');
});

test('queued rows pin, lower, cancel and move to the top', () => {
  ui.setState(busy);
  ui.click({ act: 'pin', id: '4' });
  ui.click({ act: 'low', id: '2' });
  ui.click({ act: 'cancel', id: '4' });
  ui.click({ act: 'top', id: '4' });
  eq(ui.sent, [
    { cmd: 'pin', id: '4', on: true },
    { cmd: 'low', id: '2', on: false },
    { cmd: 'cancel', id: '4' },
    { cmd: 'top', id: '4' },
  ]);
});

test('the head of the queue cannot be moved to the top', () => {
  ui.setState(busy);
  eq(ui.find({ act: 'top', id: '2' })[0].disabled, true);
});

test('a pinned running job can be unpinned and paused', () => {
  ui.setState(busy);
  ui.click({ act: 'pin', id: '3' });
  ui.click({ act: 'freeze', id: '3' });
  eq(ui.sent, [{ cmd: 'pin', id: '3', on: false }, { cmd: 'freeze', id: '3' }]);
});

test('banners resume, kill after confirming, and dismiss', () => {
  ui.setState(frozen);
  ui.click({ act: 'dismiss', id: 'f4' });
  const bannerKill = ui.find({ act: 'kill', id: '4', confirm: 'bkill:f4' });
  eq(bannerKill.length, 1);
  ui.click({ act: 'kill', confirm: 'bkill:f4' });
  ui.click({ act: 'kill', confirm: 'bkill:f4' });
  eq(ui.sent, [{ cmd: 'dismiss', id: 'f4' }, { cmd: 'kill', id: '4' }]);
});

test('expanding a row shows the detail and a readable profile key', () => {
  ui.setState(busy);
  ui.click({ act: 'expand', id: '1' });
  const v = ui.view();
  has(v, 'Copy command');
  has(v, 'work-scheduler-mcp · headroom · headroom-hog');
  lacks(v, '|');
  ui.click({ act: 'copy', text: busy.running[0].full_cmd });
  eq(ui.sent, [{ cmd: 'copy', text: busy.running[0].full_cmd }]);
});

// ── drill-in pages ────────────────────────────────────────────────────────

test('history words every ending', () => {
  ui.setState(history);
  ui.click({ act: 'goto', page: 'history' });
  const v = ui.view();
  has(v, 'gave up');
  has(v, 'requeued');
  has(v, 'disconnected');
  has(v, 'exit 3');
  has(v, 'ok');
  eq(ui.sent, [], 'navigation stays in the page');
});

test('a profile opens from history, labelled without the raw key', () => {
  ui.setState(history);
  ui.click({ act: 'goto', page: 'history' });
  const key = history.history.find(h => h.cmd === 'echo ok').key;
  ui.click({ act: 'profile', key });
  const v = ui.view();
  has(v, 'work-scheduler-mcp · headroom · echo ok');
  has(v, 'historical estimate');
  ui.click({ act: 'reset_profile', key });
  eq(ui.sent, [], 'reset is armed, not sent');
  ui.click({ act: 'reset_profile', key });
  eq(ui.sent, [{ cmd: 'reset_profile', key }]);
});

test('a profile separates measured runs and historical estimates from scheduling', () => {
  const key = '/repo|vitest run';
  const profile = { peaks: [2 * 2 ** 30], p90: 3 * 2 ** 30,
                    median_s: 12, samples: 1, runs: 3 };
  ui.setState({ ...history, profiles: { [key]: profile } });
  ui.UI.page = 'profile';
  ui.UI.profileKey = key;
  ui.setState({ ...history, profiles: { [key]: profile } });
  has(ui.view(), 'Measured peaks from 1 clean run · dashed line = historical estimate');
  has(ui.view(), 'historical estimate 3 GB median time 12s measured runs 1');
  has(ui.view(), "Scheduling may reserve more for the command's default worker count.");
  lacks(ui.view(), 'estimate (p90)');
  eq((ui.html().match(/<circle /g) || []).length, 1, 'only measured peaks are charted');

  ui.setState({ ...history, profiles: { [key]: { ...profile, peaks: [], samples: 0 } } });
  has(ui.view(), 'measured runs 0');
  lacks(ui.html(), '<svg');
});

test('a profile with no recorded runs says so', () => {
  ui.setState(busy);
  ui.UI.page = 'profile';
  ui.UI.profileKey = busy.running[0].key;
  ui.setState(busy);
  has(ui.view(), 'No runs recorded for this command yet.');
});

test('profile reset escapes repository names and preserves its confirmation key', () => {
  const key = '/repo/" onpointerenter="alert(1) <&|pnpm test';
  const profile = Object.values(history.profiles)[0];
  ui.setState({ ...history, profiles: { [key]: profile } });
  ui.UI.page = 'profile';
  ui.UI.profileKey = key;
  ui.setState(ui.STATE);
  lacks(ui.html(), ' onpointerenter="alert(1)');
  const button = ui.find({ act: 'reset_profile', key });
  eq(button.length, 1);
  eq(button[0].dataset.confirm, 'reset:' + key);
  ui.click({ act: 'reset_profile', key });
  eq(ui.sent, []);
  has(ui.view(), 'Reset? Click again');
  ui.click({ act: 'reset_profile', key });
  eq(ui.sent, [{ cmd: 'reset_profile', key }]);
});

test('settings send whole values the bridge understands', () => {
  ui.setState(busy);
  ui.click({ act: 'goto', page: 'settings' }, 0);
  ui.click({ act: 'set', key: 'kill_mode', val: 'freeze' });
  ui.click({ act: 'set', key: 'wait_cap_s', val: String(busy.settings.wait_cap_s + 60) });
  ui.click({ act: 'set', key: 'start_at_login', val: 'true' });
  eq(ui.sent, [
    { cmd: 'set', key: 'kill_mode', value: 'freeze' },
    { cmd: 'set', key: 'wait_cap_s', value: busy.settings.wait_cap_s + 60 },
    { cmd: 'set', key: 'start_at_login', value: true },
  ]);
});

test('steppers stop at their limits', () => {
  ui.setState(busy);
  ui.click({ act: 'goto', page: 'settings' }, 0);
  // 47 GB of headroom (the capture's) is past the stepper's 16 GB top.
  const [minus, plus] = ui.find({ act: 'set', key: 'headroom' });
  eq([minus.disabled, plus.disabled], [false, true]);
  eq(Number(minus.dataset.val), busy.settings.headroom - 0.5 * 2 ** 30, 'minus steps down from the real value');
});

test('a login item run by brew services is shown and cannot be toggled here', () => {
  ui.setState(history);
  ui.click({ act: 'goto', page: 'settings' }, 0);
  has(ui.view(), 'Managed by Homebrew');
  ui.click({ act: 'set', key: 'start_at_login' });
  eq(ui.sent, [], 'a disabled toggle sends nothing');
});

test('patterns are added once typed and removed by their button', () => {
  ui.setState(busy);
  ui.click({ act: 'goto', page: 'settings' }, 0);
  eq(ui.find({ act: 'add_pattern' })[0].disabled, true, 'Add is off while empty');
  ui.type('pattern', '   ');
  ui.key('Enter', 'pattern');
  eq(ui.sent, [], 'blank is not a pattern');
  ui.type('pattern', 'bun test');
  ui.key('Enter', 'pattern');
  ui.click({ act: 'remove_pattern', pattern: 'vitest' });
  eq(ui.sent, [{ cmd: 'add_pattern', pattern: 'bun test' }, { cmd: 'remove_pattern', pattern: 'vitest' }]);
});

test('the test box asks Rust and shows only the answer for what is typed', () => {
  ui.setState(busy);
  ui.click({ act: 'goto', page: 'settings' }, 0);
  ui.type('test', 'pnpm vitest run');
  eq(ui.sent, [{ cmd: 'classify', text: 'pnpm vitest run' }]);
  has(ui.view(), '…');
  ui.setState({ ...busy, classify: { text: 'pnpm vitest run', result: 'heavy', key: 'vitest run' } });
  has(ui.view(), 'gated, profiled as vitest run');
  // A late answer for an older draft is not shown against the new one.
  ui.type('test', 'pnpm dev');
  lacks(ui.view(), 'profiled as vitest run');
  ui.type('test', '   ');
  eq(ui.sent.length, 2, 'whitespace is not sent');
});

test('hook status per agent, with the install command to copy', () => {
  ui.setState(busy);
  ui.click({ act: 'goto', page: 'settings' }, 0);
  const v = ui.view();
  has(v, 'Claude Code installed');
  has(v, 'Codex not installed');
  has(v, 'pi not found');
  has(v, 'Not found on this Mac.');
  ui.click({ act: 'copy', text: 'headroom hook install' });
  eq(ui.sent, [{ cmd: 'copy', text: 'headroom hook install' }]);
  lacks(ui.page(), 'wsq');
});

test('shrinking jobs to fit can be switched off and on', () => {
  ui.setState(tuned);
  ui.click({ act: 'goto', page: 'settings' }, 0);
  has(ui.view(), 'Shrink jobs to fit');
  ui.click({ act: 'set', key: 'tune' });
  ui.setState({ ...tuned, settings: { ...tuned.settings, tune: false } });
  ui.click({ act: 'set', key: 'tune' });
  eq(ui.sent, [{ cmd: 'set', key: 'tune', value: false }, { cmd: 'set', key: 'tune', value: true }]);
});

test('letting short jobs go first can be switched off', () => {
  ui.setState(backfill);
  ui.click({ act: 'goto', page: 'settings' }, 0);
  has(ui.view(), 'At most 3 times or 5m00, then the queue waits for it.');
  ui.click({ act: 'set', key: 'backfill' });
  eq(ui.sent, [{ cmd: 'set', key: 'backfill', value: false }]);
});

test('quitting says whether it stops the queue', () => {
  ui.setState(busy);
  ui.click({ act: 'goto', page: 'settings' }, 0);
  has(ui.view(), 'Quitting stops the queue');
  ui.setState({ ...busy, attached: true });
  has(ui.view(), 'quitting closes only this menu bar');
});

test('the headroom stepper goes up to half the machine', () => {
  // The capture's machine has 64 GB: up to 32 GB, from below.
  ui.setState({ ...busy, settings: { ...busy.settings, headroom: 31.5 * 2 ** 30 } });
  ui.click({ act: 'goto', page: 'settings' }, 0);
  const [, plus] = ui.find({ act: 'set', key: 'headroom' });
  eq([plus.disabled, Number(plus.dataset.val) / 2 ** 30], [false, 32]);
});

test('quit is sent as an intent', () => {
  ui.setState(busy);
  ui.click({ act: 'goto', page: 'settings' }, 0);
  ui.click({ act: 'quit' });
  eq(ui.sent, [{ cmd: 'quit' }]);
});

// ── formatting ────────────────────────────────────────────────────────────

test('profile keys lose the repository path but keep subdir and command', () => {
  eq(ui.keyLabel('/Users/me/Projects/web|apps/site|vitest run'), 'web · apps/site · vitest run');
  eq(ui.keyLabel('/Users/me/Projects/web|pnpm'), 'web · pnpm');
  eq(ui.keyLabel('web'), 'web');
  eq(ui.keyLabel('/|sh'), '/ · sh');
  eq(ui.keyLabel(''), '');
});

test('a state arriving mid-drag is held back until the drop', () => {
  ui.setState(busy);
  ui.UI.drag = { id: '4' };
  ui.setState(held);
  lacks(ui.view(), 'held by you');
  ui.UI.drag = null;
  ui.setState(held);
  has(ui.view(), 'held by you');
});

console.log(`ui: ${pass} passed, ${fail} failed`);
if (fail) {
  console.log(failures.join('\n'));
}
// The kill-confirm timers would otherwise keep node alive for 3 s.
process.exit(fail ? 1 : 0);

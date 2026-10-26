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
  ui.UI.started = true;
  ui.UI.finishingSetup = false;
  ui.UI.welcomeOpen.clear();
  ui.UI.trail.length = 0;
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

test('first launch explains setup without installing integrations or queuing work', () => {
  ui.UI.started = false;
  const fresh = { ...busy, setup:{complete:false,error:null}, copy_result:null };
  ui.setState(fresh);
  eq(ui.UI.page, 'welcome');
  has(ui.view(), 'Connect your coding agents');
  has(ui.view(), 'Catch commands with shims');
  has(ui.view(), 'Optionally use the CLI directly');
  has(ui.view(), 'Headroom learns their memory use and runtime');
  eq(ui.sent, []);
  ui.click({ act:'copy', text:'headroom hook install' });
  ui.click({ act:'copy', text:'headroom shims install' });
  const instruction = ui.find({ act:'copy' }).find(b => b.dataset.text.includes('Run heavy tests')).dataset.text;
  ui.click({ act:'copy', text:instruction });
  eq(ui.sent, [
    {cmd:'copy',text:'headroom hook install'},
    {cmd:'copy',text:'headroom shims install'},
    {cmd:'copy',text:instruction},
  ]);
  has(instruction, 'headroom run -- <command>');
  has(instruction, 'headroom ls');
  lacks(ui.view(), 'Copied');
  ui.setState(fresh);
  eq(ui.UI.page, 'welcome');
});

test('continuing setup waits for persistence, shows failure, and exits after successful retry', () => {
  ui.UI.started = false;
  const fresh = { ...busy, setup:{complete:false,error:null} };
  ui.setState(fresh);
  ui.click({ act:'finish_setup' });
  eq(ui.sent, [{cmd:'finish_setup'}]);
  eq(ui.UI.page, 'welcome');
  eq(ui.find({ act:'finish_setup' })[0].disabled, true);
  ui.setState({ ...fresh, setup:{complete:false,error:'<save failed>'} });
  eq(ui.UI.page, 'welcome');
  has(ui.html(), '&lt;save failed&gt;');
  eq(ui.find({ act:'finish_setup' })[0].disabled, false);
  ui.click({ act:'finish_setup' });
  eq(ui.sent, [{cmd:'finish_setup'},{cmd:'finish_setup'}]);
  ui.setState({ ...fresh, setup:{complete:true,error:null} });
  eq(ui.UI.page, 'main');
  eq(ui.UI.trail.length, 0);
  eq(document.activeElement.id, 'page-title');
  ui.UI.started = false;
  ui.setState({ ...fresh, setup:{complete:true,error:null} });
  eq(ui.UI.page, 'main', 'completed setup does not reopen on launch');
});

test('welcome can be revisited without resetting completed setup', () => {
  ui.setState({ ...busy, setup:{complete:true,error:null} });
  ui.click({ act:'goto', page:'settings' }, 0);
  ui.click({ act:'goto', page:'welcome' });
  eq(ui.UI.page, 'welcome');
  ui.UI.welcomeOpen.add('cli');
  document.getElementById('welcome-detail-cli').focus();
  ui.setState({ ...busy, setup:{complete:true,error:null} });
  has(ui.html(), 'data-welcome-detail="cli" open');
  eq(document.activeElement.id, 'welcome-detail-cli', 'live updates preserve disclosure focus');
  ui.click({ act:'finish_setup' });
  eq(ui.UI.page, 'main');
  eq(ui.sent, []);
  eq(ui.UI.trail.length, 0);
});

test('welcome copy feedback reflects the confirmed native result', () => {
  ui.UI.started = false;
  const fresh = { ...busy, setup:{complete:false,error:null}, copy_result:null };
  ui.setState(fresh);
  ui.click({ act:'copy', text:'headroom hook install' });
  lacks(ui.view(), 'Copied');
  ui.setState({ ...fresh, copy_result:{text:'headroom hook install',ok:false,message:'<clipboard failed>'} });
  has(ui.html(), '&lt;clipboard failed&gt;');
  lacks(ui.view(), 'Copied');
  ui.setState({ ...fresh, copy_result:{text:'headroom hook install',ok:true,message:'Copied to clipboard.'} });
  has(ui.view(), 'Copied');
  lacks(ui.view(), 'clipboard failed');
});

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

test('state updates restore focus to replacement controls even when their next value changes', () => {
  const query = document.querySelectorAll;
  try {
    for (const data of [
      { act: 'hold' },
      { act: 'set', key: 'backfill', val: 'false' },
      { act: 'set', key: 'headroom', step: 'up', val: '2' },
    ]) {
      const before = { id: '', dataset: data };
      const after = { dataset: { ...data, ...('val' in data ? { val: data.val === 'false' ? 'true' : '3' } : {}) },
        focus() { document.activeElement = this; } };
      let queries = 0;
      document.activeElement = before;
      document.querySelectorAll = selector => selector === '[data-act]' ? [queries++ ? after : before] : [];
      ui.setState(busy);
      if (document.activeElement !== after) throw new Error('control focus was lost');
    }
  } finally {
    document.querySelectorAll = query;
    document.activeElement = null;
  }
});

test('refresh reveals a collapsed queued action before restoring its focus', () => {
  const query = document.querySelectorAll;
  const before = { dataset: { act: 'pin', id: busy.queued[0].id } };
  const classes = new Set();
  const row = { classList: { add: name => classes.add(name), remove: name => classes.delete(name) } };
  const after = { dataset: before.dataset, closest: selector => selector === '.job.queued' ? row : null,
    focus() {
      if (!classes.has('restoring-focus')) throw new Error('queued action is display:none before focus');
      document.activeElement = this;
    } };
  let queries = 0;
  try {
    document.activeElement = before;
    document.querySelectorAll = selector => selector === '[data-act]' ? [queries++ ? after : before] : [];
    ui.setState(busy);
    eq(document.activeElement === after, true);
    eq(classes.has('restoring-focus'), false, 'temporary visibility clears after focus');
    has(ui.source(), '.job.queued.restoring-focus .acts-i');
  } finally {
    document.querySelectorAll = query;
    document.activeElement = null;
  }
});

test('return navigation keeps the same history run when another run appears first', () => {
  const run = { ...history.history[0], ended_at: 1720000000 };
  const first = { ...run, ended_at: 1720000001 };
  ui.setState({ ...history, history: [first, run] });
  ui.click({ act: 'goto', page: 'history' });
  const source = ui.find({ act: 'profile' })[1];
  ui.click({ act: 'profile', run: source.dataset.run });
  ui.setState({ ...history, history: [{ ...run, ended_at: 1720000002 }, first, run] });
  ui.key('Escape');
  eq(document.activeElement.dataset.run, source.dataset.run);
  eq(document.activeElement === ui.find({ act: 'profile' })[2], true);
});

test('job details and command profiles open from keyboard controls', () => {
  ui.setState(busy);
  const action = { act: 'expand', id: busy.running[0].id };
  eq(ui.find(action)[0].tabIndex, '0');
  eq(ui.press(action, 'Enter'), true);
  eq(ui.UI.expanded.has(busy.running[0].id), true);
  eq(ui.press(action, ' '), true);
  eq(ui.UI.expanded.has(busy.running[0].id), false);
  ui.setState(history);
  ui.click({ act: 'goto', page: 'commands' });
  const row = ui.find({ act: 'profile' })[0];
  eq(row.tabIndex, '0');
  eq(ui.press({ act: 'profile', key: row.dataset.key }, 'Enter'), true);
  eq(ui.UI.page, 'profile');
  eq(ui.UI.profileKey, row.dataset.key);
});

test('settings switches expose their names and current states', () => {
  ui.setState(busy);
  ui.click({ act: 'goto', page: 'settings' }, 0);
  for (const [key, label] of [['backfill', 'Let smaller jobs go first'], ['start_at_login', 'Start at login']]) {
    const control = ui.find({ act: 'set', key })[0];
    eq([control.role, control.label, control.checked],
       ['switch', label, String(busy.settings[key])]);
  }
});


test('navigation focuses the destination heading and returns to the source row and scroll', () => {
  ui.setState(history);
  ui.click({ act: 'goto', page: 'commands' });
  const view = document.getElementById('view');
  eq(document.activeElement.id, 'page-title');
  has(ui.html(), '<h1 id="page-title" tabindex="-1">Commands</h1>');
  const row = ui.find({ act: 'profile' })[1];
  view.scrollTop = 175;
  // Custom role buttons do not receive focus on a mouse click in WebKit.
  document.activeElement = document.body;
  ui.click({ act: 'profile', key: row.dataset.key });
  eq(ui.UI.page, 'profile');
  eq(view.scrollTop, 0);
  eq(document.activeElement.id, 'page-title');
  ui.setState(history);
  eq(document.activeElement.id, 'page-title', 'one-second updates keep heading focus');
  ui.key('Escape');
  eq(ui.UI.page, 'commands');
  eq(view.scrollTop, 175);
  eq(document.activeElement.dataset, row.dataset);
  ui.click({ act: 'profile', key: row.dataset.key });
  ui.click({ act: 'goto', page: 'commands' });
  eq(view.scrollTop, 175);
  eq(document.activeElement.dataset, row.dataset);
});

test('return navigation restores the selected occurrence of repeated profile rows', () => {
  const repeated = { ...history, history: [history.history[0], history.history[0]] };
  ui.setState(repeated);
  ui.click({ act: 'goto', page: 'history' });
  ui.click({ act: 'profile' }, 1);
  ui.key('Escape');
  eq(document.activeElement === ui.find({ act: 'profile' })[1], true);
});

test('return navigation falls back to the heading if the originating job disappeared', () => {
  ui.setState(busy);
  ui.click({ act: 'expand', id: busy.running[0].id });
  document.getElementById('view').scrollTop = 90;
  ui.click({ act: 'profile', key: busy.running[0].key });
  ui.setState({ ...busy, running: [] });
  ui.key('Escape');
  eq(ui.UI.page, 'main');
  eq(document.activeElement.id, 'page-title');
  eq(document.getElementById('view').scrollTop, 90);
});

test('Escape returns through a profile and its source list after opening header settings', () => {
  ui.setState(history);
  ui.click({ act: 'goto', page: 'commands' });
  ui.click({ act: 'profile' }, 0);
  ui.click({ act: 'goto', page: 'settings' }, 0);
  ui.key('Escape');
  eq(ui.UI.page, 'profile');
  ui.key('Escape');
  eq(ui.UI.page, 'commands');
});

test('returning through profiles from different tabs restores the correct selected tab', () => {
  ui.setState(history);
  ui.click({ act:'goto', page:'history' });
  ui.click({ act:'profile' }, 0);
  ui.click({ act:'goto', page:'commands' });
  ui.click({ act:'profile' }, 0);
  ui.key('Escape');
  ui.key('Escape');
  eq(ui.UI.page, 'profile');
  eq(ui.UI.profileFrom, 'history');
  const foot = document.getElementById('foot').innerHTML;
  has(foot, 'data-page="history" aria-current="page" data-back="true"');
  lacks(foot, 'data-page="commands" aria-current');
  ui.click({ act:'goto', page:'history' });
  eq(ui.UI.page, 'history');
});

test('persistent navigation selects each page and returns to Queue without a back button', () => {
  ui.setState(busy);
  for (const page of ['history', 'commands', 'others', 'settings', 'main']) {
    ui.click({ act:'goto', page }, 0);
    eq(ui.UI.page, page);
    const foot = document.getElementById('foot').innerHTML;
    eq((foot.match(/data-act="goto"/g) || []).length, 5);
    eq((foot.match(/aria-current="page"/g) || []).length, 1);
    has(foot, `data-page="${page}" aria-current="page"`);
    has(foot, '>Queue</button>');
    lacks(foot, 'data-tip');
    lacks(ui.view(), '‹ Back');
  }
});

test('job actions expose descriptive names and toggle states', () => {
  ui.setState(busy);
  for (const job of [...busy.running, ...busy.queued]) {
    const name = `${job.label}: ${job.cmd}`;
    const pin = ui.find({ act: 'pin', id: job.id })[0];
    eq(pin.label, `Pin ${name}`);
    eq(pin.pressed, String(job.pinned));
    for (const action of ['freeze', 'kill', 'top', 'low', 'cancel']) {
      const control = ui.find({ act: action, id: job.id })[0];
      if (control) has(control.label, name);
      if (control && action === 'low') eq(control.pressed, String(job.low));
    }
  }
  eq(ui.find({ act: 'hold' })[0].pressed, 'false');
  ui.setState(held);
  eq(ui.find({ act: 'hold' })[0].pressed, 'true');
  ui.setState(frozen);
  for (const control of ui.find({ act: 'resume' })) has(control.label, 'headroom-hog');
});

test('settings expose named steppers, input labels, pattern removal, and selected segments', () => {
  ui.setState(busy);
  ui.click({ act: 'goto', page: 'settings' }, 0);
  for (const [key, label] of [['headroom', 'headroom'], ['wait_cap_s', 'foreground wait limit'],
    ['default_estimate', 'default estimate']]) {
    const [minus, plus] = ui.find({ act: 'set', key });
    eq([minus.label, plus.label], [`Decrease ${label}`, `Increase ${label}`]);
  }
  for (const control of ui.find({ act: 'set', key: 'kill_mode' })) {
    eq(control.pressed, String(control.dataset.val === busy.settings.kill_mode));
  }
  eq(ui.find({ act: 'remove_pattern', pattern: 'vitest' })[0].label, 'Remove pattern vitest');
  has(ui.html(), 'for="patIn">New heavy command pattern</label>');
  has(ui.html(), 'for="testIn">Test a command</label>');
});

test('command period and sorting controls announce the current selection', () => {
  ui.setState(history);
  ui.click({ act: 'goto', page: 'commands' });
  eq(ui.find({ act: 'cmd_days', val: '7' })[0].pressed, 'true');
  eq(ui.find({ act: 'cmd_days', val: '30' })[0].pressed, 'false');
  ui.click({ act: 'cmd_days', val: '30' });
  eq(ui.find({ act: 'cmd_days', val: '30' })[0].pressed, 'true');
  ui.click({ act: 'cmd_sort', val: 'runs' });
  eq(ui.find({ act: 'cmd_sort', val: 'runs' })[0].pressed, 'true');
  eq(ui.find({ act: 'cmd_sort', val: 'run_s' })[0].pressed, 'false');
});

test('the popover has a named main landmark, page navigation, and theme-aware native controls', () => {
  has(ui.source(), '<main class="body" id="view" aria-labelledby="page-title">');
  has(ui.source(), '<nav class="foot" id="foot" aria-label="Headroom pages">');
  has(ui.source(), 'color-scheme: light dark;');
  has(ui.source(), '@media (prefers-reduced-motion: reduce)');
  lacks(ui.source(), 'color-mix(', 'theme tints work on the original macOS 13 WebKit');
});

test('small text and status labels meet 4.5:1 contrast in both themes', () => {
  const luminance = hex => hex.slice(1).match(/../g).map(x => parseInt(x, 16) / 255)
    .map(x => x <= .04045 ? x / 12.92 : ((x + .055) / 1.055) ** 2.4)
    .reduce((sum, x, i) => sum + x * [.2126, .7152, .0722][i], 0);
  const themes = [...ui.source().matchAll(/:root\{([\s\S]*?)\}/g)];
  eq(themes.length, 2);
  for (const [index, theme] of themes.entries()) {
    const colors = Object.fromEntries([...theme[1].matchAll(/--([\w-]+):\s*(#[0-9a-f]{6})/g)]
      .map(([, name, value]) => [name, luminance(value)]));
    const pairs = ['text', 'text-dim', 'text-faint', 'accent', 'good', 'warn', 'bad', 'ice']
      .flatMap(ink => ['bg', 'bg-elev', 'bg-sunk'].map(background => [ink, background]));
    pairs.push(['good', 'good-tint'], ['bad', 'bad-tint'], ['accent', 'warn-tint'],
      ['text-dim', 'warn-tint'], ['text-dim', 'bad-tint'], ['text-dim', 'ice-tint'],
      ['on-status', 'warn'], ['on-status', 'bad']);
    for (const [ink, background] of pairs) {
      const [low, high] = [colors[ink], colors[background]].sort((a, b) => a - b);
      const ratio = (high + .05) / (low + .05);
      if (ratio < 4.5) throw new Error(`${index ? 'Dark' : 'Light'} ${ink} on ${background}: ${ratio.toFixed(2)}:1`);
    }
  }
});

// ── the main view ─────────────────────────────────────────────────────────

const GiB = 2 ** 30;
function memoryJob(overrides = {}) {
  return { ...busy.running[0], footprint: 3 * GiB, estimate: 6 * GiB,
    memory_range: { lower: 2 * GiB, upper: 4 * GiB, samples: 8, workers: 4 }, ...overrides };
}
function showMemoryJob(job) {
  ui.setState({ ...busy, running: [job], queued: [] });
}
function memoryGeometry() {
  const html = ui.html();
  const range = /class="range" style="left:([\d.]+)%;width:([\d.]+)%"/.exec(html);
  return {
    lower: range ? Number(range[1]) : null,
    upper: range ? Number(range[1]) + Number(range[2]) : null,
    live: Number(/class="f" style="width:([\d.]+)%"/.exec(html)[1]),
    estimate: Number(/class="tick" style="left:([\d.]+)%"/.exec(html)[1]),
  };
}
function near(got, want, what) {
  if (Math.abs(got - want) > 0.000001) throw new Error(`${what}: got ${got}, want ${want}`);
}

test('past peaks have measured bounds and a distinct live fill and scheduling estimate', () => {
  showMemoryJob(memoryJob());
  has(ui.view(), 'Past peaks 2–4 GB · 8 runs');
  has(ui.view(), '3 / est 6 GB');
  const { lower, upper, live, estimate } = memoryGeometry();
  near(lower / estimate, 2 / 6, 'lower bound on the same scale');
  near(upper / estimate, 4 / 6, 'upper bound on the same scale');
  near(live / estimate, 3 / 6, 'current usage on the same scale');
  near(estimate, 100 / 1.15, 'the estimate leaves room to its right');
  has(ui.html(), 'role="img" aria-label="Live memory 3 GB, shown by the fill. Conservative estimate 6 GB, shown by the vertical tick.');
  has(ui.html(), '8 successful, uninterrupted runs of this workload with 4 workers, like this job.');
  has(ui.html(), '10th to 90th percentile of past peak memory. Current usage can fall outside this range.');
});

test('a historical upper peak larger than live usage and the estimate fits on the chart', () => {
  showMemoryJob(memoryJob({ footprint: GiB, estimate: 2 * GiB }));
  const { lower, upper, live, estimate } = memoryGeometry();
  near(upper, 100 / 1.15, 'historical upper peak sets the scale');
  near(lower, upper / 2, 'lower peak');
  near(live, upper / 4, 'current usage');
  near(estimate, upper / 2, 'estimate');
});

test('widely varying history explains uncertainty instead of drawing a prediction band', () => {
  showMemoryJob(memoryJob({ estimate: 8 * GiB,
    memory_range: { lower: .3 * GiB, upper: 7.8 * GiB, samples: 13, workers: 10 } }));
  has(ui.view(), 'Memory varies widely · 13 runs');
  lacks(ui.view(), 'Past peaks 0.3–7.8 GB');
  eq(memoryGeometry().lower, null);
  has(ui.html(), 'Past peaks 0.3–7.8 GB');
  has(ui.html(), 'Memory varies too widely to show a useful expected band.');
  has(ui.html(), 'Conservative estimate 8 GB');
  showMemoryJob(memoryJob());
  has(ui.view(), 'Past peaks 2–4 GB · 8 runs');
  lacks(ui.view(), 'Memory varies widely');
});

test('fallback icons explain measured run counts until the estimate can use its own history', () => {
  for (const state of ['running', 'queued', 'frozen']) {
    for (const samples of [0, 1, 2, 3, 4]) {
      const job = memoryJob({ state, samples, samples_required: 1, memory_range: null });
      ui.setState({ ...busy, running: state === 'queued' ? [] : [job], queued: state === 'queued' ? [job] : [] });
      has(ui.view(), 'est 6 GB');
      if (samples >= 1) {
        lacks(ui.html(), 'class="fallback-estimate"');
        continue;
      }
      has(ui.html(), 'class="fallback-estimate"');
      has(ui.html(), 'tabindex="0" role="img" aria-label="Fallback memory estimate.');
      has(ui.html(), 'data-tip="Fallback memory estimate. 0 of 1 successful, uninterrupted measured run registered for this exact workload when this estimate was set.');
      has(ui.html(), '1 more measured run needed to use its own history.');
      has(ui.html(), 'Until then, the estimate uses broader history or the default.');
      has(ui.html(), 'Each later qualifying run updates the estimate.');
      has(ui.html(), 'Failed or interrupted peaks remain a minimum.');
      has(ui.html(), 'Different invocations and starting directories have separate histories.');
    }
  }
  // The threshold comes from the scheduler, rather than a separate UI constant.
  showMemoryJob(memoryJob({ samples: 1, samples_required: 2, memory_range: null }));
  has(ui.html(), '1 of 2 successful, uninterrupted measured runs');
  has(ui.html(), '1 more measured run needed');
});

test('live usage can exceed past peaks without clipping or moving their values', () => {
  showMemoryJob(memoryJob({ footprint: 10 * GiB }));
  const { lower, upper, live, estimate } = memoryGeometry();
  near(live, 100 / 1.15, 'live usage sets the scale');
  near(lower, live / 5, 'lower peak');
  near(upper, live * .4, 'upper peak');
  near(estimate, live * .6, 'estimate');
  has(ui.view(), 'Past peaks 2–4 GB · 8 runs');
  has(ui.view(), '10 / est 6 GB');
});

test('identical past peaks remain a single marked value without inventing a range', () => {
  showMemoryJob(memoryJob({ memory_range: { lower: 2 * GiB, upper: 2 * GiB, samples: 3, workers: 1 } }));
  const { lower, upper } = memoryGeometry();
  near(lower, upper, 'equal bounds keep the same position');
  has(ui.view(), 'Past peaks 2–2 GB · 3 runs');
  has(ui.html(), '3 successful, uninterrupted runs of this workload with 1 worker, like this job.');
  // CSS retains a visible marker when the numeric width is zero.
  has(ui.source(), '.fp .range{ position:absolute; top:0; height:5px; min-width:2px;');
});

test('absent history never borrows a band from the estimate or profile', () => {
  for (const memory_range of [null, undefined]) {
    showMemoryJob(memoryJob({ memory_range }));
    eq(memoryGeometry().lower, null);
    lacks(ui.view(), 'Past peaks');
    lacks(ui.html(), 'successful, uninterrupted runs of this workload');
    has(ui.view(), '3 / est 6 GB');
    has(ui.html(), 'Conservative estimate 6 GB');
  }
});

test('history explains comparisons with unrecorded worker counts', () => {
  showMemoryJob(memoryJob({ memory_range: { lower: GiB, upper: 2 * GiB, samples: 5, workers: null } }));
  has(ui.html(), '5 successful, uninterrupted runs of this workload with no worker count recorded.');
  lacks(ui.html(), 'null workers');
});

test('state updates refresh live usage, range, estimate, and explanation for the same job', () => {
  const job = memoryJob();
  showMemoryJob(job);
  const before = memoryGeometry();
  showMemoryJob({ ...job, footprint: 9 * GiB, estimate: 10 * GiB,
    memory_range: { lower: 3 * GiB, upper: 7 * GiB, samples: 12, workers: 2 } });
  const after = memoryGeometry();
  eq(after.live > before.live, true);
  near(after.upper / after.estimate, .7, 'updated range geometry');
  has(ui.view(), 'Past peaks 3–7 GB · 12 runs');
  has(ui.view(), '9 / est 10 GB');
  has(ui.html(), 'Conservative estimate 10 GB');
  has(ui.html(), '12 successful, uninterrupted runs of this workload with 2 workers, like this job.');
  lacks(ui.html(), 'Past peaks 2–4 GB');
  showMemoryJob({ ...job, memory_range: null });
  eq(memoryGeometry().lower, null, 'removed history clears the old band');
  lacks(ui.view(), 'Past peaks');
});

test('running and queued jobs are listed with why the queue waits', () => {
  ui.setState(busy);
  const v = ui.view();
  has(v, 'Running 2');
  // Where each runs, with the command on the line below.
  has(ui.html(), '<b>sc-49123</b></span><span class="cmd">headroom-hog</span>');
  has(v, 'browser-cd sleep 31');
  const hog = busy.running.find(j => j.label === 'sc-49123');
  has(ui.html(), `Live memory ${(hog.footprint / 2 ** 30).toFixed(1).replace(/\.0$/, '')} GB`);
  has(v, `CPU ${Math.round(hog.cpu_pct)}%`);
  has(v, 'Queued 2');
  has(v, 'next needs 1.5 GB, 0 GB free');
  has(ui.html(), '<b>fix-main</b></span><span class="cmd">sleep 30</span>');
  has(v, 'form-service sleep 32');
  has(v, 'est 1.5 GB');
  // The estimate, and how long it has waited on the line below.
  has(ui.html(), 'est 1.5 GB</span><span title="Time in the queue so far">waiting ');
});

test('reported agents appear on running, frozen and queued jobs and in their details', () => {
  for (const [agent, label] of [['codex', 'Codex'], ['claude', 'Claude Code'], ['pi', 'pi']]) {
    for (const state of ['running', 'frozen', 'queued']) {
      const job = { ...busy.running[0], agent, state };
      ui.setState({ ...busy, running: state === 'queued' ? [] : [job], queued: state === 'queued' ? [job] : [] });
      has(ui.html(), `<span class="job-origin" title="Started by ${label}">${label}</span>`);
      ui.click({ act: 'expand', id: job.id });
      has(ui.html(), `<span class="k">agent</span><span class="v">${label}</span>`);
      ui.UI.expanded.clear();
    }
  }
});

test('missing origins stay unlabeled regardless of the session or command', () => {
  for (const agent of [undefined, null, '', '  ']) {
    const job = { ...busy.running[0], agent, session: 'codex-session', cmd: 'claude test' };
    ui.setState({ ...busy, running: [job], queued: [{ ...job, id: 'unknown-queued', state: 'queued' }] });
    ui.UI.expanded.add(job.id);
    ui.setState({ ...busy, running: [job], queued: [] });
    lacks(ui.html(), 'class="job-origin"');
    lacks(ui.html(), '<span class="k">agent</span>');
    lacks(ui.view(), 'Terminal');
  }
});

test('custom origins are escaped in the badge, tooltip and expanded details', () => {
  const agent = '<img src=x onerror="alert(1)"> & \'custom\'';
  const escaped = '&lt;img src=x onerror=&quot;alert(1)&quot;&gt; &amp; &#39;custom&#39;';
  const job = { ...busy.running[0], agent };
  ui.setState({ ...busy, running: [job], queued: [{ ...job, id: 'custom-queued', state: 'queued' }] });
  eq(ui.html().split(`class="job-origin" title="Started by ${escaped}">${escaped}</span>`).length - 1, 2);
  ui.click({ act: 'expand', id: job.id });
  has(ui.html(), `<span class="k">agent</span><span class="v">${escaped}</span>`);
  lacks(ui.html(), '<img');
});

test('origin badges share the shrinking name column and truncate long custom labels', () => {
  const job = { ...busy.running[0], agent: 'custom agent '.repeat(30) };
  ui.setState({ ...busy, running: [job], queued: [] });
  has(ui.html(), `<span class="job-title"><b>${job.label}</b><span class="job-origin"`);
  has(ui.source(), '.job .name .job-title{ display:flex; align-items:center; gap:5px; min-width:0; }');
  has(ui.source(), '.job-title b{ flex:1; min-width:0; overflow:hidden; text-overflow:ellipsis; }');
  has(ui.source(), '.job-origin{ flex:0 1 auto; max-width:50%; min-width:0; overflow:hidden; text-overflow:ellipsis; white-space:nowrap;');
  ui.click({ act: 'expand', id: job.id });
  has(ui.html(), `<span class="k">agent</span><span class="v">${job.agent.trim()}</span>`);
});

test('a hold shows in the header button and as the queue reason', () => {
  ui.setState(held);
  has(ui.head(), 'Held');
  has(ui.view(), 'held by you');
});

test('running timing retains elapsed wall time during pauses and after resume', () => {
  const s = JSON.parse(JSON.stringify(busy));
  const j = s.running[0];
  s.running = [j];
  j.elapsed_s = 134;
  j.frozen_s = 12;
  j.waited_s = 9;
  j.state = 'frozen';
  j.runtime_estimate_s = 142;
  ui.setState(s);
  has(ui.view(), 'Elapsed 2m14');
  has(ui.view(), 'Paused 12s');
  has(ui.view(), 'Usual 2m22');
  lacks(ui.view(), 'RAM ');
  has(ui.html(), 'including pauses');
  ui.click({ act: 'expand', id: j.id });
  has(ui.view(), 'queue wait 9s before this attempt');
  has(ui.view(), 'usual runtime 2m22');
  has(ui.view(), '2m14 including pauses');
  has(ui.html(), 'It helps plan when queued jobs might fit.');
  j.state = 'running';
  j.frozen_s = 0;
  j.elapsed_s = 140;
  ui.setState(s);
  has(ui.view(), 'Elapsed 2m20');
  lacks(ui.view(), 'Paused ');
});

test('runtime display preserves missing and subsecond scheduler estimates', () => {
  const s = JSON.parse(JSON.stringify(busy));
  const j = s.running[0];
  s.running = [j];
  for (const estimate of [null, undefined, 0, 900]) {
    j.runtime_estimate_s = estimate;
    ui.UI.expanded.add(j.id);
    ui.setState(s);
    if (estimate == null) {
      lacks(ui.view(), 'Usual ');
      has(ui.view(), 'usual runtime Unknown for this worker count');
    } else {
      has(ui.view(), estimate === 0 ? 'Usual 0s' : 'Usual 15m');
    }
  }
});

test('expanded scheduling explains growth, overruns, pauses and pinned admission', () => {
  const s = JSON.parse(JSON.stringify(busy));
  const j = s.running[0];
  s.running = [j];
  j.runtime_estimate_s = 120;
  j.elapsed_s = 60;
  j.growth_reserved = 2 * GiB;
  ui.UI.expanded.add(j.id);
  ui.setState(s);
  has(ui.view(), 'Allows another 2 GB for growth');
  has(ui.view(), 'fitting shorter jobs can go ahead');
  j.elapsed_s = 121;
  ui.setState(s);
  has(ui.view(), 'Past its usual runtime. Queue planning allows as much time again as has elapsed');
  j.state = 'frozen';
  ui.setState(s);
  has(ui.view(), 'no extra growth allowance');
  has(ui.view(), 'No predicted memory release while paused');
  lacks(ui.view(), 'Past its usual runtime');
  j.state = 'queued';
  j.pinned = true;
  s.running = [];
  s.queued = [j];
  s.queue_reason = { kind: 'held' };
  ui.setState(s);
  has(ui.view(), 'Prioritized; skips memory checks');
  has(ui.view(), 'Still waits for queue hold, memory pressure and CPU limits');
  has(ui.view(), 'queue held by you');
  s.queued.unshift({ ...j, id: 'earlier-job' });
  ui.setState(s);
  has(ui.view(), 'queue Position 2');
  lacks(ui.view(), 'queue held by you');
});

test('disabled backfilling keeps runtime historical below and above the estimate', () => {
  const s = JSON.parse(JSON.stringify(busy));
  const j = s.running[0];
  s.running = [j];
  s.settings.backfill = false;
  j.runtime_estimate_s = 120;
  ui.UI.expanded.add(j.id);
  for (const elapsed of [60, 121]) {
    j.elapsed_s = elapsed;
    ui.setState(s);
    has(ui.view(), 'Usual 2m00');
    has(ui.view(), 'Usual runtime is historical context.');
    has(ui.view(), 'Smaller jobs going first is turned off; jobs follow queue order.');
    lacks(ui.html(), 'It helps plan when queued jobs might fit.');
    lacks(ui.view(), 'fitting shorter jobs can go ahead');
    lacks(ui.view(), 'Queue planning allows as much time again');
    lacks(ui.view(), 'helps predict memory releases');
  }
});

test('a guard freeze is a banner with resume and kill', () => {
  ui.setState(frozen);
  const v = ui.view();
  has(v, 'Froze “headroom-hog” (sc-49201)');
  if (!/memory critical for \d+ s/.test(v)) throw new Error('freeze reason missing: ' + v);
  has(v, '❄︎ Paused');
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
  has(ui.view(), `${first.label} git commit`);
  has(ui.view(), '↳ tsc --noEmit --project tsconfig.json');
});

test('a missing hook is called out in the header', () => {
  ui.setState(busy);
  has(ui.head(), 'Codex: hook not installed');
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
  has(ui.view(), 'Jobs last 24h 5 runs 3 failed 0 waited 0s wait p50 0s wait p95 0 went first');
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
  ui.setState({ ...history, profiles: { ...history.profiles, [key]: { ...p, runs: 4,
    parts: [{ ...parts[0], median_s: null }] } } });
  has(ui.view(), 'tsc --noEmit in 3 of 4 runs · peak 3 GB · time unknown');
  lacks(ui.view(), 'peak 3 GB · 0s');
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
  has(html, 'class="zero" data-tip="0 smaller jobs started ahead');
  eq((html.match(/data-tip=/g) || []).length, 6, 'one tooltip per stat');
  lacks(html, 'class="bad"');
  ui.setState(busy);
  eq(busy.last_day.runs, 0, 'the capture has nothing finished yet');
  has(ui.view(), 'Jobs last 24h 0 runs 0 failed 0 waited 0s wait p50');
});

test('the pressure level is coloured text with a tooltip, and so are the header stats', () => {
  ui.setState(busy);
  const head = ui.headHtml();
  has(head, `class="level ${busy.system.pressure}" data-tip="Memory pressure ${busy.system.pressure}`);
  lacks(head, 'pill');
  has(head, 'data-tip="Memory moved to disk:');
  has(head, 'data-tip="CPU in use across all cores:');
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
  has(ui.view(), 'Measured peaks from 1 successful run · dashed line = historical estimate');
  has(ui.view(), 'historical estimate 3 GB median time 12s measured runs 1');
  has(ui.view(), "Scheduling may reserve more for the command's default worker count.");
  lacks(ui.view(), 'estimate (p90)');
  eq((ui.html().match(/<circle /g) || []).length, 1, 'only measured peaks are charted');

  ui.setState({ ...history, profiles: { [key]: { ...profile, peaks: [], samples: 0, median_s: null } } });
  has(ui.view(), 'measured runs 0');
  has(ui.view(), 'median time Unknown');
  lacks(ui.view(), 'median time 0s');
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

test('button tooltips expose action consequences to assistive technology', () => {
  ui.setState(busy);
  for (const [dataset, expected] of [
    [{ act:'pin', on:'1' }, 'skip its memory-fit check'],
    [{ act:'pin', on:'0' }, 'restore normal queue priority'],
    [{ act:'freeze' }, 'It keeps its RAM'],
    [{ act:'resume' }, 'until memory pressure returns to normal'],
    [{ act:'clear_data' }, 'Click twice to confirm'],
    [{ act:'copy' }, 'Copy command'],
  ]) {
    const attrs = { title:'Copy command' };
    const button = { dataset, textContent:'Copy', getAttribute: key => attrs[key],
      setAttribute: (key, val) => { attrs[key] = val; }, removeAttribute: key => { delete attrs[key]; } };
    ui.describeButton(button);
    has(button.dataset.tip, expected);
    eq(attrs['aria-description'], button.dataset.tip);
    eq(attrs.title, undefined);
  }
});

test('hovered stats keep their tooltip and update its recorded values across focus restoration', () => {
  ui.setState(busy);
  const pop = document.getElementById('pop');
  const tip = document.getElementById('tip');
  const heading = document.getElementById('page-title');
  const originalFocus = heading.focus;
  const originalBounds = pop.getBoundingClientRect;
  const originalHitTest = document.elementFromPoint;
  const rect = {left:0,top:0,right:400,bottom:640,width:400,height:640};
  const stat = {
    get dataset() { return {tip: /data-tip="([^"]*available on this Mac[^"]*)"/.exec(ui.headHtml())[1]}; },
    closest() { return this; },
    getBoundingClientRect: () => rect,
  };
  try {
    pop.getBoundingClientRect = () => rect;
    heading.focus = function() {
      document.activeElement = this;
      pop._listeners.focusin({target:this});
    };
    heading.focus();
    document.elementFromPoint = () => stat;
    pop._listeners.mousemove({clientX:20,clientY:80,target:stat});
    eq(tip.classList.contains('on'), true);
    for (const available of [3, 4, 5]) {
      ui.setState({...busy,system:{...busy.system,mem_available:available * 1024 ** 3}});
      eq(tip.classList.contains('on'), true, 'stationary hover survives each sample');
      has(tip.textContent, `${available} GB available on this Mac`);
    }
    pop._listeners.mouseleave();
    eq(tip.classList.contains('on'), false, 'leaving still hides the tooltip');
  } finally {
    heading.focus = originalFocus;
    pop.getBoundingClientRect = originalBounds;
    document.elementFromPoint = originalHitTest;
    pop._listeners.mouseleave();
  }
});

test('clear usage data needs confirmation and waits for the backend result', () => {
  ui.setState(busy);
  ui.click({ act: 'goto', page: 'settings' }, 0);
  has(ui.view(), 'Running and queued jobs continue');
  ui.click({ act: 'clear_data' });
  eq(ui.sent, []);
  has(ui.view(), 'Clear permanently? Click again');
  ui.click({ act: 'clear_data' });
  eq(ui.sent, [{ cmd: 'clear_data' }]);
  eq(ui.STATE.running, busy.running);
  eq(ui.STATE.history, busy.history);
  lacks(ui.view(), 'statistics cleared');
  for (const ok of [true, false]) {
    ui.setState({ ...busy, data_clear: { ok, message: '<script>result</script>' } });
    has(ui.html(), `role="${ok ? 'status' : 'alert'}"`);
    has(ui.html(), '&lt;script&gt;result&lt;/script&gt;');
    lacks(ui.html(), '<script>result</script>');
  }
});

test('header explains the job budget and running growth', () => {
  ui.setState({ ...busy, queue_budget: { available: .5 * GiB, growth: 4 * GiB } });
  has(ui.head(), '0.5 GB job budget 4 GB growth');
  has(ui.headHtml(), 'Pressure, CPU and queue-order checks still apply');
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
  has(ui.view(), 'gated, command type vitest run');
  // A late answer for an older draft is not shown against the new one.
  ui.type('test', 'pnpm dev');
  lacks(ui.view(), 'command type vitest run');
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

test('Shims settings distinguish installed links from default-terminal activation', () => {
  const shims = {installed:16,total:16,path:'/Users/me/.local/share/headroom/shims',shell:'/bin/zsh',source:'fresh_login_shell',shadowed:[],message:null};
  for (const [status, label] of Object.entries({checking:'Checking…',active:'Active',partial:'Some shims missing',missing:'Not installed',not_on_path:'Not on PATH',shadowed:'Commands bypass shims',unverified:'Could not verify'})) {
    ui.setState({ ...busy, shims:{...shims,status} });
    if (ui.UI.page !== 'settings') ui.click({ act:'goto', page:'settings' }, 0);
    has(ui.view(), 'Installed links 16 of 16');
    has(ui.view(), `Default terminal PATH ${label}`);
    eq(ui.find({act:'check_shims'})[0].disabled, status === 'checking');
    if (status !== 'active') lacks(ui.view(), 'Default terminal PATH Active');
  }
});

test('Shims setup and recheck send explicit intents and escape probe messages', () => {
  ui.setState({ ...busy, shims:{installed:12,total:16,status:'shadowed',path:'<path>',shell:'/bin/zsh',shadowed:['<pnpm>'],message:'<probe message>'} });
  ui.click({act:'goto',page:'settings'},0);
  has(ui.view(), 'Installed links 12 of 16');
  has(ui.view(), 'Bypassing the shims: <pnpm>');
  has(ui.html(), '&lt;path&gt;');
  has(ui.html(), '&lt;probe message&gt;');
  lacks(ui.html(), '<pnpm>');
  ui.click({act:'check_shims'});
  ui.click({act:'copy',text:'headroom shims install'});
  eq(ui.sent,[{cmd:'check_shims'},{cmd:'copy',text:'headroom shims install'}]);
});

test('Hook settings scrolls to Agent hooks from the queue and from Settings', () => {
  const state = { ...busy, hooks: { claude:'installed', codex:'needs_review', pi:'absent' } };
  const section = document.getElementById('agent-hooks');
  const originalScroll = section.scrollIntoView;
  let jumps = 0;
  section.scrollIntoView = options => {
    eq(options, { block:'start' }); jumps++;
    document.getElementById('view').scrollTop = 600;
  };
  try {
    ui.setState(state);
    ui.click({ act:'goto', page:'settings', section:'agent-hooks' });
    eq(ui.UI.page, 'settings');
    has(ui.html(), 'id="agent-hooks" tabindex="-1"');
    eq(document.activeElement.id, 'agent-hooks');
    eq(jumps, 1);
    eq(document.getElementById('view').scrollTop, 600);
    ui.setState(state);
    eq(jumps, 1, 'telemetry updates preserve scroll rather than jumping again');
    document.getElementById('view').scrollTop = 0;
    const depth = ui.UI.trail.length;
    ui.click({ act:'goto', page:'settings', section:'agent-hooks' });
    eq(jumps, 2, 'jump still works when already on Settings');
    eq(ui.UI.trail.length, depth, 'jump within Settings adds no history entry');
    ui.key('Escape');
    eq(ui.UI.page, 'main');
    eq(document.activeElement.dataset.section, 'agent-hooks', 'return focus targets the header shortcut');
  } finally { section.scrollIntoView = originalScroll; }
});

test('confirmed Codex hook problems show approval and disabled notices', () => {
  const states = [
    ['needs_review', 'hook needs approval in /hooks', 'Codex needs review',
      'Open /hooks in Codex to approve the Headroom hook before it runs.'],
    ['disabled', 'hook disabled', 'Codex disabled',
      'Disabled in Codex. Check /hooks and the hook settings under [features] in ~/.codex/config.toml'],
  ];
  for (const [status, notice, label, hint] of states) {
    ui.setState({ ...busy, hooks: { claude: 'installed', codex: status, pi: 'absent' } });
    has(ui.head(), `Codex: ${notice}`);
    lacks(ui.head(), "aren't gated");
    has(ui.head(), 'Hook settings');
    ui.click({ act: 'goto', page: 'settings' }, 0);
    eq(ui.UI.page, 'settings');
    has(ui.view(), label);
    has(ui.view(), hint);
    eq(ui.find({ act: 'copy', text: 'headroom hook install' }).length, 0,
      'reinstalling cannot fix disabled hooks or establish approval');
    ui.click({ act: 'copy', text: '/hooks' });
  }
  eq(ui.sent, Array(2).fill({ cmd: 'copy', text: '/hooks' }));
});

test('pending and unavailable Codex checks stay neutral in the header and Settings', () => {
  for (const [status, label, hint] of [
    ['checking', 'Codex checking', 'Checking hook approval in Codex.'],
    ['review_unknown', 'Codex not verified', 'Headroom could not verify the current hook approval.'],
  ]) {
    ui.UI.page = 'main';
    ui.setState({ ...busy, hooks: { claude: 'installed', codex: status, pi: 'absent' } });
    lacks(document.getElementById('head').innerHTML, 'class="alert"');
    lacks(ui.head(), 'Hook settings');
    ui.click({ act: 'goto', page: 'settings' });
    has(ui.view(), label);
    has(ui.view(), hint);
    has(ui.html(), 'gtag user');
    eq(ui.find({ act: 'copy', text: 'headroom hook install' }).length, 0);
    eq(ui.find({ act: 'copy', text: '/hooks' }).length, status === 'review_unknown' ? 1 : 0);
  }
});

test('mixed hook issues keep installation and Codex review actions available', () => {
  ui.setState({ ...busy, hooks: { claude: 'stale', codex: 'review_unknown', pi: 'not_installed' } });
  has(ui.head(), 'Claude Code: hook path is stale. pi: hook not installed.');
  lacks(ui.head(), 'Codex:');
  ui.click({ act: 'goto', page: 'settings' }, 0);
  ui.click({ act: 'copy', text: 'headroom hook install' });
  ui.click({ act: 'copy', text: '/hooks' });
  eq(ui.sent, [{ cmd: 'copy', text: 'headroom hook install' }, { cmd: 'copy', text: '/hooks' }]);
});

test('hook status updates remove obsolete notices and suggested actions', () => {
  ui.setState({ ...busy, hooks: { claude: 'installed', codex: 'disabled', pi: 'absent' } });
  ui.click({ act: 'goto', page: 'settings' }, 0);
  eq(ui.find({ act: 'copy', text: '/hooks' }).length, 1);
  ui.setState({ ...busy, hooks: { claude: 'installed', codex: 'installed', pi: 'absent' } });
  lacks(ui.head(), 'Hook settings');
  has(ui.view(), 'Codex installed');
  lacks(ui.view(), 'Codex disabled');
  eq(ui.find({ act: 'copy', text: '/hooks' }).length, 0);
  eq(ui.find({ act: 'copy', text: 'headroom hook install' }).length, 0);
});

test('automatic worker shrinking is absent even with an older state', () => {
  ui.setState(tuned);
  ui.click({ act: 'goto', page: 'settings' }, 0);
  lacks(ui.view(), 'Shrink jobs to fit');
  eq(ui.find({ act: 'set', key: 'tune' }).length, 0);
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
  eq(ui.keyLabel(`/Users/me/Projects/web|apps/site|v2:${'a'.repeat(64)}|vitest run`), 'web · apps/site · vitest run');
  eq(ui.keyLabel(`/Users/me/Projects/web|v2:${'b'.repeat(64)}|tsc`), 'web · tsc');
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

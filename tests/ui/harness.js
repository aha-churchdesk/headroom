// A DOM stub just large enough to run ui/index.html under plain `node`,
// in live mode: the page believes it is inside the app's webview, so every
// intent goes to window.ipc instead of the prototype's mock.
//
// Clicks are made on what was actually rendered: `click()` finds the element
// in the rendered HTML by its data-* attributes and hands the page's own
// delegated listener a target carrying exactly those attributes.

'use strict';
const fs = require('fs');
const path = require('path');

function node(tag) {
  const listeners = {};
  const n = {
    tagName: tag,
    _html: '',
    scrollTop: 0,
    checked: false,
    style: new Proxy({}, { set: () => true, get: () => '' }),
    classList: {
      _s: new Set(),
      add(...c) { c.forEach(x => this._s.add(x)); },
      remove(...c) { c.forEach(x => this._s.delete(x)); },
      contains(c) { return this._s.has(c); },
      toggle(c, v) { (v === undefined ? !this._s.has(c) : v) ? this._s.add(c) : this._s.delete(c); },
    },
    addEventListener(type, fn) { listeners[type] = fn; },
    _listeners: listeners,
    append() {},
    focus() {},
    setSelectionRange() {},
  };
  Object.defineProperty(n, 'innerHTML', { get() { return n._html; }, set(v) { n._html = v; } });
  Object.defineProperty(n, 'textContent', { get() { return n._html; }, set(v) { n._html = v; } });
  return n;
}

const unescape = s => s.replace(/&quot;/g, '"').replace(/&#39;/g, "'")
  .replace(/&lt;/g, '<').replace(/&gt;/g, '>').replace(/&amp;/g, '&');

/** Opening tags carrying data-act, with their data-* attributes and disabled flag. */
function actionable(html) {
  const out = [];
  for (const m of html.matchAll(/<(\w+)((?:\s+[\w-]+(?:="[^"]*")?)*)\s*>/g)) {
    const attrs = m[2];
    if (!/data-act=/.test(attrs)) continue;
    const dataset = {};
    for (const a of attrs.matchAll(/data-([\w-]+)="([^"]*)"/g)) {
      dataset[a[1].replace(/-(\w)/g, (_, c) => c.toUpperCase())] = unescape(a[2]);
    }
    out.push({ tag: m[1], dataset, disabled: /\sdisabled(\s|$|=)/.test(attrs) });
  }
  return out;
}

/** Visible text of an HTML string, collapsed, for assertions. */
function text(html) {
  return unescape(html.replace(/<[^>]*>/g, ' ')).replace(/\s+/g, ' ').trim();
}

function loadUI() {
  const html = fs.readFileSync(path.join(__dirname, '..', '..', 'ui', 'index.html'), 'utf8');
  const scripts = [...html.matchAll(/<script>([\s\S]*?)<\/script>/g)];
  const src = scripts[scripts.length - 1][1];

  const byId = {};
  const sent = [];
  global.window = { ipc: { postMessage: m => sent.push(JSON.parse(m)) } };
  global.location = { search: '' };
  global.URLSearchParams = class { get() { return null; } };
  global.history = { replaceState() {} };
  global.matchMedia = () => ({ matches: false });
  global.document = {
    body: node('body'),
    activeElement: null,
    getElementById: id => (byId[id] ||= node('div')),
    createElement: node,
    querySelectorAll: () => [],
  };

  const exported = `;module.exports = { keyLabel, reasonText, get UI(){ return UI }, get STATE(){ return STATE } };`;
  const mod = { exports: {} };
  new Function('module', src + exported)(mod);

  const pop = byId.pop;
  const page = () => byId.head._html + byId.view._html + byId.foot._html;
  const find = (want) => actionable(page()).filter(el =>
    Object.entries(want).every(([k, v]) => el.dataset[k] === String(v)));
  const fire = (type, target) => pop._listeners[type](target);

  return Object.assign(mod.exports, {
    sent,
    setState: s => window.__setState(JSON.parse(JSON.stringify(s))),
    fixture: name => JSON.parse(fs.readFileSync(path.join(__dirname, 'fixtures', name + '.json'), 'utf8')),
    view: () => text(byId.view._html),
    /** The view's markup, for what only shows on hover (titles). */
    html: () => byId.view._html,
    head: () => text(byId.head._html),
    headHtml: () => byId.head._html,
    page: () => text(page()),
    find,
    /** Click the rendered element matching these data-* values; it must be
        the only one unless `nth` picks among several (the header and the
        footer both lead to Settings). */
    click(want, nth) {
      const els = find(want);
      if (nth === undefined ? els.length !== 1 : !els[nth]) {
        throw new Error(`click ${JSON.stringify(want)}: ${els.length} matches`);
      }
      const el = els[nth || 0];
      fire('click', { target: { closest: () => el } });
    },
    type(draft, value) {
      fire('input', { target: { dataset: { draft }, value } });
    },
    key(key, draft) {
      fire('keydown', { key, target: { dataset: { draft } } });
    },
  });
}

module.exports = { loadUI };

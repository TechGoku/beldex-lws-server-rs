// beldex-lws browser setup. The one-time token comes from the URL fragment
// and is sent with every API call. All server values are inserted as text.
'use strict';

const token = location.hash.slice(1);
const $ = (sel) => document.querySelector(sel);
const $$ = (sel) => [...document.querySelectorAll(sel)];
let step = 0;
let detected = null;
// The network a daemon reported, or null while none has answered.
let confirmed = null;

function h(tag, attrs = {}, ...children) {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (v === undefined || v === null || v === false) continue;
    if (k === 'class') el.className = v;
    else if (k.startsWith('on')) el.addEventListener(k.slice(2), v);
    else el.setAttribute(k, v === true ? '' : v);
  }
  for (const c of children.flat()) {
    if (c === null || c === undefined || c === false) continue;
    el.append(c instanceof Node ? c : document.createTextNode(String(c)));
  }
  return el;
}

async function api(path, body) {
  const res = await fetch(path, {
    method: body === undefined ? 'GET' : 'POST',
    headers: { 'X-Setup-Token': token, ...(body === undefined ? {} : { 'Content-Type': 'application/json' }) },
    body: body === undefined ? undefined : JSON.stringify(body),
    cache: 'no-store',
  });
  let data = null;
  try { data = await res.json(); } catch { /* empty */ }
  if (!res.ok) throw new Error((data && data.error) || `${res.status} ${res.statusText}`);
  return data;
}

function result(el, ok, text) {
  el.replaceChildren(h('span', { class: `pill ${ok ? 'ok' : 'bad'}` }, text));
}

const fmt = new Intl.NumberFormat();
const daemons = () => $('#daemons').value.split(/[\s,]+/).map((d) => d.trim()).filter(Boolean);
const radio = (name) => $(`input[name="${name}"]:checked`).value;

// ------------------------------------------------------------------ steps

function show(n) {
  step = n;
  for (const s of $$('[data-step]')) s.hidden = Number(s.dataset.step) !== n;
  $$('#steps span').forEach((s, i) => s.classList.toggle('done', i <= n));
  $('#back').hidden = n === 0 || n === 5;
  $('#next').hidden = n === 5;
  $('#next').textContent = n === 4 ? 'Write configuration' : 'Next';
  if (n === 4) review();
}

async function next() {
  try {
    if (step === 0) {
      if (!daemons().length) throw new Error('Enter at least one beldexd address.');
      if (!confirmed) await testDaemons();
      if (!$('#network').value) throw new Error('beldexd did not answer, so choose the network it runs.');
    }
    if (step === 1 && radio('db') === 'postgres' && !$('#pg-url').value.trim()) {
      throw new Error('Enter the PostgreSQL URL, or choose SQLite.');
    }
    if (step === 3 && detected && detected.out === $('#out').value && detected.out_exists && !$('#replace').checked) {
      throw new Error('A config file already exists there. Tick "replace it" or choose another path.');
    }
    if (step === 4) return apply();
    show(step + 1);
  } catch (e) {
    alertOn(e.message);
  }
}

function alertOn(message) {
  const el = step === 4 ? $('#apply-error') : $('#fatal');
  el.hidden = false;
  el.textContent = message;
  if (step !== 4) setTimeout(() => { el.hidden = true; }, 6000);
}

// -------------------------------------------------------------- detection

async function detect() {
  try {
    detected = await api('/api/detect');
  } catch (e) {
    $('#fatal').hidden = false;
    $('#fatal').textContent = e.message;
    $('#next').disabled = true;
    return;
  }
  const up = detected.daemons.filter((d) => d.ok);
  const list = $('#found');
  if (up.length) {
    list.append(h('p', { class: 'small muted' }, 'Found:'));
    for (const d of up) {
      list.append(h('label', { class: 'choice' },
        h('input', { type: 'checkbox', checked: d === up[0], 'data-url': d.url, onchange: syncFound }),
        h('span', {}, h('strong', {}, d.url),
          h('span', { class: 'muted small' }, `${d.network}, height ${fmt.format(d.height)}${d.syncing ? ', still syncing' : ''}${d.peers === 0 ? ', no peers' : ''}`))));
    }
    confirmNetwork(up[0].network);
  } else {
    list.append(h('p', { class: 'small muted' }, 'No beldexd answered on the default ports of this machine. Enter its address below.'));
  }
  $('#daemons').value = up.length ? up[0].url : (detected.daemons[0] || {}).url || '';
  if (detected.network && !confirmed) $('#network').value = detected.network;
  $('#data-dir').value = detected.data_dir;
  $('#out').value = detected.out;
  $('#replace-row').hidden = !detected.out_exists;
}

function syncFound() {
  const chosen = $$('#found input[type="checkbox"]').filter((c) => c.checked).map((c) => c.dataset.url);
  const typed = daemons().filter((d) => !$$('#found input').some((c) => c.dataset.url === d));
  $('#daemons').value = [...chosen, ...typed].join('\n');
}

async function testDaemons() {
  const out = $('#daemon-result');
  out.replaceChildren('Testing…');
  const results = await Promise.all(daemons().map((url) => api('/api/check/daemon', { url }).catch((e) => ({ url, ok: false, error: e.message }))));
  out.replaceChildren(...results.map((r) => h('div', {},
    h('span', { class: `pill ${r.ok ? 'ok' : 'bad'}` }, r.ok ? `${r.network}, height ${fmt.format(r.height)}` : 'no answer'), ' ', r.url)));
  const first = results.find((r) => r.ok);
  if (first) confirmNetwork(first.network);
  else confirmed = null;
  $('#network-hint').textContent = first
    ? 'Taken from the daemon.'
    : 'beldexd did not answer: choose the network it runs. The server checks it again when it starts.';
}

function confirmNetwork(network) {
  confirmed = network;
  $('#network').value = network;
}

// ---------------------------------------------------------------- choices

function syncChoices() {
  $('#pg-field').hidden = radio('db') !== 'postgres';
  const access = radio('access');
  $('#tls-fields').hidden = access !== 'tls';
  $('#proxy-fields').hidden = access !== 'proxy';
  const domain = $('#domain').value.trim() || 'lws.example.com';
  $('#caddyfile').textContent = `# /etc/caddy/Caddyfile: Caddy fetches the certificate itself\n${domain} {\n    reverse_proxy 127.0.0.1:${$('#port').value || 8443}\n}`;
}

function plan() {
  const access = radio('access');
  return {
    network: $('#network').value,
    daemons: daemons(),
    database: radio('db') === 'postgres' ? $('#pg-url').value.trim() : null,
    access,
    tls_cert: access === 'tls' ? $('#tls-cert').value.trim() : null,
    tls_key: access === 'tls' ? $('#tls-key').value.trim() : null,
    port: Number($('#port').value) || 8443,
    account_creation: $('#creation').value,
    data_dir: $('#data-dir').value.trim(),
    out: $('#out').value.trim(),
    replace: $('#replace').checked,
  };
}

function review() {
  const p = plan();
  const accessText = { local: 'only this machine', proxy: 'internet, through a TLS proxy here', tls: 'internet, HTTPS served directly' }[p.access];
  const rows = [
    ['Network', confirmed === p.network ? p.network : `${p.network} (not confirmed: beldexd did not answer)`],
    ['beldexd', p.daemons.join(', ')],
    ['Database', p.database ? 'PostgreSQL' : `SQLite in ${p.data_dir}`],
    ['Wallets', `${accessText}, port ${p.port}`],
    ['New wallets', $('#creation').selectedOptions[0].textContent],
    ['Data directory', p.data_dir],
    ['Config file', p.out + (p.replace ? ' (replaced)' : '')],
    ['Dashboard', 'http://127.0.0.1:8444/ on the server'],
  ];
  $('#review').replaceChildren(...rows.map(([k, v]) => h('tr', {}, h('td', { class: 'muted' }, k), h('td', { class: 'wrap' }, v))));
  $('#apply-error').textContent = '';
}

async function apply() {
  const button = $('#next');
  button.disabled = true;
  button.textContent = 'Writing…';
  try {
    const r = await api('/api/apply', plan());
    show(5);
    const commands = r.service
      ? ['# The installer now starts the service and checks it. Later:', 'sudo -u beldex-lws beldex-lws doctor --config /etc/beldex-lws/lws.toml', 'journalctl -u beldex-lws -f']
      : r.commands;
    $('#result').replaceChildren(
      h('table', {}, h('tbody', {},
        [['Config file', r.out], ['Wallet API', r.wallet_url], ['Dashboard', `${r.dashboard_url} (sign in with the token in ${r.token_file})`], ['Sealing key', `${r.seal_key}: back it up with the database`]]
          .map(([k, v]) => h('tr', {}, h('td', { class: 'muted' }, k), h('td', { class: 'wrap' }, v))))),
      h('h3', {}, 'Next'),
      h('pre', { class: 'box' }, commands.join('\n')),
      h('details', {}, h('summary', { class: 'small muted' }, 'Show the config file'), h('pre', { class: 'box' }, r.config)),
    );
  } catch (e) {
    alertOn(e.message);
  } finally {
    button.disabled = false;
    if (step === 4) button.textContent = 'Write configuration';
  }
}

document.addEventListener('DOMContentLoaded', () => {
  if (!token) {
    $('#fatal').hidden = false;
    $('#fatal').textContent = 'Open the full link printed by `beldex-lws setup --web`; it carries a one-time token.';
    $('#next').disabled = true;
    return;
  }
  $('#next').addEventListener('click', next);
  $('#back').addEventListener('click', () => show(Math.max(0, step - 1)));
  $('#test-daemons').addEventListener('click', () => testDaemons().catch((e) => alertOn(e.message)));
  $('#daemons').addEventListener('input', () => { confirmed = null; });
  $('#test-db').addEventListener('click', async () => {
    const out = $('#db-result');
    out.replaceChildren('Connecting…');
    const r = await api('/api/check/database', { url: $('#pg-url').value.trim() }).catch((e) => ({ ok: false, error: e.message }));
    result(out, r.ok, r.ok ? 'connected' : r.error);
  });
  $('#test-tls').addEventListener('click', async () => {
    const out = $('#tls-result');
    const r = await api('/api/check/tls', { cert: $('#tls-cert').value.trim(), key: $('#tls-key').value.trim() }).catch((e) => ({ ok: false, error: e.message }));
    result(out, r.ok, r.ok ? 'certificate and key load' : r.error);
  });
  for (const el of $$('input[name="db"], input[name="access"], #domain, #port')) el.addEventListener('input', syncChoices);
  syncChoices();
  show(0);
  detect();
});

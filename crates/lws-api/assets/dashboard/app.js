// beldex-lws admin dashboard. No dependencies. Every value from the server is
// inserted as text (never as HTML). The admin token is kept in memory, or in
// sessionStorage when "stay signed in" is ticked, and sent as a bearer token.
'use strict';

const REFRESH_MS = 5000;
const HISTORY = 180; // samples kept for charts: 15 minutes at 5 s

const state = {
  token: null,
  overview: null,
  history: [],
  tab: 'overview',
  accounts: { rows: [], next: null, selected: new Set() },
  requests: { rows: [], selected: new Set() },
  audit: { rows: [], next: null },
  timer: null,
};

// ---------------------------------------------------------------- helpers

const $ = (sel, root = document) => root.querySelector(sel);
const $$ = (sel, root = document) => [...root.querySelectorAll(sel)];

/** h('td', {class: 'num'}, 'text', childNode) */
function h(tag, attrs = {}, ...children) {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (v === undefined || v === null || v === false) continue;
    if (k === 'class') el.className = v;
    else if (k.startsWith('on')) el.addEventListener(k.slice(2), v);
    else if (k === 'text') el.textContent = v;
    else el.setAttribute(k, v === true ? '' : v);
  }
  for (const c of children.flat()) {
    if (c === null || c === undefined || c === false) continue;
    el.append(c instanceof Node ? c : document.createTextNode(String(c)));
  }
  return el;
}

const fmt = new Intl.NumberFormat();
const num = (n) => (n === null || n === undefined ? '–' : fmt.format(n));

function rate(n) {
  if (n === null || n === undefined || !isFinite(n)) return '–';
  if (n >= 100) return fmt.format(Math.round(n));
  return n.toFixed(n >= 10 ? 1 : 2);
}

function ms(seconds) {
  if (seconds === null || seconds === undefined) return '–';
  const v = seconds * 1000;
  return v >= 100 ? `${Math.round(v)} ms` : `${v.toFixed(1)} ms`;
}

function duration(s) {
  if (s === null || s === undefined) return '–';
  if (s < 60) return `${Math.round(s)}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m`;
  if (s < 86400) return `${Math.floor(s / 3600)}h ${Math.floor((s % 3600) / 60)}m`;
  return `${Math.floor(s / 86400)}d ${Math.floor((s % 86400) / 3600)}h`;
}

function ago(unix) {
  if (!unix) return '–';
  return `${duration(Date.now() / 1000 - unix)} ago`;
}

function when(unix) {
  if (!unix) return '–';
  return new Date(unix * 1000).toLocaleString();
}

function short(address) {
  return address.length > 24 ? `${address.slice(0, 12)}…${address.slice(-8)}` : address;
}

function pill(kind, text) {
  return h('span', { class: `pill ${kind}` }, text);
}

function toast(message, error = false) {
  const t = h('div', { class: `toast${error ? ' error' : ''}`, role: error ? 'alert' : 'status' }, message);
  $('#toasts').append(t);
  setTimeout(() => t.remove(), error ? 8000 : 4000);
}

function storage() {
  try { return window.sessionStorage; } catch { return null; }
}

// -------------------------------------------------------------------- api

class AuthError extends Error {}

async function api(path, { method = 'GET', body } = {}) {
  const res = await fetch(path, {
    method,
    headers: {
      Authorization: `Bearer ${state.token}`,
      ...(body === undefined ? {} : { 'Content-Type': 'application/json' }),
    },
    body: body === undefined ? undefined : JSON.stringify(body),
    cache: 'no-store',
    credentials: 'omit',
  });
  if (res.status === 401 || res.status === 403) throw new AuthError('The admin token was not accepted.');
  let data = null;
  try { data = await res.json(); } catch { /* empty body */ }
  if (!res.ok) throw new Error((data && data.error) || `${res.status} ${res.statusText}`);
  return data;
}

function failed(e) {
  if (e instanceof AuthError) {
    signOut(e.message);
  } else {
    toast(e.message || String(e), true);
  }
}

// ------------------------------------------------------------- sign in/out

function signOut(message) {
  state.token = null;
  storage()?.removeItem('lws-admin-token');
  clearInterval(state.timer);
  $('#app').hidden = true;
  $('#signin').hidden = false;
  $('#signin-error').textContent = message || '';
  $('#token').value = '';
  $('#token').focus();
}

async function signIn(token, remember) {
  state.token = token.trim();
  try {
    await refresh();
  } catch (e) {
    if (e instanceof AuthError) return signOut(e.message);
    $('#signin-error').textContent = e.message;
    return;
  }
  if (remember) storage()?.setItem('lws-admin-token', state.token);
  $('#signin').hidden = true;
  $('#app').hidden = false;
  show(state.tab);
  schedule();
}

function schedule() {
  clearInterval(state.timer);
  if ($('#auto').checked) state.timer = setInterval(() => refresh().catch(failed), REFRESH_MS);
}

// ---------------------------------------------------------------- refresh

async function refresh() {
  const o = await api('/api/v1/overview');
  state.overview = o;
  const m = o.metrics;
  state.history.push({
    t: Date.now() / 1000,
    requests: m.http_requests_total,
    errors: m.http_server_errors_total,
    blocks: m.blocks_scanned_total,
    latency: m.http_request_duration_seconds,
    lag: o.scanner.lag,
    started: m.started_at,
  });
  if (state.history.length > HISTORY) state.history.shift();
  renderHeader();
  if (state.tab === 'overview') renderOverview();
  if (state.tab === 'cluster') renderCluster();
  if (state.tab === 'settings' && !$('#server-info').childElementCount) loadSettings().catch(failed);
}

function renderHeader() {
  const o = state.overview;
  const p = $('#status-pill');
  p.className = `pill ${o.status === 'ok' ? 'ok' : o.status === 'degraded' ? 'warn' : 'bad'}`;
  p.textContent = o.status === 'ok' ? 'Healthy' : o.status === 'degraded' ? 'Degraded' : 'Down';
  $('#network').textContent = o.network;
  $('#refreshed').textContent = `Updated ${new Date().toLocaleTimeString()}`;
  const pending = o.requests.create + o.requests.import;
  const badge = $('#requests-count');
  badge.hidden = pending === 0;
  badge.textContent = num(pending);
}

/** Per-second rate of counter `key` between samples a and b; null across a
 *  counter reset (a restart, or a different process behind a balancer). */
function perSecond(a, b, key) {
  const dt = b.t - a.t;
  if (!(dt > 0) || b[key] < a[key] || b.started !== a.started) return null;
  return (b[key] - a[key]) / dt;
}

/** Per-second rate of `key` between the last two samples. */
function currentRate(key) {
  const hs = state.history;
  return hs.length < 2 ? null : perSecond(hs[hs.length - 2], hs[hs.length - 1], key);
}

/** Quantile of the latency observed between two samples, from bucket deltas. */
function windowQuantile(a, b, q) {
  if (!a || !b || !b.counts || !a.counts || b.count < a.count) return null;
  const counts = b.counts.map((c, i) => Math.max(0, c - (a.counts[i] || 0)));
  const total = counts.reduce((x, y) => x + y, 0);
  if (total === 0) return null;
  const rank = q * total;
  let seen = 0;
  for (let i = 0; i < counts.length; i++) {
    if (counts[i] > 0 && seen + counts[i] >= rank) {
      const lower = i === 0 ? 0 : b.bounds[i - 1];
      const upper = b.bounds[i];
      if (upper === undefined) return lower;
      return lower + (upper - lower) * Math.min(1, Math.max(0, (rank - seen) / counts[i]));
    }
    seen += counts[i];
  }
  return null;
}

function series(fn) {
  const hs = state.history;
  const out = [];
  for (let i = 1; i < hs.length; i++) out.push(fn(hs[i - 1], hs[i]));
  return out;
}

// ------------------------------------------------------------------ chart

function chart(lines, { unit = '' } = {}) {
  const W = 600, H = 150, L = 44, R = 8, T = 8, B = 18;
  const svgNS = 'http://www.w3.org/2000/svg';
  const svg = document.createElementNS(svgNS, 'svg');
  svg.setAttribute('viewBox', `0 0 ${W} ${H}`);
  svg.setAttribute('class', 'chart');
  svg.setAttribute('role', 'img');
  svg.setAttribute('preserveAspectRatio', 'none');
  const values = lines.flatMap((l) => l.values.filter((v) => v !== null && isFinite(v)));
  const top = Math.max(0, ...values);
  const max = (top > 0 ? top : 1) * 1.15;
  const n = Math.max(2, ...lines.map((l) => l.values.length));
  const x = (i) => L + ((W - L - R) * i) / (n - 1);
  const y = (v) => T + (H - T - B) * (1 - v / max);
  for (let g = 0; g <= 3; g++) {
    const v = (max * g) / 3;
    const line = document.createElementNS(svgNS, 'line');
    line.setAttribute('x1', L); line.setAttribute('x2', W - R);
    line.setAttribute('y1', y(v)); line.setAttribute('y2', y(v));
    line.setAttribute('class', 'grid-line');
    svg.append(line);
    const label = document.createElementNS(svgNS, 'text');
    label.setAttribute('x', L - 6); label.setAttribute('y', y(v) + 3);
    label.setAttribute('text-anchor', 'end');
    label.setAttribute('class', 'axis-label');
    label.textContent = v >= 100 ? fmt.format(Math.round(v)) : v.toFixed(v >= 10 ? 0 : 1);
    svg.append(label);
  }
  for (const l of lines) {
    let d = '';
    l.values.forEach((v, i) => {
      if (v === null || !isFinite(v)) return;
      d += `${d ? 'L' : 'M'}${x(i + (n - l.values.length)).toFixed(1)},${y(v).toFixed(1)}`;
    });
    const path = document.createElementNS(svgNS, 'path');
    path.setAttribute('d', d);
    path.setAttribute('class', 'line');
    path.setAttribute('stroke', `var(${l.color})`);
    svg.append(path);
  }
  const legend = h('div', { class: 'legend' });
  for (const l of lines) {
    const last = [...l.values].reverse().find((v) => v !== null && isFinite(v));
    const item = h('span', {}, `${l.name}: ${last === undefined ? '–' : rate(last)}${unit}`);
    item.style.setProperty('--c', `var(${l.color})`);
    legend.append(item);
  }
  const box = h('div');
  if (values.length === 0) box.append(h('div', { class: 'empty small' }, 'Collecting data…'));
  else box.append(svg);
  box.append(legend);
  return box;
}

// --------------------------------------------------------------- overview

function kpi(label, value, sub, kind) {
  return h('div', { class: 'card kpi' },
    h('div', { class: 'label' }, label),
    h('div', { class: 'value' }, value, kind ? ' ' : '', kind ? pill(kind[0], kind[1]) : null),
    sub ? h('div', { class: 'sub' }, sub) : null);
}

function renderOverview() {
  const o = state.overview;
  const m = o.metrics;
  const hs = state.history;
  const last = hs[hs.length - 1], prev = hs[hs.length - 2];
  const rps = currentRate('requests');
  const errs = currentRate('errors');
  const p95 = windowQuantile(prev && prev.latency, last && last.latency, 0.95);
  const bps = currentRate('blocks');
  const lagKind = o.scanner.lag <= 2 ? ['ok', 'in sync'] : o.scanner.lag <= 100 ? ['warn', 'catching up'] : ['bad', 'behind'];

  $('#kpis').replaceChildren(
    kpi('Accounts', num(o.accounts.total), `${num(o.accounts.active)} active · ${num(o.accounts.inactive)} inactive · ${num(o.accounts.hidden)} hidden`),
    kpi('Behind the chain', num(o.accounts.behind), `active accounts more than ${o.accounts.behind_blocks} blocks behind`),
    kpi('Chain height', num(o.chain.height), `scanner lag ${num(o.scanner.lag)} blocks`, o.chain.height ? lagKind : null),
    kpi('Requests / s', rate(rps), `${num(m.http_requests_total)} since start · ${num(m.http_in_flight)} in flight`),
    kpi('Latency p95', ms(p95 ?? m.http_request_duration_seconds.p95), p95 === null ? 'since start' : 'last 5 seconds'),
    kpi('Server errors / s', rate(errs), `${num(m.http_server_errors_total)} since start · ${num(m.http_shed_total)} shed · ${num(m.http_rate_limited_total)} rate-limited`),
    kpi('Pending requests', num(o.requests.create + o.requests.import), `${num(o.requests.create)} create · ${num(o.requests.import)} import`),
    kpi('Uptime', duration(m.uptime_seconds), `version ${m.version}`),
  );

  $('#chart-rps').replaceChildren(chart([
    { name: 'requests', color: '--chart-1', values: series((a, b) => perSecond(a, b, 'requests')) },
    { name: '5xx', color: '--chart-2', values: series((a, b) => perSecond(a, b, 'errors')) },
  ], { unit: '/s' }));
  $('#chart-latency').replaceChildren(chart([
    { name: 'p50', color: '--chart-1', values: series((a, b) => { const v = windowQuantile(a.latency, b.latency, 0.5); return v === null ? null : v * 1000; }) },
    { name: 'p95', color: '--chart-3', values: series((a, b) => { const v = windowQuantile(a.latency, b.latency, 0.95); return v === null ? null : v * 1000; }) },
  ], { unit: ' ms' }));
  $('#chart-blocks').replaceChildren(chart([
    { name: 'blocks', color: '--chart-1', values: series((a, b) => perSecond(a, b, 'blocks')) },
  ], { unit: '/s' }));
  $('#chart-lag').replaceChildren(chart([{ name: 'lag', color: '--chart-3', values: hs.map((s) => s.lag) }], { unit: ' blocks' }));

  const daemons = o.daemons.map((d) => h('tr', {},
    h('td', { class: 'mono' }, d.url),
    h('td', {}, d.healthy ? pill('ok', 'up') : pill('bad', 'benched')),
    h('td', { class: 'num' }, num(d.height)),
    h('td', { class: 'num' }, d.height && o.chain.height ? num(Math.max(0, o.chain.height - d.height)) : '–'),
    h('td', { class: 'num' }, num(d.consecutive_failures)),
    h('td', { class: 'wrap small muted' }, d.last_error || ''),
  ));
  $('#daemons').replaceChildren(h('div', { class: 'table-wrap' }, h('table', {},
    h('thead', {}, h('tr', {}, h('th', {}, 'Endpoint'), h('th', {}, 'State'), h('th', { class: 'num' }, 'Height'), h('th', { class: 'num' }, 'Behind'), h('th', { class: 'num' }, 'Failures'), h('th', {}, 'Last error'))),
    h('tbody', {}, daemons))));

  const kv = (rows) => h('table', {}, h('tbody', {}, rows.map(([k, v]) => h('tr', {}, h('td', { class: 'muted' }, k), h('td', {}, v)))));
  const p = m.scan_batch_duration_seconds, f = m.daemon_fetch_duration_seconds;
  $('#scanning').replaceChildren(kv([
    ['Accounts scanned here', num(m.scanned_accounts)],
    ['Lowest scan height', num(o.scanner.min_scan_height)],
    ['Blocks scanned', num(m.blocks_scanned_total)],
    ['Outputs / spends found', `${num(m.outputs_found_total)} / ${num(m.spends_found_total)}`],
    ['Reorgs handled', num(m.reorgs_total)],
    ['Batch time p50 / p95', `${ms(p.p50)} / ${ms(p.p95)}`],
    ['Block download p50 / p95', `${ms(f.p50)} / ${ms(f.p95)}`],
    ['Scan errors / rejected commits', `${num(m.scan_errors_total)} / ${num(m.commits_rejected_total)}`],
    ['New-block notifications', m.block_notifications_connected ? pill('ok', `connected · ${num(m.block_notifications_total)} received`) : pill('warn', 'off: polling')],
  ]));
  const db = o.database;
  $('#database').replaceChildren(kv([
    ['Backend', db.backend],
    ['Primary', db.primary ? pill('ok', 'reachable') : pill('bad', 'unreachable')],
    ['Read pool', db.read ? pill('ok', 'reachable') : pill('bad', 'unreachable')],
    ['Highest stored block', num(db.stored_tip)],
    ['Accounts created via /login', num(m.accounts_created_total)],
    ['Admin changes since start', num(m.admin_actions_total)],
  ]));
}

// --------------------------------------------------------------- accounts

function accountFilter() {
  const params = new URLSearchParams();
  const q = $('#account-q').value.trim();
  if (q) params.set('q', q);
  const status = $('#account-status').value;
  if (status !== 'all') params.set('status', status);
  if ($('#account-behind').checked) params.set('behind', 'true');
  params.set('limit', '100');
  return params;
}

async function loadAccounts(more = false) {
  const params = accountFilter();
  if (more && state.accounts.next) params.set('before', state.accounts.next);
  const page = await api(`/api/v1/accounts?${params}`);
  if (!more) { state.accounts.rows = []; state.accounts.selected.clear(); }
  state.accounts.rows.push(...page.accounts);
  state.accounts.next = page.next_before;
  renderAccounts();
}

const statusKind = { active: 'ok', inactive: 'warn', hidden: '' };

function renderAccounts() {
  const sel = state.accounts.selected;
  const rows = state.accounts.rows.map((a) => h('tr', {},
    h('td', {}, h('input', { type: 'checkbox', 'aria-label': 'Select', checked: sel.has(a.address), onchange: (e) => { e.target.checked ? sel.add(a.address) : sel.delete(a.address); bulkBar(); } })),
    h('td', { class: 'mono', title: a.address }, short(a.address), a.admin ? h('span', { class: 'tag' }, 'admin') : null),
    h('td', {}, pill(statusKind[a.status], a.status)),
    h('td', { class: 'num' }, num(a.scan_height)),
    h('td', { class: 'num' }, a.behind === null ? '–' : num(a.behind)),
    h('td', { class: 'num' }, num(a.start_height)),
    h('td', { class: 'small' }, when(a.created_at)),
    h('td', { class: 'small' }, ago(a.last_access)),
    h('td', {}, h('button', { class: 'link', type: 'button', onclick: () => showAccount(a.address) }, 'Details')),
  ));
  $('#accounts').replaceChildren(...(rows.length ? rows : [h('tr', {}, h('td', { colspan: 9, class: 'empty' }, 'No accounts match.'))]));
  $('#accounts-more').hidden = !state.accounts.next;
  $('#select-all').checked = rows.length > 0 && sel.size === state.accounts.rows.length;
  bulkBar();
}

function bulkBar() {
  const n = state.accounts.selected.size;
  $('#bulk').hidden = n === 0;
  $('#bulk-count').textContent = `${num(n)} selected`;
}

function confirmDialog(title, text, extra) {
  return new Promise((resolve) => {
    const d = $('#confirm');
    $('#confirm-title').textContent = title;
    $('#confirm-text').textContent = text;
    $('#confirm-extra').replaceChildren(...(extra ? [extra] : []));
    d.returnValue = '';
    d.addEventListener('close', () => resolve(d.returnValue === 'ok'), { once: true });
    d.showModal();
  });
}

function heightField(defaultValue) {
  const input = h('input', { type: 'number', min: '0', id: 'rescan-height', value: String(defaultValue), required: true });
  return { input, el: h('div', { class: 'field' }, h('label', { for: 'rescan-height' }, 'Rescan from height'), input,
    h('span', { class: 'hint' }, 'History above this height is rebuilt from the chain. The account keeps being served meanwhile.')) };
}

async function setStatus(addresses, status) {
  const verb = { active: 'Activate', inactive: 'Deactivate', hidden: 'Hide' }[status];
  const text = { active: 'They will be scanned and served.', inactive: 'They stop being scanned but are still served.', hidden: 'They stop being scanned and are no longer served to wallets.' }[status];
  if (!(await confirmDialog(`${verb} ${num(addresses.length)} account(s)?`, text))) return;
  const r = await api('/api/v1/accounts/status', { method: 'POST', body: { status, addresses } });
  toast(`Updated ${num(r.updated.length)} account(s).`);
  await loadAccounts();
}

async function rescan(addresses, from) {
  const field = heightField(from ?? 0);
  if (!(await confirmDialog(`Rescan ${num(addresses.length)} account(s)?`, '', field.el))) return;
  const height = Number(field.input.value);
  if (!Number.isInteger(height) || height < 0) return toast('Enter a block height.', true);
  const r = await api('/api/v1/accounts/rescan', { method: 'POST', body: { height, addresses } });
  toast(`Rescanning ${num(r.updated.length)} account(s) from ${num(height)}.`);
  await loadAccounts();
}

async function showAccount(address) {
  const a = await api(`/api/v1/accounts/${encodeURIComponent(address)}`);
  const kv = [
    ['Address', h('span', { class: 'mono wrap' }, a.address)],
    ['Status', pill(statusKind[a.status], a.status)],
    ['Scanned to', num(a.scan_height)],
    ['Behind', a.behind === null ? '–' : num(a.behind)],
    ['Started at height', num(a.start_height)],
    ['Outputs / spends', `${num(a.outputs)} / ${num(a.spends)}`],
    ['Created', when(a.created_at)],
    ['Last seen', `${when(a.last_access)} (${ago(a.last_access)})`],
    ['Flags', [a.admin ? 'admin' : null, a.generated_locally ? 'generated locally' : null].filter(Boolean).join(', ') || 'none'],
  ];
  const actions = h('div', { class: 'row' },
    ...['active', 'inactive', 'hidden'].filter((s) => s !== a.status).map((s) =>
      h('button', { class: 'btn', type: 'button', onclick: () => { $('#detail-dialog').close(); setStatus([a.address], s).catch(failed); } },
        { active: 'Activate', inactive: 'Deactivate', hidden: 'Hide' }[s])),
    h('button', { class: 'btn', type: 'button', onclick: () => { $('#detail-dialog').close(); rescan([a.address], a.start_height).catch(failed); } }, 'Rescan…'));
  $('#detail').replaceChildren(
    h('table', {}, h('tbody', {}, kv.map(([k, v]) => h('tr', {}, h('td', { class: 'muted' }, k), h('td', { class: 'wrap' }, v))))),
    actions);
  $('#detail-dialog').showModal();
}

function addAccount() {
  const d = $('#account-dialog');
  $('#account-form').reset();
  $('#account-error').textContent = '';
  d.showModal();
}

async function submitAccount(e) {
  if (e.submitter && e.submitter.value === 'cancel') return;
  e.preventDefault();
  const height = $('#new-height').value.trim();
  const body = {
    address: $('#new-address').value.trim(),
    view_key: $('#new-key').value.trim(),
    admin: $('#new-admin').checked,
    ...(height === '' ? {} : { start_height: Number(height) }),
  };
  try {
    const r = await api('/api/v1/accounts', { method: 'POST', body });
    $('#account-dialog').close();
    toast(r.created ? `Added; scanning from ${num(r.start_height)}.` : 'That account already exists.');
    if (state.tab === 'accounts') await loadAccounts();
  } catch (err) {
    if (err instanceof AuthError) return failed(err);
    $('#account-error').textContent = err.message;
  }
}

// --------------------------------------------------------------- requests

async function loadRequests() {
  const r = await api('/api/v1/requests');
  state.requests.rows = r.requests;
  state.requests.selected.clear();
  renderRequests();
}

const requestKey = (r) => `${r.kind}:${r.address}`;

function renderRequests() {
  const sel = state.requests.selected;
  const rows = state.requests.rows.map((r) => h('tr', {},
    h('td', {}, h('input', { type: 'checkbox', 'aria-label': 'Select', checked: sel.has(requestKey(r)), onchange: (e) => { e.target.checked ? sel.add(requestKey(r)) : sel.delete(requestKey(r)); } })),
    h('td', {}, r.kind === 'create' ? pill('', 'create') : pill('warn', 'import')),
    h('td', { class: 'mono', title: r.address }, short(r.address)),
    h('td', { class: 'num' }, num(r.start_height)),
    h('td', { class: 'small' }, when(r.created_at)),
  ));
  $('#requests').replaceChildren(...(rows.length ? rows : [h('tr', {}, h('td', { colspan: 5, class: 'empty' }, 'Nothing is waiting for approval.'))]));
  $('#select-all-requests').checked = false;
}

async function decide(action) {
  const chosen = state.requests.rows.filter((r) => state.requests.selected.has(requestKey(r)));
  if (!chosen.length) return toast('Select requests first.', true);
  const verb = action === 'accept' ? 'Accept' : 'Reject';
  if (!(await confirmDialog(`${verb} ${num(chosen.length)} request(s)?`, action === 'accept' ? 'Accepted imports rescan the account from its start height.' : 'Rejected requests are discarded; the wallet can ask again.'))) return;
  let done = 0;
  for (const kind of ['create', 'import']) {
    const addresses = chosen.filter((r) => r.kind === kind).map((r) => r.address);
    if (!addresses.length) continue;
    const r = await api(`/api/v1/requests/${action}`, { method: 'POST', body: { kind, addresses } });
    done += r.updated.length;
  }
  toast(`${verb}ed ${num(done)} request(s).`);
  await loadRequests();
  await refresh();
}

// ---------------------------------------------------------------- cluster

function renderCluster() {
  const o = state.overview;
  const rows = o.instances.map((i) => {
    const m = i.stats || {};
    const lat = m.http_request_duration_seconds || {};
    return h('tr', {},
      h('td', { class: 'mono' }, i.name, o.server && o.server.instance === i.name ? h('span', { class: 'tag' }, 'this') : null),
      h('td', {}, i.role),
      h('td', {}, i.version),
      h('td', {}, duration(Date.now() / 1000 - i.started_at)),
      h('td', {}, i.stale ? pill('bad', `stale · ${ago(i.heartbeat_at)}`) : pill('ok', ago(i.heartbeat_at))),
      h('td', { class: 'num' }, num(m.http_requests_total)),
      h('td', { class: 'num' }, ms(lat.p95)),
      h('td', { class: 'num' }, num(m.blocks_scanned_total)),
      h('td', { class: 'num' }, num(m.scanned_accounts)),
      h('td', { class: 'num' }, m.daemon_height && m.min_scan_height !== undefined && m.scanned_accounts ? num(Math.max(0, m.daemon_height - m.min_scan_height - 1)) : '–'),
    );
  });
  $('#instances').replaceChildren(...(rows.length ? rows : [h('tr', {}, h('td', { colspan: 10, class: 'empty' }, 'No instance has reported yet.'))]));
  const leases = o.leases.map((l) => h('tr', {},
    h('td', { class: 'mono' }, l.name), h('td', { class: 'mono' }, l.holder),
    h('td', {}, l.live ? pill('ok', `held · renews within ${duration(Math.max(0, l.expires_at - Date.now() / 1000))}`) : pill('warn', 'expired: a standby will take it'))));
  $('#leases').replaceChildren(leases.length
    ? h('div', { class: 'table-wrap' }, h('table', {}, h('thead', {}, h('tr', {}, h('th', {}, 'Shard'), h('th', {}, 'Held by'), h('th', {}, 'State'))), h('tbody', {}, leases)))
    : h('p', { class: 'muted' }, 'No scanner holds a lease yet.'));
}

// ------------------------------------------------------------------ audit

async function loadAudit(more = false) {
  const params = new URLSearchParams({ limit: '100' });
  if (more && state.audit.next) params.set('before', state.audit.next);
  const page = await api(`/api/v1/audit?${params}`);
  if (!more) state.audit.rows = [];
  state.audit.rows.push(...page.entries);
  state.audit.next = page.next_before;
  const rows = state.audit.rows.map((e) => h('tr', {},
    h('td', { class: 'small' }, when(e.at)),
    h('td', {}, e.actor),
    h('td', { class: 'mono' }, e.remote),
    h('td', {}, h('span', { class: 'tag' }, e.action)),
    h('td', { class: 'wrap mono small' }, typeof e.detail === 'string' ? e.detail : JSON.stringify(e.detail)),
  ));
  $('#audit').replaceChildren(...(rows.length ? rows : [h('tr', {}, h('td', { colspan: 5, class: 'empty' }, 'No changes recorded yet.'))]));
  $('#audit-more').hidden = !state.audit.next;
}

// --------------------------------------------------------------- settings

async function loadSettings() {
  const c = await api('/api/v1/config');
  const table = (obj) => h('table', {}, h('tbody', {}, Object.entries(obj || {}).map(([k, v]) =>
    h('tr', {}, h('td', { class: 'muted' }, k.replaceAll('_', ' ')), h('td', { class: 'mono wrap' }, Array.isArray(v) ? (v.join(', ') || '–') : typeof v === 'object' && v !== null ? JSON.stringify(v) : String(v))))));
  $('#server-info').replaceChildren(table(c.server));
  $('#api-config').replaceChildren(table(c.api));
}

// ------------------------------------------------------------------- tabs

function show(tab) {
  state.tab = tab;
  for (const b of $$('nav.tabs button')) b.setAttribute('aria-selected', String(b.dataset.tab === tab));
  for (const v of $$('[data-view]')) v.hidden = v.dataset.view !== tab;
  const load = {
    overview: async () => renderOverview(),
    accounts: () => loadAccounts(),
    requests: () => loadRequests(),
    cluster: async () => renderCluster(),
    audit: () => loadAudit(),
    settings: () => loadSettings(),
  }[tab];
  if (state.overview) load().catch(failed);
  try { history.replaceState(null, '', `#${tab}`); } catch { /* sandboxed */ }
}

function applyTheme(theme) {
  if (theme) document.documentElement.dataset.theme = theme;
  else delete document.documentElement.dataset.theme;
}

// ------------------------------------------------------------------- init

document.addEventListener('DOMContentLoaded', () => {
  let theme = null;
  try { theme = localStorage.getItem('lws-theme'); } catch { /* storage blocked */ }
  applyTheme(theme);
  $('#theme').addEventListener('click', () => {
    const dark = document.documentElement.dataset.theme === 'dark' ||
      (!document.documentElement.dataset.theme && matchMedia('(prefers-color-scheme: dark)').matches);
    const next = dark ? 'light' : 'dark';
    applyTheme(next);
    try { localStorage.setItem('lws-theme', next); } catch { /* storage blocked */ }
  });

  $('#signin-form').addEventListener('submit', (e) => { e.preventDefault(); signIn($('#token').value, $('#remember').checked); });
  $('#signout').addEventListener('click', () => signOut());
  $('#auto').addEventListener('change', schedule);
  for (const b of $$('nav.tabs button')) b.addEventListener('click', () => show(b.dataset.tab));

  $('#account-filter').addEventListener('submit', (e) => { e.preventDefault(); loadAccounts().catch(failed); });
  $('#account-status').addEventListener('change', () => loadAccounts().catch(failed));
  $('#account-behind').addEventListener('change', () => loadAccounts().catch(failed));
  $('#accounts-more').addEventListener('click', () => loadAccounts(true).catch(failed));
  $('#select-all').addEventListener('change', (e) => {
    state.accounts.selected = new Set(e.target.checked ? state.accounts.rows.map((a) => a.address) : []);
    renderAccounts();
  });
  for (const b of $$('[data-bulk]')) b.addEventListener('click', () => {
    const addresses = [...state.accounts.selected];
    (b.dataset.bulk === 'rescan' ? rescan(addresses) : setStatus(addresses, b.dataset.bulk)).catch(failed);
  });
  $('#add-account').addEventListener('click', addAccount);
  $('#account-form').addEventListener('submit', submitAccount);

  $('#select-all-requests').addEventListener('change', (e) => {
    state.requests.selected = new Set(e.target.checked ? state.requests.rows.map(requestKey) : []);
    renderRequests();
    $('#select-all-requests').checked = e.target.checked;
  });
  $('#accept').addEventListener('click', () => decide('accept').catch(failed));
  $('#reject').addEventListener('click', () => decide('reject').catch(failed));
  $('#audit-more').addEventListener('click', () => loadAudit(true).catch(failed));

  const initial = (location.hash || '#overview').slice(1);
  if ($(`nav.tabs button[data-tab="${CSS.escape(initial)}"]`)) state.tab = initial;
  show(state.tab);

  const saved = storage()?.getItem('lws-admin-token');
  if (saved) signIn(saved, true);
  else signOut();
});

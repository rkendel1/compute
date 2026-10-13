/*! @stack-ui/core 0.1.0 — classic build. Exposes window.StackUI. */
'use strict';
(function (global) {
'use strict';
// ---- states.js
// State semantics (docs/design-system-contract.md §6). Pure data and pure
// functions: no DOM, no network, no storage.
//
// Mappings are keyed by source FIELD, never by bare string: the same word means
// different things in different fields (Compute `ready` is an in-progress
// release phase and also verified process readiness).

const TONES = Object.freeze(['positive', 'progress', 'attention', 'critical', 'neutral', 'unknown']);

const GLYPHS = Object.freeze({
  positive: '●',
  progress: '◐',
  attention: '▲',
  critical: '✕',
  neutral: '○',
  unknown: '?',
});

const OUTCOMES = Object.freeze(['submitted', 'accepted', 'completed', 'failed', 'cancelled', 'rejected', 'outcome_unknown']);
const OUTCOME_LABELS = Object.freeze({
  submitted: 'Sending…',
  accepted: 'Started',
  completed: 'Done',
  failed: 'Failed',
  cancelled: 'Cancelled',
  rejected: 'Not accepted',
  outcome_unknown: 'Result unknown — check current state',
});
const VERIFICATION_STATES = Object.freeze(['verified', 'verification_failed', 'not_verified', 'not_evaluated']);
const FRESHNESS_STATES = Object.freeze(['live', 'cached', 'stale', 'unknown']);
const CAPABILITY_STATES = Object.freeze(['available', 'partial', 'unsupported']);

/// "network_ready" → "Network ready". Matches the labels products show today.
function humanize(value) {
  const text = String(value).replace(/_/g, ' ');
  return text ? text[0].toUpperCase() + text.slice(1) : text;
}

const P = 'positive';
const G = 'progress';
const A = 'attention';
const C = 'critical';
const N = 'neutral';
const U = 'unknown';

/// Condition-axis mappings. A value is either a tone or [tone, label].
/// Sources are cited so a reviewer can check each table against its enum.
const CONDITION = {
  // compute: crates/compute-environment/src/model.rs `ActualState`
  'compute.environment.actual_state': {
    running: P, completed: P, pending: G, starting: G, stopping: G, degraded: A, failed: C, denied: C, stopped: N,
  },
  // compute: crates/compute-environment/src/status.rs `Health`
  'compute.health': { healthy: P, unhealthy: C, unknown: U },
  // compute: crates/compute-environment/src/status.rs `ReadinessState` (environment)
  'compute.environment.readiness': {
    ready: P, created: G, starting: G, degraded: A, failed: C, unavailable: C,
  },
  // compute: crates/compute-core/src/computers.rs `ReadinessState` (process)
  'compute.process.readiness': { ready: P, starting: G, unready: A },
  // compute: crates/compute-core/src/computers.rs `ObservedProcess::status`, which may
  // also carry the machine's observed word when the machine is not running.
  'compute.process.state': {
    ready: P, running: P, pending: G, starting: G, stopping: G, unready: A, failed: C, lost: C,
    stopped: N, exited: N, destroyed: N, expired: N, unverified: U, unreachable: U,
  },
  // compute: crates/compute-state/src/model.rs `DeploymentStatus`
  'compute.deployment.status': {
    active: P, complete: P, pending: G, starting: G, ready: G, network_ready: G, switching: G, draining: G,
    failed: C, rolled_back: C,
  },
  // compute: crates/compute-state/src/model.rs `VersionStatus`
  'compute.version.status': { published: P, publishing: G, failed: C },
  // compute: crates/compute-state/src/model.rs `RolloutStatus`
  'compute.rollout.status': { active: P, applying: G, failed: C, superseded: N },
  // compute: crates/compute-core/src/jobs.rs `JobStatus`. `timed_out` is a definite
  // terminal failure (the job ran past its deadline and was stopped), not an
  // unknown outcome.
  'compute.job.status': {
    succeeded: P, created: G, accepted: G, queued: G, waiting_for_capacity: G, reserved: G, admitted: G,
    preparing: G, running: G, failed: C, rejected: C, timed_out: C, cancelled: N,
  },
  // compute: packages/compute-appport/src/computers.ts `ComputerReality.observed`
  'compute.computer.observed': {
    running: P, starting: G, reconciling: G, stopping: G, failed: C, lost: C,
    stopped: N, destroyed: N, expired: N, unverified: U, unreachable: U,
  },
  // compute: crates/compute-core/src/computers.rs `ComputerStatus` (stored)
  'compute.computer.status': {
    running: P, pending: G, provisioning: G, resuming: G, stopping: G, destroying: G,
    failed: C, lost: C, stopped: N, destroyed: N, expired: N, unreachable: U,
  },
  // flow_db: crates/feltdb-server/src/main.rs `/health/ready` `state`
  'feltdb.health.ready_state': { healthy: P, recovering: G, degraded: A, unavailable: C },
  // flow_db: packages/core/src/managed-recovery.ts `AuthorityAvailabilityState`
  'feltdb.authority.availability': { available: P, recovering: G, degraded: A, closed: N, unknown: U },
  // flow_db: /v1/control-plane/overview `authority.lease_clock_healthy` (boolean)
  'feltdb.authority.lease_clock_healthy': { true: [P, 'Healthy'], false: [A, 'Degraded'] },
};

/// Verification-axis mappings: field value → verification state.
const VERIFICATION = {
  // flow_db: crates/feltdb-server/src/control_plane.rs `revision_to_json` re-checks
  // a revision's content against its id.
  'feltdb.revision.integrity_verified': { true: 'verified', false: 'verification_failed' },
};

/// Freshness-axis mappings.
const FRESHNESS = {
  // compute: crates/compute-environment/src/api.rs `X-Compute-State`
  'compute.response.state': { live: 'live', cached: 'cached', stale: 'stale' },
  // flow_db: packages/core/src/runtime-snapshot.ts staleness
  'feltdb.snapshot.staleness': { current: 'live', stale: 'stale', unknown: 'unknown' },
};

const freezeAll = (table) => Object.freeze(Object.fromEntries(
  Object.entries(table).map(([field, values]) => [field, Object.freeze({ ...values })]),
));
const CONDITION_FIELDS = freezeAll(CONDITION);
const VERIFICATION_FIELDS = freezeAll(VERIFICATION);
const FRESHNESS_FIELDS = freezeAll(FRESHNESS);

const has = (object, key) => Object.prototype.hasOwnProperty.call(object, key);

/// Every field this release maps, by axis.
function fields() {
  return {
    condition: Object.keys(CONDITION_FIELDS),
    verification: Object.keys(VERIFICATION_FIELDS),
    freshness: Object.keys(FRESHNESS_FIELDS),
  };
}

/// The source values a field maps, in declaration order.
function valuesOf(field) {
  const table = CONDITION_FIELDS[field] || VERIFICATION_FIELDS[field] || FRESHNESS_FIELDS[field];
  if (!table) throw new Error(`unknown field: ${field}`);
  return Object.keys(table);
}

/// The condition of one source value. Unmapped values are `unknown` with the
/// raw value as their label, so nothing renders as healthy by omission.
function resolve(field, value, options) {
  const table = CONDITION_FIELDS[field];
  if (!table) throw new Error(`unknown field: ${field}`);
  const key = String(value);
  const entry = has(table, key) ? table[key] : null;
  const [tone, label] = entry === null ? [U, null] : (Array.isArray(entry) ? entry : [entry, null]);
  const text = (options && options.label) || label || (entry === null ? key : humanize(key));
  return { field, value: key, tone, label: text, glyph: GLYPHS[tone], mapped: entry !== null };
}

/// A verification state with its checks. `verified` requires at least one
/// listed check; without one the claim is `not_evaluated` (contract §6.1 rule 1).
function verification(state, checks) {
  const list = normalizeChecks(checks);
  let normalized = VERIFICATION_STATES.includes(state) ? state : 'not_evaluated';
  if (normalized === 'verified' && list.length === 0) normalized = 'not_evaluated';
  if (normalized === 'verified' && list.some((check) => check.status === 'failed')) normalized = 'verification_failed';
  return { state: normalized, checks: list };
}

/// A verification state from a mapped source field.
function verificationOf(field, value, checks) {
  const table = VERIFICATION_FIELDS[field];
  if (!table) throw new Error(`unknown field: ${field}`);
  const key = String(value);
  const state = has(table, key) ? table[key] : 'not_evaluated';
  const list = normalizeChecks(checks).map((check) => ({
    ...check,
    status: check.status || (state === 'verified' ? 'passed' : state === 'verification_failed' ? 'failed' : undefined),
  }));
  return verification(state, list);
}

/// A freshness state from a mapped source field; anything else is `unknown`.
function freshnessOf(field, value) {
  const table = FRESHNESS_FIELDS[field];
  if (!table) throw new Error(`unknown field: ${field}`);
  const key = String(value);
  return has(table, key) ? table[key] : 'unknown';
}

/// A capability status; anything undeclared is `unsupported` (FeltDB's rule).
function capability(status) {
  return CAPABILITY_STATES.includes(status) ? status : 'unsupported';
}

function normalizeChecks(checks) {
  if (!checks) return [];
  return checks.map((check) => (typeof check === 'string'
    ? { label: check }
    : { label: String(check.label), ...(check.status ? { status: check.status } : {}) }));
}

// ---- view.js
// Component structure (docs/design-system-contract.md §4–5) as plain trees:
//   { t: tag, a: attributes, c: children, on: { event: handler } }
// Children are trees, strings, numbers, null/false (skipped), arrays, or opaque
// slot content (a DOM Node or a React element) passed through untouched.
// dom.js and the React binding both render these trees, so the DOM a component
// produces is the same in every product.


const node = (t, a, c, on) => ({ t, a: a || {}, c: c === undefined ? [] : [].concat(c), ...(on ? { on } : {}) });
const isTree = (value) => Boolean(value && typeof value === 'object' && typeof value.t === 'string' && value.a && Array.isArray(value.c));

const cx = (...names) => names.filter(Boolean).join(' ');
const present = (value) => value !== null && value !== undefined && value !== '';

/// Extra attributes a product passes through (data-*, aria-*, id, title).
const passthrough = (attrs) => {
  const out = {};
  for (const [key, value] of Object.entries(attrs || {})) {
    if (/^(data-|aria-)/.test(key) || key === 'id' || key === 'title') out[key] = value;
  }
  return out;
};

/// Deterministic timestamp text, identical in every renderer and timezone.
function formatTime(value) {
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return String(value);
  return `${date.toISOString().slice(0, 19).replace('T', ' ')} UTC`;
}
/// A <time> element, or the raw text when the value is not a valid time: a
/// malformed timestamp from a product must never break the page around it.
const timeNode = (value, cls) => {
  const date = new Date(value);
  return Number.isNaN(date.getTime())
    ? node('span', { class: cls }, String(value))
    : node('time', { class: cls, datetime: date.toISOString() }, formatTime(value));
};

function StatusIndicator(props) {
  const { field, value, detail, className, attrs } = props;
  const resolved = field ? resolve(field, value, { label: props.label }) : null;
  const tone = resolved ? resolved.tone : (props.tone || 'unknown');
  const label = resolved ? resolved.label : (present(props.label) ? props.label : humanize(tone));
  const glyph = props.glyph || GLYPHS[tone] || GLYPHS.unknown;
  return node('span', {
    class: cx('stack-status', `stack-status--${tone}`, className),
    'data-tone': tone,
    ...(resolved ? { 'data-field': resolved.field, 'data-value': resolved.value } : {}),
    ...passthrough(attrs),
  }, [
    node('span', { class: 'stack-status__glyph', 'aria-hidden': 'true' }, glyph),
    node('span', { class: 'stack-status__label' }, label),
    present(detail) ? node('span', { class: 'stack-status__detail' }, detail) : null,
  ]);
}

const VERIFICATION_GLYPHS = { verified: '✓', verification_failed: '✕', not_verified: '!', not_evaluated: '–' };

function VerificationBadge(props) {
  const { state, checks } = verification(props.state, props.checks);
  const failed = checks.filter((check) => check.status === 'failed');
  let label;
  if (state === 'verified') label = checks.length === 1 ? `Verified · ${checks[0].label}` : `Verified · ${checks.length} checks`;
  else if (state === 'verification_failed') {
    const named = failed.length ? failed : checks;
    label = named.length === 1 ? `Verification failed · ${named[0].label}`
      : named.length ? `Verification failed · ${named.length} checks` : 'Verification failed';
  } else label = state === 'not_verified' ? 'Not verified' : 'Not evaluated';
  return node('div', {
    class: cx('stack-verification', `stack-verification--${state}`, props.className),
    'data-verification': state,
    ...passthrough(props.attrs),
  }, [
    node('span', { class: 'stack-verification__glyph', 'aria-hidden': 'true' }, VERIFICATION_GLYPHS[state]),
    node('span', { class: 'stack-verification__label' }, label),
    present(props.at) ? [' · ', timeNode(props.at, 'stack-verification__at')] : null,
    present(props.evidenceHref) ? node('a', { class: 'stack-verification__evidence', href: props.evidenceHref }, 'Evidence') : null,
    checks.length > 1 ? node('ul', { class: 'stack-verification__checks' }, checks.map((check) => node('li', {
      class: 'stack-verification__check',
      ...(check.status ? { 'data-check-status': check.status } : {}),
    }, check.status ? `${check.label} — ${check.status}` : check.label))) : null,
  ]);
}

const FRESHNESS_LABELS = { live: 'Live', cached: 'Cached', stale: 'Stale', unknown: 'Freshness unknown' };
const FRESHNESS_GLYPHS = { live: '●', cached: '◷', stale: '▲', unknown: '?' };

function FreshnessLabel(props) {
  const freshness = FRESHNESS_LABELS[props.freshness] ? props.freshness : 'unknown';
  const showAsOf = present(props.asOf) && freshness !== 'live';
  return node('span', {
    class: cx('stack-freshness', `stack-freshness--${freshness}`, props.className),
    'data-freshness': freshness,
    ...passthrough(props.attrs),
  }, [
    node('span', { class: 'stack-freshness__glyph', 'aria-hidden': 'true' }, FRESHNESS_GLYPHS[freshness]),
    node('span', { class: 'stack-freshness__label' }, [
      FRESHNESS_LABELS[freshness],
      showAsOf ? [' · as of ', timeNode(props.asOf, 'stack-freshness__as-of')] : null,
      present(props.source) ? ` · ${props.source}` : null,
    ]),
  ]);
}

function Unavailable(props) {
  const status = props.status === 'partial' ? 'partial' : 'unsupported';
  const title = present(props.title) ? props.title : (status === 'partial' ? 'Partly available' : 'Not available');
  return node('div', {
    class: cx('stack-unavailable', `stack-unavailable--${status}`, props.className),
    role: 'note',
    'data-capability-status': status,
    ...(present(props.capability) ? { 'data-capability': props.capability } : {}),
    ...passthrough(props.attrs),
  }, [
    node(props.headingLevel ? heading(props.headingLevel) : 'p', { class: 'stack-unavailable__title' }, title),
    present(props.reason) ? node('p', { class: 'stack-unavailable__reason' }, props.reason) : null,
    props.children || null,
  ]);
}

function Loading(props) {
  const what = present(props.what) ? props.what : '';
  const text = props.long ? `Still loading${what ? ` ${what}` : ''}…` : `Loading${what ? ` ${what}` : ''}…`;
  return node('div', {
    class: cx('stack-loading', props.long ? 'stack-loading--long' : null, props.className),
    role: 'status',
    'aria-live': 'polite',
    ...passthrough(props.attrs),
  }, [
    node('span', { class: 'stack-loading__indicator', 'aria-hidden': 'true' }),
    node('span', { class: 'stack-loading__label' }, text),
  ]);
}

const heading = (level) => `h${Math.min(6, Math.max(1, Number(level) || 2))}`;

function EmptyState(props) {
  const variant = props.variant === 'no-match' ? 'no-match' : 'empty';
  const title = present(props.title) ? props.title : (variant === 'no-match' ? 'Nothing matches' : 'Nothing here yet');
  return node('div', {
    class: cx('stack-empty', `stack-empty--${variant}`, props.className),
    ...passthrough(props.attrs),
  }, [
    node(heading(props.headingLevel), { class: 'stack-empty__title' }, title),
    present(props.body) ? node('p', { class: 'stack-empty__body' }, props.body) : null,
    props.action ? node('div', { class: 'stack-empty__action' }, props.action) : null,
  ]);
}

function ErrorPanel(props) {
  const meta = [
    present(props.code) ? ['Code ', node('code', {}, props.code)] : null,
    present(props.requestId) ? [present(props.code) ? ' · ' : '', 'Request ', node('code', {}, props.requestId)] : null,
  ].filter(Boolean);
  return node('div', {
    class: cx('stack-error', props.className),
    role: 'alert',
    ...passthrough(props.attrs),
  }, [
    node('p', { class: 'stack-error__title' }, present(props.title) ? props.title : 'Something went wrong'),
    present(props.message) ? node('p', { class: 'stack-error__message' }, props.message) : null,
    meta.length ? node('p', { class: 'stack-error__meta' }, meta) : null,
    props.onRetry || props.actions ? node('div', { class: 'stack-error__actions' }, [
      props.onRetry ? Button({ label: props.retryLabel || 'Retry', onClick: props.onRetry }) : null,
      props.actions || null,
    ]) : null,
  ]);
}

function MetadataList(props) {
  return node('dl', { class: cx('stack-meta', props.className), ...passthrough(props.attrs) },
    (props.items || []).map((item) => {
      const missing = !present(item.value) && item.value !== 0 && item.value !== false;
      return node('div', { class: 'stack-meta__item', ...passthrough(item.attrs) }, [
        node('dt', { class: 'stack-meta__label' }, item.label),
        node('dd', {
          class: cx('stack-meta__value', item.mono ? 'stack-meta__value--mono' : null),
          ...(missing ? { 'data-missing': 'true' } : {}),
        }, missing ? '—' : (typeof item.value === 'boolean' ? String(item.value) : item.value)),
        present(item.hint) ? node('dd', { class: 'stack-meta__hint' }, item.hint) : null,
      ]);
    }));
}

function ResourceIdentity(props) {
  const length = Number(props.length) || 12;
  const id = present(props.id) ? String(props.id) : null;
  const truncated = id && id.length > length ? `${id.slice(0, length)}…` : id;
  const name = present(props.name)
    ? node(props.href ? 'a' : 'span', { class: 'stack-identity__name', ...(props.href ? { href: props.href } : {}) }, props.name)
    : null;
  return node('span', { class: cx('stack-identity', props.className), ...passthrough(props.attrs) }, [
    present(props.kind) ? node('span', { class: 'stack-identity__kind' }, props.kind) : null,
    name,
    id ? node('code', { class: 'stack-identity__id', title: id }, truncated === id ? id : [
      node('span', { 'aria-hidden': 'true' }, truncated),
      node('span', { class: 'stack-visually-hidden' }, id),
    ]) : null,
  ]);
}

function PageHeader(props) {
  return node('header', { class: cx('stack-page-header', props.className), ...passthrough(props.attrs) }, [
    present(props.eyebrow) ? node('p', { class: 'stack-page-header__eyebrow' }, props.eyebrow) : null,
    node('div', { class: 'stack-page-header__title-row' }, [
      node(heading(props.headingLevel || 1), { class: 'stack-page-header__title' }, props.title),
      props.status ? node('span', { class: 'stack-page-header__status' }, props.status) : null,
      props.actions ? node('div', { class: 'stack-page-header__actions' }, props.actions) : null,
    ]),
    present(props.description) ? node('p', { class: 'stack-page-header__description' }, props.description) : null,
    props.meta ? node('div', { class: 'stack-page-header__meta' }, props.meta) : null,
  ]);
}

const VARIANTS = ['primary', 'secondary', 'danger', 'ghost'];
const SIZES = ['sm', 'md', 'lg'];

function Button(props) {
  const variant = VARIANTS.includes(props.variant) ? props.variant : 'secondary';
  const size = SIZES.includes(props.size) ? props.size : 'md';
  const inert = Boolean(props.disabled || props.busy);
  return node('button', {
    class: cx('stack-button', `stack-button--${variant}`, `stack-button--${size}`, props.className),
    type: props.type === 'submit' ? 'submit' : 'button',
    ...(inert ? { disabled: true } : {}),
    ...(props.busy ? { 'aria-busy': 'true' } : {}),
    ...passthrough(props.attrs),
  }, [
    props.busy ? node('span', { class: 'stack-button__busy', 'aria-hidden': 'true' }) : null,
    node('span', { class: 'stack-button__label' }, props.label),
  ], props.onClick && !inert ? { click: props.onClick } : undefined);
}

const COMPONENTS = Object.freeze({
  StatusIndicator, VerificationBadge, FreshnessLabel, Unavailable, Loading, EmptyState,
  ErrorPanel, MetadataList, ResourceIdentity, PageHeader, Button,
});

// ---- dom.js
// Framework-free rendering of view trees into DOM nodes. Builds nodes with
// createElement / createTextNode / setAttribute only: it never parses markup
// or evaluates code. The built bundle must not even name those APIs, because
// Compute scans its served script for them literally (scripts/check-boundary.mjs).


/// Render a view tree (or string, number, array, Node) to a DOM Node.
function toDom(value, doc) {
  const d = doc || document;
  if (value === null || value === undefined || value === false || value === true) return d.createDocumentFragment();
  if (typeof value === 'string' || typeof value === 'number') return d.createTextNode(String(value));
  if (Array.isArray(value)) {
    const fragment = d.createDocumentFragment();
    for (const item of value) fragment.appendChild(toDom(item, d));
    return fragment;
  }
  if (isTree(value)) {
    const element = d.createElement(value.t);
    for (const [key, attr] of Object.entries(value.a)) {
      if (attr === null || attr === undefined || attr === false) continue;
      element.setAttribute(key, attr === true ? '' : String(attr));
    }
    for (const [event, handler] of Object.entries(value.on || {})) element.addEventListener(event, handler);
    for (const child of value.c) element.appendChild(toDom(child, d));
    return element;
  }
  if (value && typeof value.nodeType === 'number') return value;
  throw new TypeError('toDom: unsupported child');
}

/// Each component as a function returning a DOM element, e.g.
///   StackUI.dom.StatusIndicator({ field: 'compute.health', value: 'healthy' })
const dom = Object.freeze(Object.fromEntries(
  Object.entries(COMPONENTS).map(([name, component]) => [name, (props, doc) => toDom(component(props || {}), doc)]),
));

global.StackUI = Object.freeze({ version: "0.1.0", TONES, GLYPHS, OUTCOMES, OUTCOME_LABELS, VERIFICATION_STATES, FRESHNESS_STATES, CAPABILITY_STATES, humanize, fields, valuesOf, resolve, verification, verificationOf, freshnessOf, capability, isTree, formatTime, StatusIndicator, VerificationBadge, FreshnessLabel, Unavailable, Loading, EmptyState, ErrorPanel, MetadataList, ResourceIdentity, PageHeader, Button, COMPONENTS, toDom, dom });
})(typeof window !== 'undefined' ? window : globalThis);

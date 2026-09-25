// Compute control plane UI.
//
// The UI is a client of the Compute API and nothing else: every operation
// below is a call to `api(METHOD, PATH)` with a path from the API's route
// table (the UI/API parity test enforces this). It keeps no state of its
// own beyond what is on screen; live changes arrive as lifecycle events.
'use strict';

const view = document.getElementById('view');
const dialog = document.getElementById('dialog');
const toastBox = document.getElementById('toast');
const enc = encodeURIComponent;

// ---- API ------------------------------------------------------------------

function token() {
  try { return sessionStorage.getItem('compute.token') || ''; } catch { return ''; }
}

async function api(method, path, body) {
  const headers = { 'Content-Type': 'application/json' };
  if (token()) headers.Authorization = `Bearer ${token()}`;
  const response = await fetch(path, {
    method,
    headers,
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await response.text();
  const value = text ? JSON.parse(text) : null;
  if (!response.ok) {
    const error = new Error((value && value.message) || `HTTP ${response.status}`);
    error.kind = value && value.kind;
    throw error;
  }
  return value;
}

// ---- Rendering helpers ------------------------------------------------------

function h(tag, attributes, ...children) {
  const element = document.createElement(tag);
  for (const [name, value] of Object.entries(attributes || {})) {
    if (value === undefined || value === null || value === false) continue;
    if (name.startsWith('on')) element.addEventListener(name.slice(2), value);
    else if (name === 'class') element.className = value;
    else element.setAttribute(name, value === true ? '' : value);
  }
  for (const child of children.flat(Infinity)) {
    if (child === null || child === undefined || child === false) continue;
    element.append(child instanceof Node ? child : document.createTextNode(String(child)));
  }
  return element;
}

// ● running/healthy · ◐ starting/deploying · ○ stopped · × failed
const STATES = {
  running: ['ok', '●', 'Running'],
  healthy: ['ok', '●', 'Healthy'],
  completed: ['ok', '●', 'Completed'],
  complete: ['ok', '●', 'Complete'],
  active: ['ok', '●', 'Active'],
  serving: ['ok', '●', 'Serving'],
  valid: ['ok', '●', 'Valid'],
  starting: ['warn', '◐', 'Starting'],
  pending: ['warn', '◐', 'Pending'],
  ready: ['warn', '◐', 'Ready'],
  network_ready: ['warn', '◐', 'Network ready'],
  switching: ['warn', '◐', 'Switching'],
  draining: ['warn', '◐', 'Draining'],
  issuing: ['warn', '◐', 'Issuing'],
  renewing: ['warn', '◐', 'Renewing'],
  due: ['warn', '◐', 'Due'],
  deploying: ['warn', '◐', 'Deploying'],
  stopping: ['warn', '◐', 'Stopping'],
  degraded: ['warn', '◐', 'Degraded'],
  unknown: ['idle', '○', 'Unknown'],
  stopped: ['idle', '○', 'Stopped'],
  disabled: ['idle', '○', 'Disabled'],
  unmanaged: ['idle', '○', 'Unmanaged'],
  not_due: ['idle', '○', 'Not due'],
  unhealthy: ['bad', '×', 'Unhealthy'],
  failed: ['bad', '×', 'Failed'],
  rolled_back: ['bad', '↺', 'Rolled back'],
  expired: ['bad', '×', 'Expired'],
  denied: ['bad', '×', 'Denied'],
};

/// The release lifecycle, in order.
const RELEASE = ['pending', 'starting', 'ready', 'network_ready', 'switching', 'active', 'draining', 'complete'];

function state(value, text) {
  const [tone, glyph, label] = STATES[value] || ['idle', '○', value];
  return h('span', { class: `state ${tone}` }, h('span', { class: 'glyph', 'aria-hidden': 'true' }, glyph), text || label);
}

/// The single status of a project or environment: actual state, unless it
/// runs, in which case its health.
function status(item) {
  if (item.deployment && RELEASE.slice(0, -1).includes(item.deployment.status)) {
    return state('deploying', `Releasing · ${STATES[item.deployment.status][2]}`);
  }
  if (item.actual_state === 'running') return state(item.health === 'unknown' ? 'running' : item.health);
  return state(item.actual_state);
}

function ago(timestamp) {
  if (!timestamp) return '—';
  const seconds = Math.max(0, Math.round((Date.now() - new Date(timestamp)) / 1000));
  if (seconds < 60) return `${seconds}s ago`;
  if (seconds < 3600) return `${Math.round(seconds / 60)} minutes ago`;
  if (seconds < 86400) return `${Math.round(seconds / 3600)} hours ago`;
  return `${Math.round(seconds / 86400)} days ago`;
}

function short(id) {
  return id ? String(id).replace(/^sha256:/, '').slice(0, 12) : '—';
}

function bytes(value) {
  if (!value) return '—';
  const units = ['B', 'KB', 'MB', 'GB'];
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) { value /= 1024; unit += 1; }
  return `${value.toFixed(unit ? 1 : 0)} ${units[unit]}`;
}

function table(headers, rows, empty) {
  if (!rows.length) return h('div', { class: 'panel empty' }, empty || 'Nothing here yet.');
  return h('div', { class: 'panel' }, h('table', {},
    h('thead', {}, h('tr', {}, headers.map((header) => h('th', {}, header)))),
    h('tbody', {}, rows)));
}

function fact(label, value, mono) {
  return h('div', { class: 'panel fact' }, h('div', { class: 'label' }, label), h('div', { class: `value${mono ? ' mono' : ''}` }, value));
}

function toast(message, bad) {
  toastBox.textContent = message;
  toastBox.className = `show${bad ? ' bad' : ''}`;
  clearTimeout(toast.timer);
  toast.timer = setTimeout(() => { toastBox.className = ''; }, bad ? 6000 : 3000);
}

// ---- Dialogs ------------------------------------------------------------------

function modal(title, body, actions) {
  dialog.replaceChildren(
    h('div', { class: 'body' }, h('h3', {}, title), body),
    h('div', { class: 'footer' }, actions),
  );
  if (!dialog.open) dialog.showModal();
}

function close() { if (dialog.open) dialog.close(); }

/// Confirm an operation, stating exactly what it affects and what it does
/// not. Resolves true when confirmed.
function confirmImpact(title, affects, spares, verb) {
  return new Promise((resolve) => {
    const body = h('div', {},
      affects.length ? [h('div', {}, 'This will affect:'), h('ul', {}, affects.map((item) => h('li', {}, item)))] : null,
      spares.length ? [h('div', {}, 'It will not affect:'), h('ul', {}, spares.map((item) => h('li', {}, item)))] : null);
    const done = (value) => { close(); resolve(value); };
    dialog.onclose = () => resolve(false);
    modal(title, body, [
      h('button', { onclick: () => done(false) }, 'Cancel'),
      h('button', { class: verb === 'danger' ? 'primary danger' : 'primary', onclick: () => done(true), 'data-confirm': 'true' }, 'Confirm'),
    ]);
  });
}

async function act(label, operation) {
  try {
    const result = await operation();
    toast(label);
    await render();
    return result;
  } catch (error) {
    toast(`${label} failed: ${error.message}`, true);
    await render();
    return null;
  }
}

// ---- Lifecycle operations, spelled out ---------------------------------------------

const ENVIRONMENT = {
  start: (environment) => api('POST', `/environments/${enc(environment)}/start`),
  stop: (environment) => api('POST', `/environments/${enc(environment)}/stop`),
  restart: (environment) => api('POST', `/environments/${enc(environment)}/restart`),
};

const PROJECT = {
  start: (environment, project) => api('POST', `/environments/${enc(environment)}/projects/${enc(project)}/start`),
  stop: (environment, project) => api('POST', `/environments/${enc(environment)}/projects/${enc(project)}/stop`),
  restart: (environment, project) => api('POST', `/environments/${enc(environment)}/projects/${enc(project)}/restart`),
};

const WORKLOAD = {
  start: (environment, project, workload) => api('POST', `/environments/${enc(environment)}/projects/${enc(project)}/workloads/${enc(workload)}/start`),
  stop: (environment, project, workload) => api('POST', `/environments/${enc(environment)}/projects/${enc(project)}/workloads/${enc(workload)}/stop`),
  restart: (environment, project, workload) => api('POST', `/environments/${enc(environment)}/projects/${enc(project)}/workloads/${enc(workload)}/restart`),
  run: (environment, project, workload) => api('POST', `/environments/${enc(environment)}/projects/${enc(project)}/workloads/${enc(workload)}/run`),
};

// ---- Impact of lifecycle operations ------------------------------------------------

async function environmentImpact(environment) {
  const all = await api('GET', '/environments');
  const target = await api('GET', `/environments/${enc(environment)}`);
  return {
    affects: target.projects.flatMap((project) => project.workloads
      .filter((workload) => workload.kind === 'service')
      .map((workload) => `${project.name} / ${workload.name}`)),
    spares: [...all.filter((item) => item.name !== environment).map((item) => `Environment ${item.name}`), 'Compute daemon'],
  };
}

async function projectImpact(environment, project) {
  const detail = await api('GET', `/projects/${enc(project)}`);
  const current = await api('GET', `/environments/${enc(environment)}/projects/${enc(project)}`);
  const siblings = (await api('GET', `/environments/${enc(environment)}/projects`)).filter((item) => item.name !== project);
  return {
    affects: current.workloads.filter((workload) => workload.kind === 'service')
      .map((workload) => `${project} / ${environment} · ${workload.name}`),
    spares: [
      ...detail.environments.filter((item) => item.environment !== environment).map((item) => `${project} / ${item.environment}`),
      ...siblings.map((item) => `${item.name} / ${environment}`),
      'Compute daemon',
    ],
  };
}

async function environmentLifecycle(environment, action) {
  if (action !== 'start') {
    const impact = await environmentImpact(environment);
    const verb = action === 'stop' ? 'Stop' : 'Restart';
    if (!(await confirmImpact(`${verb} ${environment}?`, impact.affects, impact.spares, action === 'stop' ? 'danger' : ''))) return;
  }
  await act(`${environment}: ${action}`, () => ENVIRONMENT[action](environment));
}

async function projectLifecycle(environment, project, action) {
  if (action !== 'start') {
    const impact = await projectImpact(environment, project);
    const verb = action === 'stop' ? 'Stop' : 'Restart';
    if (!(await confirmImpact(`${verb} ${project} / ${environment}?`, impact.affects, impact.spares, action === 'stop' ? 'danger' : ''))) return;
  }
  await act(`${project} / ${environment}: ${action}`, () => PROJECT[action](environment, project));
}

async function workloadLifecycle(environment, project, workload, action) {
  if (action === 'run') {
    const result = await act(`${workload}: run`, () => WORKLOAD.run(environment, project, workload));
    if (result) showOutput(`${workload} — ${result.status}`, result.stdout, result.stderr);
    return;
  }
  if (action !== 'start') {
    const verb = action === 'stop' ? 'Stop' : 'Restart';
    const siblings = (await api('GET', `/environments/${enc(environment)}/projects/${enc(project)}`)).workloads
      .filter((item) => item.name !== workload).map((item) => `${project} / ${environment} · ${item.name}`);
    if (!(await confirmImpact(`${verb} ${workload}?`, [`${project} / ${environment} · ${workload}`], [...siblings, 'Compute daemon'], action === 'stop' ? 'danger' : ''))) return;
  }
  await act(`${workload}: ${action}`, () => WORKLOAD[action](environment, project, workload));
}

async function removeProject(environment, project) {
  const impact = await projectImpact(environment, project);
  if (!(await confirmImpact(`Remove ${project} from ${environment}?`, impact.affects, impact.spares, 'danger'))) return;
  await act(`${project} removed from ${environment}`, () => api('DELETE', `/environments/${enc(environment)}/projects/${enc(project)}`));
}

function showOutput(title, stdout, stderr) {
  modal(title, h('div', {},
    h('label', {}, 'stdout'), h('pre', { class: 'log' }, stdout || '(empty)'),
    h('label', {}, 'stderr'), h('pre', { class: 'log' }, stderr || '(empty)')),
  [h('button', { onclick: close }, 'Close')]);
}

// ---- Deploy / promote / assign --------------------------------------------------------

/// Add to environment → select environment → select revision → review → apply.
async function deployWizard({ project, environment, revision }) {
  const projects = project ? null : await api('GET', '/projects');
  const environments = await api('GET', '/environments');
  const state = { project, environment, revision };
  const steps = ['Project', 'Environment', 'Revision', 'Review'];
  const stepOf = () => (!state.project ? 0 : !state.environment ? 1 : !state.revision ? 2 : 3);
  const header = () => h('div', { class: 'steps' }, steps.map((step, index) =>
    h('span', { class: index === stepOf() ? 'active' : '' }, `${index + 1}. ${step}${index < 3 ? ' →' : ''}`)));

  const next = async () => {
    const step = stepOf();
    if (step === 0) {
      const select = h('select', { id: 'wizard-project' }, projects.map((item) => h('option', { value: item.name }, item.name)));
      modal('Deploy', h('div', {}, header(), h('label', { for: 'wizard-project' }, 'Project'), select), [
        h('button', { onclick: close }, 'Cancel'),
        h('button', { class: 'primary', onclick: () => { state.project = select.value; next(); } }, 'Next'),
      ]);
    } else if (step === 1) {
      const select = h('select', { id: 'wizard-environment' }, environments.map((item) => h('option', { value: item.name }, item.name)));
      modal(`Deploy ${state.project}`, h('div', {}, header(), h('label', { for: 'wizard-environment' }, 'Environment'), select), [
        h('button', { onclick: close }, 'Cancel'),
        h('button', { class: 'primary', onclick: () => { state.environment = select.value; next(); } }, 'Next'),
      ]);
    } else if (step === 2) {
      const revisions = await api('GET', `/projects/${enc(state.project)}/revisions`);
      if (!revisions.length) {
        modal(`Deploy ${state.project}`, h('div', {}, header(), h('p', {}, 'This project has no registered revisions. Register one with `compute project push`.')), [h('button', { onclick: close }, 'Close')]);
        return;
      }
      const select = h('select', { id: 'wizard-revision' }, revisions.map((item) =>
        h('option', { value: item.revision_id }, `${item.revision} · ${short(item.revision_digest)} · ${ago(item.created_at)}`)));
      modal(`Deploy ${state.project} to ${state.environment}`, h('div', {}, header(), h('label', { for: 'wizard-revision' }, 'Revision'), select), [
        h('button', { onclick: close }, 'Cancel'),
        h('button', { class: 'primary', onclick: () => { state.revision = select.value; state.label = select.selectedOptions[0].textContent; next(); } }, 'Next'),
      ]);
    } else {
      const revisions = await api('GET', `/projects/${enc(state.project)}/revisions`);
      const chosen = revisions.find((item) => item.revision_id === state.revision || item.revision === state.revision) || {};
      modal(`Deploy ${state.project} to ${state.environment}`, h('div', {}, header(),
        h('ul', {},
          h('li', {}, `Project: ${state.project}`),
          h('li', {}, `Environment: ${state.environment}`),
          h('li', {}, `Revision: ${chosen.revision || state.revision} (${short(chosen.revision_digest)})`),
          h('li', {}, `Workloads: ${(chosen.workloads || []).map((item) => `${item.name} (${item.kind})`).join(', ') || '—'}`)),
        h('p', { class: 'subtitle' }, 'The new revision starts next to what serves now. Traffic moves only once it is ready, and what it replaces drains before it stops. If anything fails first, the current revision keeps serving.')), [
        h('button', { onclick: close }, 'Cancel'),
        h('button', { class: 'primary', 'data-apply': 'true', onclick: async () => {
          close();
          const deployment = await act(`Deploying ${state.project} to ${state.environment}`, () => api('POST', '/deployments', {
            project: state.project, environment: state.environment, revision: chosen.revision_id || state.revision,
          }));
          if (deployment && deployment.status === 'failed') toast(`Deployment failed: ${deployment.failure}`, true);
          if (deployment) location.hash = `#/deployments/${enc(deployment.deployment_id)}`;
        } }, 'Apply'),
      ]);
    }
  };
  await next();
}

async function promoteDialog(project, from) {
  const detail = await api('GET', `/projects/${enc(project)}`);
  const source = detail.environments.find((item) => item.environment === from);
  const targets = (await api('GET', '/environments')).filter((item) => item.name !== from);
  if (!targets.length) { toast('There is no other environment to promote to.', true); return; }
  const select = h('select', { id: 'promote-target' }, targets.map((item) => h('option', { value: item.name }, item.name)));
  modal(`Promote ${project}`, h('div', {},
    h('p', {}, `Deploy the exact revision current in ${from} — ${source ? source.revision : '?'} — to another environment. Nothing is rebuilt; configuration stays per environment.`),
    h('label', { for: 'promote-target' }, 'To'), select), [
    h('button', { onclick: close }, 'Cancel'),
    h('button', { class: 'primary', 'data-apply': 'true', onclick: async () => {
      close();
      const deployment = await act(`Promoting ${project} from ${from} to ${select.value}`, () => api('POST', '/deployments/promote', { project, from, to: select.value }));
      if (deployment && deployment.status === 'failed') toast(`Promotion failed: ${deployment.failure}`, true);
      if (deployment) location.hash = `#/deployments/${enc(deployment.deployment_id)}`;
    } }, 'Promote'),
  ]);
}

async function redeploy(environment, project) {
  const current = await api('GET', `/environments/${enc(environment)}/projects/${enc(project)}`);
  await deployWizard({ project, environment, revision: current.revision_id });
}

// ---- Views ------------------------------------------------------------------

async function environmentsView() {
  const environments = await api('GET', '/environments');
  return [
    h('div', { class: 'title' }, h('h1', {}, 'Environments'),
      h('div', { class: 'actions' },
        h('button', { onclick: () => deployWizard({}) }, 'Deploy'),
        h('button', { class: 'primary', onclick: createEnvironment }, 'New environment'))),
    h('div', { class: 'subtitle' }, 'What is running where.'),
    environments.length ? h('div', { class: 'cards' }, environments.map((environment) =>
      h('div', { class: 'panel card', role: 'link', tabindex: '0', 'data-environment': environment.name,
        onclick: () => { location.hash = `#/environments/${enc(environment.name)}`; },
        onkeydown: (event) => { if (event.key === 'Enter') location.hash = `#/environments/${enc(environment.name)}`; } },
      h('div', { class: 'name' }, environment.name),
      h('div', { class: 'meta' }, `${environment.project_count} projects · ${environment.workload_count} workloads · ${environment.provider}`),
      status(environment))))
      : h('div', { class: 'panel empty' }, 'No environments yet. Create one, such as preprod or production.'),
  ];
}

function createEnvironment() {
  const name = h('input', { id: 'environment-name', placeholder: 'production', autocomplete: 'off' });
  modal('New environment', h('div', {}, h('label', { for: 'environment-name' }, 'Name'), name), [
    h('button', { onclick: close }, 'Cancel'),
    h('button', { class: 'primary', onclick: async () => {
      close();
      await act(`Environment ${name.value} created`, () => api('POST', '/environments', { name: name.value }));
    } }, 'Create'),
  ]);
  name.focus();
}

async function environmentView(name) {
  const environment = await api('GET', `/environments/${enc(name)}`);
  const rows = environment.projects.map((project) => h('tr', {
    class: 'link', 'data-project': project.name,
    onclick: () => { location.hash = `#/environments/${enc(name)}/projects/${enc(project.name)}`; },
  },
  h('td', {}, h('strong', {}, project.name)),
  h('td', { class: 'mono' }, project.revision || '—'),
  h('td', {}, project.desired_state),
  h('td', {}, status(project)),
  h('td', {}, `${project.workload_count} workloads · ${project.service_count} services`),
  h('td', {}, project.provider),
  h('td', {}, project.deployment ? [state(project.deployment.status), ' ', h('span', { class: 'chip' }, ago(project.deployment.created_at))] : '—')));
  return [
    h('div', { class: 'crumbs' }, h('a', { href: '#/' }, 'Environments'), ' / ', name),
    h('div', { class: 'title' }, h('h1', {}, name.toUpperCase()), status(environment),
      h('div', { class: 'actions' },
        h('button', { onclick: () => deployWizard({ environment: name }) }, 'Add project'),
        h('button', { onclick: () => environmentLifecycle(name, 'start') }, 'Start'),
        h('button', { onclick: () => environmentLifecycle(name, 'restart') }, 'Restart'),
        h('button', { class: 'danger', onclick: () => environmentLifecycle(name, 'stop') }, 'Stop'))),
    h('div', { class: 'subtitle' }, `${environment.project_count} projects · ${environment.workload_count} workloads · desired ${environment.desired_state} · policy ${short(environment.policy_id)}`),
    h('h2', {}, 'Projects'),
    table(['Project', 'Revision', 'Desired', 'Status', 'Workloads', 'Provider', 'Last deployment'], rows, 'No projects in this environment. Add one.'),
  ];
}

const TABS = ['Overview', 'Workloads', 'Deployments', 'Logs', 'Resources', 'Configuration', 'Receipts', 'Events'];

async function projectView(environment, name, tab) {
  const project = await api('GET', `/environments/${enc(environment)}/projects/${enc(name)}`);
  const base = `#/environments/${enc(environment)}/projects/${enc(name)}`;
  const active = TABS.includes(tab) ? tab : 'Overview';
  return [
    h('div', { class: 'crumbs' }, h('a', { href: '#/' }, 'Environments'), ' / ', h('a', { href: `#/environments/${enc(environment)}` }, environment), ' / ', name),
    h('div', { class: 'title' }, h('h1', {}, `${name.toUpperCase()} / ${environment.toUpperCase()}`), status(project),
      h('div', { class: 'actions' },
        h('button', { onclick: () => redeploy(environment, name) }, 'Deploy'),
        h('button', { onclick: () => promoteDialog(name, environment) }, 'Promote'),
        h('button', { onclick: () => projectLifecycle(environment, name, 'start') }, 'Start'),
        h('button', { onclick: () => projectLifecycle(environment, name, 'restart') }, 'Restart'),
        h('button', { class: 'danger', onclick: () => projectLifecycle(environment, name, 'stop') }, 'Stop'),
        h('button', { class: 'danger', onclick: () => removeProject(environment, name) }, 'Remove'))),
    h('div', { class: 'tabs', role: 'tablist' }, TABS.map((item) => h('button', {
      role: 'tab', class: item === active ? 'active' : '', 'aria-selected': String(item === active),
      onclick: () => { location.hash = `${base}/${item.toLowerCase()}`; },
    }, item))),
    await projectTab(environment, name, project, active),
  ];
}

async function projectTab(environment, name, project, tab) {
  const services = project.workloads.filter((workload) => workload.kind === 'service');
  switch (tab) {
    case 'Overview': {
      const memory = project.workloads.map((workload) => workload.resources.memory_limit_bytes || 0).reduce((a, b) => a + b, 0);
      const ports = project.workloads.flatMap((workload) => workload.ports.map((port) => `${workload.name} ${port.name}: ${port.logical} → ${port.host}`));
      const evidence = project.workloads.find((workload) => workload.evidence.admission_id) || { evidence: {} };
      return h('div', {},
        h('div', { class: 'grid' },
          fact('Revision', project.revision || '—', true),
          fact('Desired state', project.desired_state),
          fact('Actual state', status(project)),
          fact('Latest deployment', project.deployment ? [project.deployment.revision, ' · ', state(project.deployment.status), ' · ', ago(project.deployment.created_at)] : '—'),
          fact('Resources', `Memory ${memory ? bytes(memory) : 'no limit'} · CPU not measured`),
          fact('Network', ports.length ? ports.join('\n') : 'No ports'),
          fact('Admission', [h('div', {}, `policy ${short(evidence.evidence.policy_id)}`), h('div', {}, `admission ${short(evidence.evidence.admission_id)}`)], true),
          fact('Revision digest', short(project.revision_digest), true)),
        h('h2', {}, 'Services'),
        table(['Service', 'Status', 'Ports'], services.map((workload) => h('tr', {},
          h('td', {}, workload.name), h('td', {}, workload.actual_state === 'running' ? state(workload.health) : state(workload.actual_state)),
          h('td', { class: 'mono' }, workload.ports.map((port) => `${port.logical}→${port.host}`).join(', ') || '—'))), 'No services.'));
    }
    case 'Workloads':
      return table(['Workload', 'Kind', 'Desired', 'Status', 'Restarts', 'Started', ''], project.workloads.map((workload) => h('tr', { 'data-workload': workload.name },
        h('td', {}, h('strong', {}, workload.name), h('div', { class: 'chip' }, workload.runtime)),
        h('td', {}, workload.kind),
        h('td', {}, workload.desired_state),
        h('td', {}, workload.actual_state === 'running' ? state(workload.health) : state(workload.actual_state), workload.error ? h('div', { class: 'error' }, workload.error) : null),
        h('td', {}, String(workload.restarts)),
        h('td', {}, ago(workload.started_at)),
        h('td', {}, workload.kind === 'task'
          ? h('button', { class: 'small', onclick: () => workloadLifecycle(environment, name, workload.name, 'run') }, 'Run')
          : [h('button', { class: 'small', onclick: () => workloadLifecycle(environment, name, workload.name, 'start') }, 'Start'), ' ',
            h('button', { class: 'small', onclick: () => workloadLifecycle(environment, name, workload.name, 'restart') }, 'Restart'), ' ',
            h('button', { class: 'small danger', onclick: () => workloadLifecycle(environment, name, workload.name, 'stop') }, 'Stop')]))));
    case 'Deployments': {
      const deployments = await api('GET', `/deployments?environment=${enc(environment)}&project=${enc(name)}&limit=50`);
      return table(['Deployment', 'Revision', 'Status', 'Admission', 'Created', 'Promoted from'], deployments.map((deployment) => h('tr', {
        class: 'link', 'data-deployment': deployment.deployment_id,
        onclick: () => { location.hash = `#/deployments/${enc(deployment.deployment_id)}`; },
      },
        h('td', { class: 'mono' }, deployment.deployment_id),
        h('td', { class: 'mono' }, deployment.revision),
        h('td', {}, state(deployment.status), (deployment.failure || deployment.rollback_reason) ? h('div', { class: 'error' }, deployment.failure || deployment.rollback_reason) : null),
        h('td', {}, deployment.workloads.map((workload) => h('div', { class: 'mono' }, `${workload.name}: ${workload.admitted ? 'admitted' : 'denied'} ${short(workload.admission_id)}`))),
        h('td', {}, ago(deployment.created_at)),
        h('td', { class: 'mono' }, deployment.promoted_from || '—'))), 'No deployments.');
    }
    case 'Logs': {
      const panes = [];
      for (const workload of project.workloads) {
        const logs = await api('GET', `/environments/${enc(environment)}/projects/${enc(name)}/workloads/${enc(workload.name)}/logs`);
        panes.push(h('h2', {}, workload.name), h('pre', { class: 'log' }, (logs.stdout || '') + (logs.stderr ? `\n${logs.stderr}` : '') || '(no output yet)'));
      }
      return h('div', {}, panes);
    }
    case 'Resources':
      return table(['Workload', 'Memory limit', 'Timeout', 'CPU', 'Disk (logs)', 'Network'], project.workloads.map((workload) => h('tr', {},
        h('td', {}, workload.name),
        h('td', {}, workload.resources.memory_limit_bytes ? bytes(workload.resources.memory_limit_bytes) : 'none'),
        h('td', {}, workload.resources.timeout_ms ? `${workload.resources.timeout_ms} ms` : 'none'),
        h('td', {}, 'not measured'),
        h('td', {}, bytes(workload.resources.disk_bytes)),
        h('td', {}, workload.resources.network))));
    case 'Configuration': {
      const rows = Object.entries(project.config).map(([key, value]) => h('tr', {}, h('td', { class: 'mono' }, key), h('td', { class: 'mono' }, value)));
      return h('div', {},
        h('p', { class: 'subtitle' }, `Configuration of ${name} in ${environment}. It layers over the environment's configuration; Compute adds COMPUTE_PORT_* and PORT. Compute is not a secrets manager.`),
        table(['Key', 'Value'], rows, 'No project configuration in this environment.'));
    }
    case 'Receipts': {
      const receipts = await api('GET', `/environments/${enc(environment)}/projects/${enc(name)}/receipts?limit=50`);
      return table(['Receipt', 'Execution', 'Admission', 'Created', ''], receipts.map((receipt) => h('tr', {},
        h('td', { class: 'mono' }, short(receipt.receipt_id)),
        h('td', { class: 'mono' }, receipt.execution_id),
        h('td', { class: 'mono' }, short(receipt.admission_id)),
        h('td', {}, ago(receipt.created_at)),
        h('td', {}, h('button', { class: 'small', onclick: async () => {
          const document = await api('GET', `/receipts/${enc(receipt.receipt_id)}`);
          modal(`Receipt ${short(receipt.receipt_id)}`, h('pre', { class: 'log' }, JSON.stringify(document, null, 2)), [h('button', { onclick: close }, 'Close')]);
        } }, 'View')))), 'No receipts yet.');
    }
    case 'Events': {
      const events = await api('GET', `/events?environment=${enc(environment)}&project=${enc(name)}&limit=100`);
      return eventTable(events.slice().reverse());
    }
    default:
      return h('div', {});
  }
}

function eventTable(events) {
  return table(['#', 'When', 'Event', 'Where', 'Message'], events.map((event) => h('tr', {},
    h('td', { class: 'mono' }, String(event.sequence)),
    h('td', {}, ago(event.at)),
    h('td', { class: 'mono' }, event.kind),
    h('td', {}, [event.project, event.environment].filter(Boolean).join(' / ') || '—'),
    h('td', {}, event.message))), 'No events.');
}

async function projectsView() {
  const projects = await api('GET', '/projects');
  return [
    h('div', { class: 'title' }, h('h1', {}, 'Projects'), h('div', { class: 'actions' }, h('button', { class: 'primary', onclick: () => deployWizard({}) }, 'Deploy'))),
    h('div', { class: 'subtitle' }, 'Software, and every environment it runs in.'),
    table(['Project', 'Revisions', 'Environments'], projects.map((project) => h('tr', {
      class: 'link', 'data-project': project.name, onclick: () => { location.hash = `#/projects/${enc(project.name)}`; },
    },
    h('td', {}, h('strong', {}, project.name)),
    h('td', {}, String(project.revision_count)),
    h('td', {}, project.environments.map((placement) => h('span', { class: 'chip' }, `${placement.environment} · ${placement.revision}`))))),
    'No projects yet. Register one with `compute project push` or `compute deploy --source`.'),
  ];
}

async function projectDetailView(name) {
  const detail = await api('GET', `/projects/${enc(name)}`);
  const environments = await api('GET', '/environments');
  const member = new Set(detail.environments.map((placement) => placement.environment));
  const checks = environments.map((environment) => h('label', { 'data-environment': environment.name },
    h('input', { type: 'checkbox', checked: member.has(environment.name), onchange: async (event) => {
      event.target.checked = member.has(environment.name);
      if (member.has(environment.name)) await removeProject(environment.name, name);
      else await deployWizard({ project: name, environment: environment.name });
    } }),
    h('span', {}, environment.name),
    member.has(environment.name) ? status(detail.environments.find((item) => item.environment === environment.name)) : h('span', { class: 'chip' }, 'not deployed')));
  return [
    h('div', { class: 'crumbs' }, h('a', { href: '#/projects' }, 'Projects'), ' / ', name),
    h('div', { class: 'title' }, h('h1', {}, name.toUpperCase()),
      h('div', { class: 'actions' }, h('button', { class: 'primary', onclick: () => deployWizard({ project: name }) }, 'Add to environment'))),
    h('div', { class: 'subtitle' }, `${detail.revision_count} revisions · latest ${detail.latest_revision || '—'}`),
    h('h2', {}, 'Environments'),
    h('div', { class: 'panel checks', style: 'padding: 4px 16px' }, checks),
    h('h2', {}, 'Revisions'),
    table(['Revision', 'Digest', 'Workloads', 'Registered'], detail.revisions.map((revision) => h('tr', {},
      h('td', { class: 'mono' }, revision.revision),
      h('td', { class: 'mono' }, short(revision.revision_digest)),
      h('td', {}, revision.workloads.map((workload) => `${workload.name} (${workload.kind})`).join(', ')),
      h('td', {}, ago(revision.created_at))))),
    h('h2', {}, 'Deployments'),
    table(['Deployment', 'Environment', 'Revision', 'Status', 'Created'], detail.deployments.map((deployment) => h('tr', {
      class: 'link', onclick: () => { location.hash = `#/deployments/${enc(deployment.deployment_id)}`; },
    },
      h('td', { class: 'mono' }, deployment.deployment_id),
      h('td', {}, deployment.environment),
      h('td', { class: 'mono' }, deployment.revision),
      h('td', {}, state(deployment.status)),
      h('td', {}, ago(deployment.created_at)))), 'No deployments.'),
  ];
}

async function servicesView() {
  const [services, providers] = await Promise.all([api('GET', '/services'), api('GET', '/providers')]);
  return [
    h('div', { class: 'title' }, h('h1', {}, 'Services')),
    h('div', { class: 'subtitle' }, 'Shared services projects consume, and the providers Compute runs on.'),
    table(['Service', 'Capabilities', 'Provider', 'Endpoint', ''], services.map((service) => h('tr', {},
      h('td', {}, h('strong', {}, service.name), service.description ? h('div', { class: 'subtitle' }, service.description) : null),
      h('td', {}, service.capabilities.map((capability) => h('span', { class: 'chip' }, capability))),
      h('td', {}, service.provider),
      h('td', { class: 'mono' }, service.endpoint || '—'),
      h('td', {}, h('button', { class: 'small danger', onclick: async () => {
        if (await confirmImpact(`Remove ${service.name}?`, [`The ${service.name} registration`], ['Every workload', 'Compute daemon'], 'danger')) {
          await act(`${service.name} removed`, () => api('DELETE', `/services/${enc(service.name)}`));
        }
      } }, 'Remove')))), 'No shared services registered.'),
    h('h2', {}, 'Providers'),
    table(['Provider', 'Kind', 'Priority', 'Endpoint', 'Registered'], providers.map((provider) => h('tr', {},
      h('td', {}, provider.provider_id), h('td', {}, provider.kind), h('td', {}, String(provider.priority)),
      h('td', { class: 'mono' }, provider.endpoint || '—'), h('td', {}, ago(provider.observed_at))))),
  ];
}

async function eventsView() {
  const events = await api('GET', '/events?limit=200');
  return [
    h('div', { class: 'title' }, h('h1', {}, 'Events')),
    h('div', { class: 'subtitle' }, 'Lifecycle events, newest first. They update live.'),
    eventTable(events.slice().reverse()),
  ];
}

// ---- Releases -------------------------------------------------------------------

function progress(current) {
  const failed = ['failed', 'rolled_back'].includes(current);
  const at = RELEASE.indexOf(current);
  return h('ol', { class: 'progress', 'aria-label': 'Release progress' }, RELEASE.map((step, index) => h('li', {
    class: failed ? '' : index < at || current === 'complete' ? 'done' : index === at ? 'current' : '',
    'aria-current': index === at ? 'step' : null,
  }, STATES[step][2])), failed ? h('li', { class: 'failed', 'aria-current': 'step' }, STATES[current][2]) : null);
}

async function rollbackRelease(deployment) {
  const moved = RELEASE.indexOf(deployment.status) >= RELEASE.indexOf('switching');
  const affects = deployment.status === 'complete'
    ? [`A new release of ${deployment.old_revision || 'the previous revision'} to ${deployment.environment}`]
    : moved ? [`Traffic of ${deployment.project} in ${deployment.environment} returns to ${deployment.old_revision || 'the previous revision'}`]
      : [`The release of ${deployment.revision} is abandoned; its instances stop`];
  if (await confirmImpact(`Roll back ${deployment.project} ${deployment.revision}?`, affects, ['Other projects', 'Other environments'], 'danger')) {
    const result = await act(`Rolling back ${deployment.revision}`, () => api('POST', `/deployments/${enc(deployment.deployment_id)}/rollback`));
    if (result && result.deployment_id !== deployment.deployment_id) location.hash = `#/deployments/${enc(result.deployment_id)}`;
  }
}

async function showReceipt(deploymentId) {
  const receipt = await api('GET', `/deployments/${enc(deploymentId)}/receipt`);
  modal('Deployment receipt', h('pre', { class: 'log' }, JSON.stringify(receipt, null, 2)), [h('button', { onclick: close }, 'Close')]);
}

async function deploymentView(id) {
  const deployment = await api('GET', `/deployments/${enc(id)}`);
  const events = await api('GET', `/events?deployment=${enc(id)}&limit=200`);
  const readiness = Object.entries(deployment.readiness_result || {});
  const network = deployment.network_result || {};
  const traffic = deployment.traffic_switch_result || {};
  const inFlight = RELEASE.slice(0, -1).includes(deployment.status);
  const reason = deployment.failure || deployment.rollback_reason;
  return [
    h('div', { class: 'crumbs' }, h('a', { href: '#/' }, 'Environments'), ' / ',
      h('a', { href: `#/environments/${enc(deployment.environment)}` }, deployment.environment), ' / ',
      h('a', { href: `#/environments/${enc(deployment.environment)}/projects/${enc(deployment.project)}/deployments` }, deployment.project), ' / ', short(id)),
    h('div', { class: 'title' }, h('h1', {}, `RELEASE ${deployment.revision}`), state(deployment.status),
      h('div', { class: 'actions' },
        deployment.receipt ? h('button', { onclick: () => showReceipt(id) }, 'Receipt') : null,
        ['failed', 'rolled_back'].includes(deployment.status) ? null
          : h('button', { class: 'danger', 'data-rollback': 'true', onclick: () => rollbackRelease(deployment) }, 'Roll back'))),
    h('div', { class: 'subtitle' }, `${deployment.project} → ${deployment.environment} · replaces ${deployment.old_revision || 'nothing'} · ${inFlight ? `${STATES[deployment.status][2].toLowerCase()} since ${ago(deployment.status_since)}` : `ended ${ago(deployment.completed_at || deployment.updated_at)}`}`),
    progress(deployment.status),
    reason ? h('div', { class: 'panel fact error', role: 'alert' }, reason) : null,
    h('div', { class: 'grid' },
      fact('Revision', `${deployment.revision} · ${short(deployment.revision_digest)}`, true),
      fact('Replaces', deployment.old_revision ? `${deployment.old_revision} · ${short(deployment.previous)}` : 'nothing', true),
      fact('Configuration', short(deployment.config_digest), true),
      fact('Promoted from', deployment.promoted_from ? short(deployment.promoted_from) : '—', true)),
    h('h2', {}, 'Instances'),
    table(['Instance', 'Workload', 'State', 'Process', 'Ports', 'Connections', 'Readiness'], deployment.instances.map((instance) => h('tr', { 'data-instance': instance.instance_id },
      h('td', { class: 'mono' }, short(instance.instance_id)),
      h('td', {}, instance.workload),
      h('td', {}, state(instance.state), instance.error ? h('div', { class: 'error' }, instance.error) : null),
      h('td', {}, instance.actual_state ? state(instance.actual_state) : '—'),
      h('td', { class: 'mono' }, instance.ports.map((port) => `${port.name} ${port.host}`).join(', ') || '—'),
      h('td', {}, String(instance.open_connections)),
      h('td', {}, instance.readiness || '—'))), inFlight ? 'Instances are created once the release is admitted.' : 'This release has no instances now.'),
    h('h2', {}, 'Readiness'),
    table(['Workload', 'Check', 'Result', 'Ready'], readiness.map(([workload, result]) => h('tr', {},
      h('td', {}, workload), h('td', {}, result.check || '—'), h('td', {}, result.detail || '—'), h('td', {}, ago(result.ready_at)))), 'Not verified yet.'),
    h('h2', {}, 'Network'),
    table(['Endpoint', 'Port', 'From', 'To', 'Verified'], (traffic.endpoints || (network.endpoints || []).map((endpoint) => ({ endpoint: endpoint.endpoint, host_port: endpoint.host_port }))).map((endpoint) => h('tr', {},
      h('td', { class: 'mono' }, endpoint.endpoint),
      h('td', { class: 'mono' }, String(endpoint.host_port)),
      h('td', { class: 'mono' }, endpoint.from_revision ? `${endpoint.from_revision} · ${short(endpoint.from_instance)}` : '—'),
      h('td', { class: 'mono' }, endpoint.to_instance ? short(endpoint.to_instance) : '—'),
      h('td', {}, ((traffic.verified || []).find((item) => item.endpoint === endpoint.endpoint) || {}).verified || '—'))), 'Traffic has not moved.'),
    (network.domains || []).length ? table(['Domain', 'DNS', 'TLS', 'Routing'], network.domains.map((domain) => h('tr', {},
      h('td', {}, h('a', { href: `#/domains/${enc(domain.domain)}` }, domain.domain)), h('td', {}, state(domain.dns)), h('td', {}, state(domain.tls)), h('td', {}, state(domain.routing))))) : null,
    h('h2', {}, 'Events'),
    eventTable(events.slice().reverse()),
  ];
}

// ---- Domains ---------------------------------------------------------------------

async function addDomain() {
  const environments = await api('GET', '/environments');
  if (!environments.length) { toast('Create an environment first.', true); return; }
  const name = h('input', { id: 'domain-name', placeholder: 'app.example.com', autocomplete: 'off' });
  const environment = h('select', { id: 'domain-environment' }, environments.map((item) => h('option', { value: item.name }, item.name)));
  const project = h('select', { id: 'domain-project' });
  const fill = async () => {
    const projects = await api('GET', `/environments/${enc(environment.value)}/projects`);
    project.replaceChildren(...projects.map((item) => h('option', { value: item.name }, item.name)));
  };
  environment.onchange = fill;
  await fill();
  modal('Add domain', h('div', {},
    h('p', { class: 'subtitle' }, 'A domain routes to one project in one environment. Compute keeps its DNS record and certificate.'),
    h('label', { for: 'domain-name' }, 'Domain'), name,
    h('label', { for: 'domain-environment' }, 'Environment'), environment,
    h('label', { for: 'domain-project' }, 'Project'), project), [
    h('button', { onclick: close }, 'Cancel'),
    h('button', { class: 'primary', 'data-apply': 'true', onclick: async () => {
      close();
      const created = await act(`${name.value} added`, () => api('POST', '/domains', { name: name.value, environment: environment.value, project: project.value }));
      if (created) location.hash = `#/domains/${enc(created.name)}`;
    } }, 'Add'),
  ]);
  name.focus();
}

async function domainsView() {
  const domains = await api('GET', '/domains');
  return [
    h('div', { class: 'title' }, h('h1', {}, 'Domains'),
      h('div', { class: 'actions' },
        h('button', { onclick: () => act('DNS reconciled', () => api('POST', '/dns/reconcile')) }, 'Reconcile DNS'),
        h('button', { class: 'primary', onclick: addDomain }, 'Add domain'))),
    h('div', { class: 'subtitle' }, 'Each domain routes to one project in one environment, and follows its releases.'),
    table(['Domain', 'Routes to', 'Status', 'DNS', 'TLS', 'Routing'], domains.map((domain) => h('tr', {
      class: 'link', 'data-domain': domain.name, onclick: () => { location.hash = `#/domains/${enc(domain.name)}`; },
    },
    h('td', {}, h('strong', {}, domain.name)),
    h('td', { class: 'mono' }, domain.endpoint),
    h('td', {}, state(domain.status)),
    h('td', {}, state(domain.dns.status)),
    h('td', {}, state(domain.tls.status)),
    h('td', {}, state(domain.routing.status)))), 'No domains yet.'),
  ];
}

function reconciliation(label, item) {
  return fact(label, [state(item.status), h('div', {}, `desired: ${item.desired || '—'}`), h('div', {}, `actual: ${item.actual || '—'}`),
    item.last_error ? h('div', { class: 'error' }, item.last_error) : null, h('div', { class: 'subtitle' }, `checked ${ago(item.last_reconciled_at)}`)]);
}

async function domainView(name) {
  const domain = await api('GET', `/domains/${enc(name)}`);
  const certificate = domain.certificate;
  return [
    h('div', { class: 'crumbs' }, h('a', { href: '#/domains' }, 'Domains'), ' / ', name),
    h('div', { class: 'title' }, h('h1', {}, name), state(domain.status),
      h('div', { class: 'actions' },
        h('button', { onclick: () => act('DNS reconciled', () => api('POST', '/dns/reconcile')) }, 'Reconcile DNS'),
        certificate ? h('button', { onclick: () => act(`Renewing the certificate for ${name}`, () => api('POST', `/certificates/${enc(name)}/renew`)) }, 'Renew certificate') : null,
        h('button', { class: 'danger', onclick: async () => {
          if (await confirmImpact(`Remove ${name}?`, [`Routing of ${name}`, 'Its DNS records at the provider', 'Its certificate'], [`${domain.project} in ${domain.environment}`, 'Other domains'], 'danger')) {
            const removed = await act(`${name} removed`, () => api('DELETE', `/domains/${enc(name)}`));
            if (removed) location.hash = '#/domains';
          }
        } }, 'Remove'))),
    h('div', { class: 'subtitle' }, [`Routes to `, h('a', { href: `#/environments/${enc(domain.environment)}/projects/${enc(domain.project)}` }, domain.endpoint),
      domain.serving_revision ? ` · serving ${domain.serving_revision} on port ${domain.host_port}` : ' · nothing serves it yet']),
    h('div', { class: 'grid' },
      reconciliation('Routing', domain.routing),
      reconciliation('DNS', domain.dns),
      reconciliation('TLS', domain.tls)),
    h('h2', {}, 'DNS records'),
    table(['Record', 'Provider', 'Desired', 'Actual', 'Status', 'Checked'], domain.dns_records.map((record) => h('tr', {},
      h('td', { class: 'mono' }, `${record.name} ${record.record_type} (${record.zone})`),
      h('td', {}, record.provider),
      h('td', { class: 'mono' }, record.value),
      h('td', { class: 'mono' }, record.state.actual || '—'),
      h('td', {}, state(record.state.status), record.state.last_error ? h('div', { class: 'error' }, record.state.last_error) : null),
      h('td', {}, ago(record.state.last_reconciled_at)))), domain.dns_provider === 'none' ? 'DNS is managed outside Compute.' : 'No records yet.'),
    h('h2', {}, 'Certificate'),
    certificate ? h('div', { class: 'grid' },
      fact('Status', [state(certificate.status), ' ', state(certificate.renewal_status)]),
      fact('Expires', certificate.expires_at ? `${new Date(certificate.expires_at).toISOString().slice(0, 10)}` : '—'),
      fact('Issuer', certificate.issuer, true),
      fact('Fingerprint', short(certificate.fingerprint), true),
      fact('Key', certificate.held_here ? 'Held by this node' : 'Not on this node', false),
      certificate.last_error ? fact('Last error', h('span', { class: 'error' }, certificate.last_error)) : null)
      : h('div', { class: 'panel empty' }, 'TLS is disabled for this domain.'),
  ];
}

// ---- Router and live updates --------------------------------------------------------------

function route() {
  const parts = location.hash.replace(/^#\/?/, '').split('/').filter(Boolean).map(decodeURIComponent);
  if (parts[0] === 'environments' && parts[2] === 'projects' && parts[3]) {
    const tab = parts[4] ? parts[4][0].toUpperCase() + parts[4].slice(1) : 'Overview';
    return { nav: 'environments', render: () => projectView(parts[1], parts[3], tab) };
  }
  if (parts[0] === 'environments' && parts[1]) return { nav: 'environments', render: () => environmentView(parts[1]) };
  if (parts[0] === 'projects' && parts[1]) return { nav: 'projects', render: () => projectDetailView(parts[1]) };
  if (parts[0] === 'projects') return { nav: 'projects', render: projectsView };
  if (parts[0] === 'services') return { nav: 'services', render: servicesView };
  if (parts[0] === 'deployments' && parts[1]) return { nav: 'environments', render: () => deploymentView(parts[1]) };
  if (parts[0] === 'domains' && parts[1]) return { nav: 'domains', render: () => domainView(parts[1]) };
  if (parts[0] === 'domains') return { nav: 'domains', render: domainsView };
  if (parts[0] === 'events') return { nav: 'events', render: eventsView };
  return { nav: 'environments', render: environmentsView };
}

let rendering = null;
async function render() {
  const { nav, render: renderView } = route();
  for (const link of document.querySelectorAll('[data-nav]')) link.classList.toggle('active', link.dataset.nav === nav);
  const current = rendering = Symbol('render');
  try {
    const content = await renderView();
    if (current !== rendering) return;
    view.replaceChildren(...[content].flat());
  } catch (error) {
    if (current !== rendering) return;
    view.replaceChildren(h('div', { class: 'panel empty error' }, error.kind === 'not_found' ? 'Not found.' : `The Compute API returned an error: ${error.message}`));
  }
  refreshDaemon();
}

async function refreshDaemon() {
  const box = document.getElementById('daemon');
  try {
    const status = await api('GET', '/status');
    box.replaceChildren(
      state(status.state_available ? 'healthy' : 'failed', status.state_available ? `${status.state.kind} state` : 'control state unavailable'),
      h('span', { class: 'chip', title: status.state.location }, status.instance_id));
  } catch {
    box.replaceChildren(state('failed', 'daemon unreachable'));
  }
}

let refreshTimer = null;
function scheduleRefresh() {
  if (dialog.open) return;
  clearTimeout(refreshTimer);
  refreshTimer = setTimeout(render, 250);
}

function listen() {
  const source = new EventSource('/events/stream');
  source.onmessage = scheduleRefresh;
  source.addEventListener('lagged', scheduleRefresh);
  source.onerror = () => { setTimeout(refreshDaemon, 1000); };
}

document.getElementById('token').addEventListener('click', () => {
  const input = h('input', { id: 'token-input', type: 'password', value: token(), autocomplete: 'off' });
  modal('API token', h('div', {},
    h('p', {}, 'When the daemon requires a bearer token for changes, enter it here. It is kept for this browser tab only.'),
    h('label', { for: 'token-input' }, 'Token'), input), [
    h('button', { onclick: close }, 'Cancel'),
    h('button', { class: 'primary', onclick: () => {
      try { sessionStorage.setItem('compute.token', input.value); } catch { /* storage unavailable */ }
      close();
      toast('Token set');
    } }, 'Save'),
  ]);
});

window.addEventListener('hashchange', render);
render();
listen();

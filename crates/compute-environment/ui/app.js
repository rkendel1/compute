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
  active: ['ok', '●', 'Active'],
  starting: ['warn', '◐', 'Starting'],
  pending: ['idle', '○', 'Pending'],
  queued: ['warn', '◐', 'Queued'],
  admitted: ['warn', '◐', 'Admitted'],
  placed: ['warn', '◐', 'Placed'],
  deploying: ['warn', '◐', 'Deploying'],
  stopping: ['warn', '◐', 'Stopping'],
  degraded: ['warn', '◐', 'Degraded'],
  unknown: ['idle', '○', 'Unknown'],
  stopped: ['idle', '○', 'Stopped'],
  superseded: ['idle', '○', 'Superseded'],
  unhealthy: ['bad', '×', 'Unhealthy'],
  failed: ['bad', '×', 'Failed'],
  denied: ['bad', '×', 'Denied'],
};

function state(value, text) {
  const [tone, glyph, label] = STATES[value] || ['idle', '○', value];
  return h('span', { class: `state ${tone}` }, h('span', { class: 'glyph', 'aria-hidden': 'true' }, glyph), text || label);
}

/// The single status of a project or environment: actual state, unless it
/// runs, in which case its health.
function status(item) {
  if (item.deployment && ['queued', 'admitted', 'placed', 'starting'].includes(item.deployment.status)) {
    return state('deploying');
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
        h('p', { class: 'subtitle' }, 'Every workload is admitted and placed before anything changes. If admission fails, the current deployment keeps running.')), [
        h('button', { onclick: close }, 'Cancel'),
        h('button', { class: 'primary', 'data-apply': 'true', onclick: async () => {
          close();
          const deployment = await act(`Deploying ${state.project} to ${state.environment}`, () => api('POST', '/deployments', {
            project: state.project, environment: state.environment, revision: chosen.revision_id || state.revision,
          }));
          if (deployment && deployment.status === 'failed') toast(`Deployment failed: ${deployment.failure}`, true);
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
      return table(['Deployment', 'Revision', 'Status', 'Admission', 'Created', 'Promoted from'], deployments.map((deployment) => h('tr', {},
        h('td', { class: 'mono' }, deployment.deployment_id),
        h('td', { class: 'mono' }, deployment.revision),
        h('td', {}, state(deployment.status), deployment.failure ? h('div', { class: 'error' }, deployment.failure) : null),
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
    table(['Deployment', 'Environment', 'Revision', 'Status', 'Created'], detail.deployments.map((deployment) => h('tr', {},
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

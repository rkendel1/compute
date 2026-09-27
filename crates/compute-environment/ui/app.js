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
  provisioning: ['warn', '◐', 'Provisioning'],
  resuming: ['warn', '◐', 'Resuming'],
  destroying: ['warn', '◐', 'Destroying'],
  exited: ['idle', '○', 'Exited'],
  destroyed: ['idle', '○', 'Destroyed'],
  degraded: ['warn', '◐', 'Degraded'],
  unknown: ['idle', '○', 'Unknown'],
  stopped: ['idle', '○', 'Stopped'],
  superseded: ['idle', '○', 'Superseded'],
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
  const name = h('input', { id: 'environment-name', placeholder: 'my-app', autocomplete: 'off' });
  // Modest defaults: a computer that fits on a laptop. Ask for more.
  const cpu = h('input', { id: 'environment-cpu', type: 'number', min: '1', value: '1' });
  const memory = h('input', { id: 'environment-memory', type: 'number', min: '1', value: '1' });
  const storage = h('input', { id: 'environment-storage', type: 'checkbox' });
  const endpoint = h('input', { id: 'environment-public', type: 'checkbox' });
  const features = h('input', { id: 'environment-features', placeholder: 'containers, kvm, gpu', autocomplete: 'off' });
  const keep = h('input', { type: 'radio', name: 'environment-lifetime', value: 'persistent', checked: true });
  const temporary = h('input', { type: 'radio', name: 'environment-lifetime', value: 'ephemeral' });
  const target = h('input', { id: 'environment-target', placeholder: 'placement chooses', autocomplete: 'off' });
  const node = h('input', { id: 'environment-node', type: 'checkbox' });
  modal('New environment', h('div', {},
    h('label', { for: 'environment-name' }, 'Name'), name,
    h('p', {}, h('strong', {}, 'What kind of computer do you need?'), ' Compute places it on a target that can provide it.'),
    h('label', { for: 'environment-cpu' }, 'CPUs'), cpu,
    h('label', { for: 'environment-memory' }, 'Memory (GiB)'), memory,
    h('label', { class: 'check' }, storage, ' Persistent storage'),
    h('label', { class: 'check' }, endpoint, ' Public endpoint'),
    h('label', { for: 'environment-features' }, 'Machine features'), features,
    h('label', {}, 'Lifetime'),
    h('label', { class: 'check' }, keep, ' Keep running, until destroyed'),
    h('label', { class: 'check' }, temporary, ' Temporary (1 hour), its evidence kept'),
    h('label', { for: 'environment-target' }, 'Target (optional)'), target,
    h('label', { class: 'check' }, node, ' No computer: run projects on this control-plane node')), [
    h('button', { onclick: close }, 'Cancel'),
    h('button', { class: 'primary', onclick: async () => {
      close();
      const definition = { name: name.value };
      if (!node.checked) {
        const capabilities = [storage.checked ? 'persistent_storage' : null, endpoint.checked ? 'public_endpoint' : null].filter(Boolean);
        definition.computer = {
          lifecycle: temporary.checked ? 'ephemeral' : 'persistent',
          requirements: {
            cpu_count: Number(cpu.value) || undefined,
            memory_bytes: Number(memory.value) ? Math.round(Number(memory.value) * 2 ** 30) : undefined,
            capabilities,
            features: features.value.split(',').map((item) => item.trim()).filter(Boolean),
          },
          target: target.value.trim() || undefined,
        };
        if (temporary.checked) definition.computer.ttl_seconds = 3600;
      }
      const created = await act(`Environment ${name.value} created`, () => api('POST', '/environments', definition));
      if (created && definition.computer && document.body.dataset.mode === 'work') location.hash = `#/work/${enc(name.value)}`;
    } }, 'Create'),
  ]);
  name.focus();
}

async function environmentView(name) {
  const environment = await api('GET', `/environments/${enc(name)}`);
  const computer = environment.computer;
  const header = [
    h('div', { class: 'crumbs' }, h('a', { href: '#/' }, 'Environments'), ' / ', name),
    h('div', { class: 'title', 'data-view': 'manage', 'data-environment-id': environment.environment_id },
      h('h1', {}, name.toUpperCase()), computer ? state(computer.status) : status(environment),
      h('span', { class: 'chip mono', title: 'Environment ID' }, environment.environment_id),
      h('div', { class: 'actions' },
        computer
          ? h('a', { class: 'button', href: `#/work/${enc(name)}` }, 'Work')
          : h('button', { onclick: () => deployWizard({ environment: name }) }, 'Add project'),
        h('button', { onclick: () => environmentLifecycle(name, 'start') }, 'Start'),
        h('button', { onclick: () => environmentLifecycle(name, 'restart') }, 'Restart'),
        h('button', { class: 'danger', onclick: () => environmentLifecycle(name, 'stop') }, 'Stop'))),
  ];
  if (computer) {
    // An environment with a computer: its projects are software in its
    // repositories, built and run there. Deployment is a release of one,
    // reconciled in place.
    const observed = computer.observed || {};
    const rollouts = await api('GET', `/rollouts?environment=${enc(name)}`).catch(() => []);
    const running = computer.status === 'running';
    const rows = (computer.desired.projects || []).map((project) => {
      const repository = (computer.desired.repositories || []).find((item) => item.name === project.repository) || {};
      const seen = observed.repositories && observed.repositories[project.repository];
      const built = observed.builds && observed.builds[project.name];
      const current = rollouts.find((rollout) => rollout.project === project.name && rollout.status !== 'superseded');
      return h('tr', { class: 'link', 'data-project': project.name, onclick: () => { location.hash = `#/software/${enc(project.name)}`; } },
        h('td', {}, h('strong', {}, project.name)),
        h('td', {}, current ? [current.version, ' ', state(current.status === 'applying' ? 'deploying' : current.status)] : h('span', { class: 'meta' }, 'unversioned')),
        h('td', { class: 'mono' }, revisionText(repository.revision, seen && seen.commit)),
        h('td', {}, built ? state(built.evidence.outcome === 'succeeded' ? 'complete' : 'failed', built.evidence.outcome) : '—'));
    });
    const endpointOf = (process) => (computer.endpoints || []).find((endpoint) => endpoint.process === process);
    const processes = (computer.desired.processes || []).map((process) => {
      const seen = observed.processes && observed.processes[process.name];
      const endpoint = endpointOf(process.name);
      return h('tr', { 'data-process': process.name },
        h('td', {}, h('strong', {}, process.name)),
        h('td', {}, process.kind || 'application'),
        h('td', {}, seen ? state(seen.state) : '—'),
        h('td', { class: 'mono' }, endpoint && endpoint.url ? h('a', { href: endpoint.url, target: '_blank', rel: 'noopener' }, endpoint.url) : '—'),
        h('td', {},
          h('button', { disabled: !running, onclick: () => act(`Restarting ${process.name}`, () => api('POST', `/environments/${enc(name)}/processes/${enc(process.name)}/restart`)) }, 'Restart'), ' ',
          h('button', { disabled: !running, onclick: async () => {
            const logs = await api('GET', `/environments/${enc(name)}/logs?process=${enc(process.name)}&limit=200`).catch((error) => ({ log: error.message }));
            showOutput(`${process.name} log`, logs.log || '', '');
          } }, 'Logs')));
    });
    return [
      header,
      h('div', { class: 'subtitle' }, `${(computer.desired.projects || []).length} projects · ${(computer.desired.processes || []).length} processes · desired ${environment.desired_state} · policy ${short(environment.policy_id)}`),
      machineSection(name, environment),
      h('h2', {}, 'Software'),
      table(['Project', 'Version', 'Revision', 'Build'], rows, 'No projects. Run one.'),
      h('h2', {}, 'Applications, services, and agents'),
      table(['Name', 'Kind', 'Health', 'Endpoint', ''], processes, 'Nothing runs yet. Add it in Work.'),
      h('h2', {}, 'Deployments'),
      table(['Kind', 'Project', 'Version', 'Status', 'By', 'When'], rollouts.map((rollout) => h('tr', { class: 'link', onclick: () => { location.hash = `#/operations/rollout/${enc(rollout.rollout_id)}`; } },
        h('td', {}, rollout.kind), h('td', {}, rollout.project), h('td', {}, rollout.version),
        h('td', {}, state(rollout.status === 'applying' ? 'deploying' : rollout.status)), h('td', {}, rollout.created_by), h('td', {}, ago(rollout.created_at)))), 'Nothing deployed here yet.'),
    ];
  }
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
    header,
    h('div', { class: 'subtitle' }, `${environment.project_count} projects · ${environment.workload_count} workloads · desired ${environment.desired_state} · policy ${short(environment.policy_id)}`),
    machineSection(name, environment),
    h('h2', {}, 'Projects'),
    table(['Project', 'Revision', 'Desired', 'Status', 'Workloads', 'Provider', 'Last deployment'], rows, 'No projects in this environment. Add one.'),
  ];
}

// ---- One environment, two modes ------------------------------------------------------
//
// Manage controls which computer exists and how it is operated: placement,
// requirements, lifetime, replacement. Work controls what is inside that
// computer and what it is doing. Both are views of the same environment
// (the same ID, the same API, the same authority); neither holds state of
// its own.
//
// Work is not a remote terminal with buttons. The operator edits a local
// draft of what the environment should hold; nothing changes until GO
// submits the whole draft as one durable, authorized, generation-fenced
// change, which the environment's computer reconciles in place.

/// Local drafts, by environment. Only this page holds them; GO, Discard,
/// or Refresh ends them.
const DRAFTS = new Map();
/// The last command run from Work, by environment: its output on screen.
const OUTPUT = new Map();

const LIST_FIELDS = ['repositories', 'packages', 'processes', 'projects'];

function draftOf(name, computer) {
  let draft = DRAFTS.get(name);
  if (!draft) {
    const contents = structuredClone(computer.desired);
    for (const field of LIST_FIELDS) contents[field] = contents[field] || [];
    draft = {
      base: computer.desired.generation || 0,
      contents,
      config: { ...(computer.config || {}) },
      lifecycle: { lifecycle: computer.requested_lifecycle || computer.lifecycle, ttl_seconds: 3600 },
      conflict: null,
    };
  }
  return draft;
}

function edit(name, draft, change) {
  change(draft);
  DRAFTS.set(name, draft);
  render();
}

/// The changes a draft makes, as the operator would say them.
function changes(computer, draft) {
  const out = [];
  const nouns = { repositories: 'repository', packages: 'package', processes: 'process', projects: 'project' };
  for (const field of LIST_FIELDS) {
    const before = new Map((computer.desired[field] || []).map((item) => [item.name, JSON.stringify(item)]));
    const after = new Map(draft.contents[field].map((item) => [item.name, JSON.stringify(item)]));
    for (const [item, value] of after) {
      if (!before.has(item)) out.push(`Add ${nouns[field]} ${item}`);
      else if (before.get(item) !== value) out.push(`Change ${nouns[field]} ${item}`);
    }
    for (const item of before.keys()) if (!after.has(item)) out.push(`Remove ${nouns[field]} ${item}`);
  }
  const config = computer.config || {};
  for (const key of new Set([...Object.keys(config), ...Object.keys(draft.config)])) {
    if (!(key in draft.config)) out.push(`Unset ${key}`);
    else if (!(key in config)) out.push(`Set ${key}`);
    else if (config[key] !== draft.config[key]) out.push(`Change ${key}`);
  }
  if (lifecycleChanged(computer, draft)) out.push(draft.lifecycle.lifecycle === 'persistent' ? 'Keep running until destroyed' : `Make temporary (${draft.lifecycle.ttl_seconds}s)`);
  return out;
}

function configChanged(computer, draft) {
  return JSON.stringify(Object.entries(computer.config || {}).sort()) !== JSON.stringify(Object.entries(draft.config).sort());
}

function lifecycleChanged(computer, draft) {
  return draft.lifecycle.lifecycle !== (computer.requested_lifecycle || computer.lifecycle) || draft.lifecycle.touched === true;
}

/// GO: the whole draft as one authorized change, refused if the
/// environment changed since it was loaded.
async function go(name, computer, draft) {
  const body = { contents: draft.contents, expected_generation: draft.base };
  if (configChanged(computer, draft)) body.config = draft.config;
  if (lifecycleChanged(computer, draft)) {
    body.lifecycle = { lifecycle: draft.lifecycle.lifecycle };
    if (draft.lifecycle.lifecycle === 'ephemeral') body.lifecycle.ttl_seconds = Number(draft.lifecycle.ttl_seconds) || 3600;
  }
  try {
    await api('POST', `/environments/${enc(name)}/contents`, body);
    DRAFTS.delete(name);
    toast(`GO: ${name} is changing in place`);
  } catch (error) {
    if (error.kind === 'conflict') {
      draft.conflict = error.message;
      DRAFTS.set(name, draft);
    } else {
      toast(`GO failed: ${error.message}`, true);
    }
  }
  await render();
}

function lifetimeText(computer) {
  if (computer.lifecycle === 'persistent') return 'Keep running (until destroyed)';
  return computer.expires_at ? `Temporary · expires ${new Date(computer.expires_at).toLocaleString()}` : 'Temporary';
}

function machineFacts(computer) {
  const requirements = computer.requirements || {};
  const machine = computer.machine || {};
  return [
    fact('Status', state(computer.status)),
    fact('Resources', [requirements.cpu_count ? `${requirements.cpu_count} CPU` : null, requirements.memory_bytes ? bytes(requirements.memory_bytes) : null, requirements.architecture]
      .filter(Boolean).join(' · ') || 'Any'),
    fact('Target', computer.target
      ? `${computer.target}${machine.provider_kind ? ` · ${machine.provider_kind}` : ''}`
      : (computer.requested_target ? `${computer.requested_target} (requested)` : 'placing…'), true),
    fact('Lifetime', lifetimeText(computer)),
    fact('Machine', machine.resource || machine.session_id || '—', true),
  ];
}

// ---- Manage: the machine ------------------------------------------------------------

function machineSection(name, environment) {
  const computer = environment.computer;
  if (!computer) {
    return [
      h('h2', {}, 'Computer'),
      h('div', { class: 'panel note', 'data-machine': 'node' },
        'This environment has no computer of its own: its projects run on this control-plane node (',
        h('span', { class: 'mono' }, environment.machine ? environment.machine.target : 'local'),
        '). Create an environment with a computer to work in it and to change it in place.'),
    ];
  }
  const live = !['destroying', 'destroyed', 'expired'].includes(computer.status);
  return [
    h('h2', {}, 'Computer'),
    h('div', { class: 'grid', 'data-machine': 'computer' }, machineFacts(computer),
      fact('Placement', short(computer.placement_id), true),
      fact('Contents', computer.converged ? state('running', `Converged · generation ${computer.desired.generation || 0}`)
        : state('pending', `Reconciling · ${computer.observed.converged_generation || 0} of ${computer.desired.generation || 0}`))),
    computer.failure ? h('div', { class: 'panel error' }, `${computer.failure.phase}: ${computer.failure.code} — ${computer.failure.message}`) : null,
    h('div', { class: 'actions row' },
      h('a', { class: 'button primary', href: `#/work/${enc(name)}`, 'data-work': 'true' }, 'Work on this →'),
      h('button', { disabled: !live, onclick: () => replaceDialog(name, computer) }, 'Replace machine…'),
      h('button', { class: 'danger', disabled: !live, onclick: () => destroyComputer(name) }, 'Destroy')),
  ];
}

function replaceDialog(name, computer) {
  const requirements = computer.requirements || {};
  const cpu = h('input', { id: 'replace-cpu', type: 'number', min: '1', value: String(requirements.cpu_count || 1) });
  const memory = h('input', { id: 'replace-memory', type: 'number', min: '1', value: String(Math.max(1, Math.round((requirements.memory_bytes || 2 ** 30) / 2 ** 30))) });
  modal(`Replace ${name}'s machine`, h('div', {},
    h('p', {}, 'Replacement provisions a new machine that meets these requirements and moves the environment\'s contents onto it. Ordinary changes never need this: they happen in place, from Work.'),
    h('label', { for: 'replace-cpu' }, 'CPUs'), cpu,
    h('label', { for: 'replace-memory' }, 'Memory (GiB)'), memory), [
    h('button', { onclick: close }, 'Cancel'),
    h('button', { class: 'primary danger', onclick: async () => {
      close();
      await act(`Replacing ${name}'s machine`, () => api('POST', `/environments/${enc(name)}/replace`, {
        ...requirements,
        cpu_count: Number(cpu.value) || undefined,
        memory_bytes: Number(memory.value) ? Math.round(Number(memory.value) * 2 ** 30) : undefined,
      }));
    } }, 'Replace'),
  ]);
}

async function destroyComputer(name) {
  if (await confirmImpact(`Destroy ${name}'s machine?`, ['The machine, its workspace, and everything running on it'], ['The environment\'s record, desired contents, and evidence'], 'danger')) {
    DRAFTS.delete(name);
    await act(`Destroying ${name}`, () => api('DELETE', `/environments/${enc(name)}`));
  }
}

// ---- Work ----------------------------------------------------------------------------

async function workHomeView() {
  const environments = await api('GET', '/environments');
  const computers = environments.filter((environment) => environment.computer);
  return [
    h('div', { class: 'title' }, h('h1', {}, 'Work'),
      h('div', { class: 'actions' },
        h('button', { onclick: temporaryDialog }, 'Temporary environment'),
        h('button', { class: 'primary', onclick: createEnvironment }, 'New environment'))),
    h('div', { class: 'subtitle' }, 'What do you want to work on?'),
    computers.length ? h('div', { class: 'cards' }, computers.map((environment) =>
      h('div', { class: 'panel card', role: 'link', tabindex: '0', 'data-environment': environment.name,
        onclick: () => { location.hash = `#/work/${enc(environment.name)}`; },
        onkeydown: (event) => { if (event.key === 'Enter') location.hash = `#/work/${enc(environment.name)}`; } },
      h('div', { class: 'name' }, environment.name),
      h('div', { class: 'meta' }, environment.target ? `on ${environment.target}` : 'placing…'),
      state(environment.computer))))
      : h('div', { class: 'panel empty' }, 'No environment has a computer yet. Create one, or start a temporary one.'),
  ];
}

function temporaryDialog() {
  const cpu = h('input', { id: 'temporary-cpu', type: 'number', min: '1', value: '1' });
  const memory = h('input', { id: 'temporary-memory', type: 'number', min: '1', value: '1' });
  const hours = h('input', { id: 'temporary-hours', type: 'number', min: '1', value: '1' });
  modal('Temporary environment', h('div', {},
    h('p', {}, 'A computer for this piece of work. It expires, and its record and evidence remain; close the session to end it sooner.'),
    h('label', { for: 'temporary-cpu' }, 'CPUs'), cpu,
    h('label', { for: 'temporary-memory' }, 'Memory (GiB)'), memory,
    h('label', { for: 'temporary-hours' }, 'Lifetime (hours)'), hours), [
    h('button', { onclick: close }, 'Cancel'),
    h('button', { class: 'primary', onclick: async () => {
      close();
      const session = await act('Temporary environment requested', () => api('POST', '/sessions', {
        computer: {
          lifecycle: 'ephemeral',
          requirements: {
            cpu_count: Number(cpu.value) || undefined,
            memory_bytes: Number(memory.value) ? Math.round(Number(memory.value) * 2 ** 30) : undefined,
          },
          ttl_seconds: Math.max(1, Number(hours.value) || 1) * 3600,
        },
      }));
      if (session) location.hash = `#/work/${enc(session.environment)}`;
    } }, 'Start'),
  ]);
}

async function workView(name) {
  const environment = await api('GET', `/environments/${enc(name)}`);
  if (!environment.computer) {
    return [
      h('div', { class: 'crumbs' }, h('a', { href: '#/work' }, 'Work'), ' / ', name),
      h('div', { class: 'panel empty' }, `${name} runs on this control-plane node and has no computer to work in. Manage it instead, or create an environment with a computer.`),
    ];
  }
  const computer = environment.computer;
  const sessions = await api('GET', `/sessions?environment=${enc(name)}`).catch(() => []);
  const draft = draftOf(name, computer);
  const live = !['destroying', 'destroyed', 'expired'].includes(computer.status);
  const running = computer.status === 'running';
  const pending = changes(computer, draft);
  const stale = draft.conflict || (DRAFTS.has(name) && draft.base !== (computer.desired.generation || 0));
  const { contents } = draft;
  const observed = computer.observed || {};
  const input = (value, onchange, attributes) => h('input', { value: value === undefined ? '' : value, autocomplete: 'off', disabled: !live, onchange: (event) => edit(name, draft, () => onchange(event.target.value)), ...attributes });
  const remove = (list, item) => h('button', { class: 'danger', disabled: !live, onclick: () => edit(name, draft, (next) => { next.contents[list] = next.contents[list].filter((entry) => entry.name !== item); }) }, 'Remove');
  const evidence = (item) => item ? h('span', { class: 'chip', title: item.evidence ? `job ${item.evidence.job_id}` : '' }, item.evidence && item.evidence.error ? h('span', { class: 'error' }, item.evidence.error) : ago(item.evidence && item.evidence.at)) : '—';

  const repositories = contents.repositories.map((repository) => h('tr', { 'data-repository': repository.name },
    h('td', {}, h('strong', {}, repository.name)),
    h('td', { class: 'mono' }, repository.url),
    h('td', {}, input(repository.revision, (value) => { repository.revision = value; }, { 'aria-label': `${repository.name} revision` })),
    h('td', { class: 'mono' }, observed.repositories && observed.repositories[repository.name]
      ? `${observed.repositories[repository.name].revision} ${short(observed.repositories[repository.name].commit)}` : '—'),
    h('td', {}, evidence(observed.repositories && observed.repositories[repository.name])),
    h('td', {},
      h('button', { disabled: !live, 'data-pull': repository.name, title: 'Fetch the revision again: a branch that moved',
        onclick: () => edit(name, draft, () => { repository.sync = (repository.sync || 0) + 1; }) }, 'Pull'), ' ',
      remove('repositories', repository.name))));

  const projects = contents.projects.map((project) => {
    const commands = [project.build && project.build.length ? 'build' : null, project.test && project.test.length ? 'test' : null, ...Object.keys(project.commands || {})].filter(Boolean);
    const built = observed.builds && observed.builds[project.name];
    return h('tr', { 'data-project': project.name },
      h('td', {}, h('strong', {}, project.name)),
      h('td', {}, project.repository),
      h('td', {}, built ? state(built.evidence.outcome === 'succeeded' ? 'complete' : 'failed', `${built.evidence.outcome === 'succeeded' ? 'Built' : 'Build failed'} ${short(built.commit)}`) : (project.build && project.build.length ? state('pending', 'Not built') : '—')),
      h('td', {}, commands.map((command) => h('button', { disabled: !running, 'data-command': `${project.name}/${command}`, onclick: () => runProjectCommand(name, project.name, command) }, command))),
      h('td', {}, remove('projects', project.name)));
  });

  const byKind = (kinds) => contents.processes.filter((process) => kinds.includes(process.kind || 'application'));
  const processRows = (list) => list.map((process) => {
    const seen = observed.processes && observed.processes[process.name];
    return h('tr', { 'data-process': process.name },
      h('td', {}, h('strong', {}, process.name)),
      h('td', { class: 'mono' }, process.command.join(' ')),
      h('td', {}, process.repository || '—'),
      h('td', {}, process.port ? String(process.port) : '—'),
      h('td', {}, h('select', { disabled: !live, 'aria-label': `${process.name} desired`, onchange: (event) => edit(name, draft, () => { process.desired = event.target.value; }) },
        ['running', 'stopped'].map((value) => h('option', { value, selected: (process.desired || 'running') === value }, value)))),
      h('td', {}, seen ? state(seen.state, `${STATES[seen.state] ? STATES[seen.state][2] : seen.state}${seen.pid ? ` · pid ${seen.pid}` : ''}`) : '—'),
      h('td', {},
        h('button', { disabled: !running, onclick: () => showLog(name, process.name) }, 'Log'), ' ',
        h('button', { disabled: !running, 'data-restart': process.name, onclick: () => act(`Restarting ${process.name}`, () => api('POST', `/environments/${enc(name)}/processes/${enc(process.name)}/restart`)) }, 'Restart'), ' ',
        remove('processes', process.name)));
  });
  const processHeaders = ['Name', 'Command', 'Repository', 'Port', 'Desired', 'Observed', ''];

  const packages = contents.packages.map((item) => h('tr', { 'data-package': item.name },
    h('td', {}, h('strong', {}, item.name)),
    h('td', { class: 'mono' }, item.install.join(' ')),
    h('td', {}, item.repository || '—'),
    h('td', {}, evidence(observed.packages && observed.packages[item.name])),
    h('td', {}, remove('packages', item.name))));

  const config = Object.keys(draft.config).sort().map((key) => h('tr', { 'data-config': key },
    h('td', { class: 'mono' }, key),
    h('td', {}, input(draft.config[key], (value) => { draft.config[key] = value; }, { type: 'password', 'aria-label': `${key} value` })),
    h('td', {}, h('button', { class: 'danger', disabled: !live, onclick: () => edit(name, draft, (next) => { delete next.config[key]; }) }, 'Unset'))));

  const endpoints = (computer.endpoints || []).map((endpoint) => h('tr', { 'data-endpoint': endpoint.process },
    h('td', {}, endpoint.process),
    h('td', {}, String(endpoint.port)),
    h('td', { class: 'mono' }, endpoint.url ? h('a', { href: endpoint.url, target: '_blank', rel: 'noopener' }, endpoint.url) : '—'),
    h('td', {}, state(endpoint.serving ? 'serving' : 'stopped'))));

  const output = OUTPUT.get(name);
  const terminal = h('input', { id: 'work-command', placeholder: 'make test', autocomplete: 'off', disabled: !running, 'aria-label': 'Command to run in the computer',
    onkeydown: (event) => { if (event.key === 'Enter' && event.target.value.trim()) runCommand(name, event.target.value.trim()); } });

  const radio = (value, label) => h('label', { class: 'check' },
    h('input', { type: 'radio', name: 'lifetime', value, disabled: !live, checked: draft.lifecycle.lifecycle === value,
      onchange: () => edit(name, draft, (next) => { next.lifecycle.lifecycle = value; next.lifecycle.touched = value === 'ephemeral'; }) }), ' ', label);

  return h('div', { class: 'work draft', 'data-view': 'work', 'data-environment-id': environment.environment_id },
    h('div', { class: 'crumbs' }, h('a', { href: '#/work' }, 'Work'), ' / ', name),
    h('div', { class: 'title' }, h('h1', {}, name), state(computer.status),
      h('span', { class: 'chip mono', title: 'Environment ID' }, environment.environment_id),
      h('div', { class: 'actions' },
        h('a', { class: 'button', href: `#/environments/${enc(name)}` }, 'Manage'),
        h('button', { disabled: !live, onclick: () => act(`Reconciling ${name}`, () => api('POST', `/environments/${enc(name)}/reconcile`)) }, 'Reconcile'))),
    h('div', { class: 'subtitle' }, 'What do you want to do?'),
    h('div', { class: 'actions row' },
      h('a', { class: 'button', href: '#/run?add=1' }, 'Add another project'),
      contents.projects.map((project) => h('a', { class: 'button', href: `#/software/${enc(project.name)}` }, `${project.name}: versions`))),
    !computer.converged && live ? h('div', { class: 'panel progress-strip', 'data-reconciling': 'true' },
      h('div', { class: 'meta' }, `Compute is making ${name} hold generation ${computer.desired.generation || 0}:`),
      stepList(reconcileProgress(computer))) : null,
    computer.failure ? h('div', { class: 'panel error' }, `${computer.failure.phase}: ${computer.failure.code} — ${computer.failure.message}`) : null,

    h('h3', {}, 'Repositories'),
    table(['Repository', 'URL', 'Revision', 'Checked out', 'Last synced', ''], repositories, 'No repositories.'),
    live ? addRow(['name', 'url', 'revision'], 'Add repository', (value) => edit(name, draft, (next) => {
      next.contents.repositories.push({ name: value.name, url: value.url, revision: value.revision || 'main' });
    })) : null,

    h('h3', {}, 'Projects'),
    table(['Project', 'Repository', 'Build', 'Run', ''], projects, 'No projects. A project names a repository and how to build and test it.'),
    live ? addRow(['name', 'repository', 'build', 'test'], 'Add project', (value) => edit(name, draft, (next) => {
      const sh = (command) => command ? ['sh', '-c', command] : undefined;
      next.contents.projects.push({ name: value.name, repository: value.repository, build: sh(value.build), test: sh(value.test) });
    })) : null,

    h('h3', {}, 'Applications'),
    table(processHeaders, processRows(byKind(['application'])), 'No applications.'),
    h('h3', {}, 'Services'),
    table(processHeaders, processRows(byKind(['service'])), 'No services.'),
    h('h3', {}, 'Agents'),
    table(processHeaders, processRows(byKind(['agent'])), 'No agents.'),
    byKind(['process']).length ? [h('h3', {}, 'Processes'), table(processHeaders, processRows(byKind(['process'])))] : null,
    live ? addRow(['name', 'command', 'repository', 'port', 'kind'], 'Add', (value) => edit(name, draft, (next) => {
      next.contents.processes.push({
        name: value.name,
        kind: ['service', 'agent', 'process'].includes(value.kind) ? value.kind : 'application',
        command: value.command.split(/\s+/).filter(Boolean),
        repository: value.repository || undefined,
        port: Number(value.port) || undefined,
        desired: 'running',
      });
    })) : null,

    live ? h('div', { class: 'actions row' },
      h('span', { class: 'meta' }, 'Add from a template: '),
      Object.entries(TEMPLATES).map(([key, template]) => h('button', { 'data-template': key, onclick: () => edit(name, draft, (next) => {
        if (next.contents.processes.some((process) => process.name === template.name)) return;
        next.contents.processes.push({ ...structuredClone(template), desired: 'running' });
      }) }, { database: 'Database (PostgreSQL)', redis: 'Redis', agent: 'Agent' }[key]))) : null,
    h('h3', {}, 'Packages'),
    table(['Package', 'Install', 'Repository', 'Installed', ''], packages, 'No packages.'),
    live ? addRow(['name', 'install', 'repository'], 'Add package', (value) => edit(name, draft, (next) => {
      next.contents.packages.push({ name: value.name, install: value.install.split(/\s+/).filter(Boolean), repository: value.repository || undefined });
    })) : null,

    h('h3', {}, 'Configuration'),
    table(['Key', 'Value', ''], config, 'No configuration.'),
    live ? addRow(['name', 'value'], 'Set', (value) => edit(name, draft, (next) => { next.config[value.name] = value.value; })) : null,

    h('h3', {}, 'Endpoints'),
    table(['Process', 'Port', 'Address', ''], endpoints, 'No endpoints. Give a process a port to publish one.'),

    h('h3', {}, 'Terminal'),
    h('div', { class: 'panel terminal' },
      h('div', { class: 'add-row' }, terminal, h('button', { disabled: !running, onclick: () => terminal.value.trim() && runCommand(name, terminal.value.trim()) }, 'Run')),
      output ? h('div', { 'data-output': output.status },
        h('div', { class: 'meta' }, `${output.title} · ${output.status}${output.job ? ` · job ${output.job}` : ''}`),
        h('pre', { class: 'mono' }, output.text || '')) : h('div', { class: 'meta' }, 'Commands run inside the computer as durable jobs, with receipts.')),

    h('h3', {}, 'Files'),
    h('div', { class: 'panel terminal' },
      h('div', { class: 'add-row' },
        h('input', { id: 'work-path', placeholder: 'repos', value: '', autocomplete: 'off', disabled: !running, 'aria-label': 'Path in the computer' }),
        h('button', { disabled: !running, 'data-files': 'list', onclick: () => listFiles(name, document.getElementById('work-path').value.trim()) }, 'List'),
        h('button', { disabled: !running, 'data-files': 'view', onclick: () => viewFile(name, document.getElementById('work-path').value.trim()) }, 'View')),
      h('div', { class: 'meta' }, 'Paths are relative to the computer\'s workspace; checkouts are under repos/.')),

    h('h3', {}, 'Sessions'),
    table(['Session', 'Kind', 'Status', 'Opened', ''], sessions.map((session) => h('tr', { 'data-session': session.session_id },
      h('td', { class: 'mono' }, session.session_id),
      h('td', {}, session.kind === 'ephemeral' ? 'temporary (owns this environment)' : 'attached'),
      h('td', {}, state(session.status === 'open' ? 'active' : 'stopped', session.status)),
      h('td', {}, ago(session.opened_at)),
      h('td', {}, session.status === 'open' ? h('button', { onclick: () => closeSession(name, session) }, 'Close') : null))), 'No sessions.'),
    h('div', { class: 'actions row' }, h('button', { disabled: !running, onclick: () => act(`Session opened in ${name}`, () => api('POST', '/sessions', { environment: name })) }, 'Open session')),

    h('h3', {}, 'Computer'),
    h('div', { class: 'grid' }, machineFacts(computer)),
    h('h3', {}, 'Environment lifetime'),
    h('div', { class: 'panel lifetime' },
      radio('ephemeral', 'Temporary: expires, and its evidence remains'),
      draft.lifecycle.lifecycle === 'ephemeral' ? h('label', { class: 'inline' }, 'Lifetime (seconds) ',
        input(draft.lifecycle.ttl_seconds, (value) => { draft.lifecycle.ttl_seconds = Number(value) || 3600; draft.lifecycle.touched = true; }, { type: 'number', min: '60', 'aria-label': 'Lifetime in seconds' })) : null,
      radio('persistent', 'Keep running: until destroyed')),

    h('div', { class: 'panel go' },
      stale ? h('div', { class: 'conflict', 'data-conflict': 'true' },
        h('strong', {}, 'Environment changed since you loaded it.'), ' ', draft.conflict ? h('span', { class: 'meta' }, draft.conflict) : null, ' ',
        h('button', { onclick: () => { DRAFTS.delete(name); render(); } }, 'Refresh')) : null,
      pending.length ? h('ul', {}, pending.map((item) => h('li', {}, item))) : h('div', {}, 'No local changes.'),
      h('div', { class: 'actions' },
        h('button', { disabled: !pending.length, onclick: () => { DRAFTS.delete(name); render(); } }, 'Discard'),
        h('button', { class: 'primary go', disabled: !pending.length || !live || Boolean(stale), 'data-go': 'true', onclick: () => go(name, computer, draft) }, 'GO'))));
}

/// Follow a job in the environment's computer, showing its output.
async function followJob(name, title, submitted) {
  OUTPUT.set(name, { title, status: 'running', job: submitted.job_id, text: '' });
  render();
  let delay = 100;
  for (;;) {
    const job = await api('GET', `/environments/${enc(name)}/jobs/${enc(submitted.job_id)}`);
    if (job.result || ['succeeded', 'failed', 'cancelled', 'timed_out', 'rejected'].includes(job.job.status)) {
      const result = job.result && job.result.result;
      OUTPUT.set(name, {
        title,
        status: job.job.status,
        job: submitted.job_id,
        text: result ? `${result.stdout.text}${result.stderr.text}` : (job.job.failure || ''),
      });
      render();
      return;
    }
    await new Promise((resolve) => setTimeout(resolve, delay));
    delay = Math.min(delay * 2, 1000);
  }
}

async function runCommand(name, line) {
  try {
    const submitted = await api('POST', `/environments/${enc(name)}/exec`, { command: ['sh', '-c', line] });
    await followJob(name, `$ ${line}`, submitted);
  } catch (error) {
    toast(`Run failed: ${error.message}`, true);
  }
}

async function runProjectCommand(name, project, command) {
  try {
    const submitted = await api('POST', `/environments/${enc(name)}/run`, { project, command });
    await followJob(name, `${project} ${command}`, submitted);
  } catch (error) {
    toast(`${project} ${command} failed: ${error.message}`, true);
  }
}

async function showLog(name, process) {
  try {
    const logs = await api('GET', `/environments/${enc(name)}/logs?process=${enc(process)}&limit=200`);
    OUTPUT.set(name, { title: `${process} log`, status: 'succeeded', text: logs.log || '' });
    render();
  } catch (error) {
    toast(`Log failed: ${error.message}`, true);
  }
}

async function closeSession(name, session) {
  if (session.kind === 'ephemeral' && !await confirmImpact('Close this session?', [`Its temporary environment ${name} and its machine`], ['The records and evidence'], 'danger')) return;
  await act('Session closed', () => api('DELETE', `/sessions/${enc(session.session_id)}`));
}

/// A row of inputs that adds one item to a draft.
function addRow(fields, label, add) {
  const inputs = Object.fromEntries(fields.map((item) => [item, h('input', { placeholder: item, autocomplete: 'off', 'aria-label': `${label}: ${item}` })]));
  return h('div', { class: 'add-row' }, Object.values(inputs), h('button', { onclick: () => {
    const value = Object.fromEntries(fields.map((item) => [item, inputs[item].value.trim()]));
    if (!value.name) { toast(`${label}: a name is required`, true); return; }
    add(value);
  } }, label));
}

// ---- Home: what do you want to do? -----------------------------------------------
//
// The control plane opens on the software and the things people do with it,
// not on infrastructure. Every action below leads to a surface that shows
// what will change, then GO, then what Compute is doing.

const ACTIONS = [
  ['run', 'Run a project', 'From a Git repository or a folder: Compute proposes how, you press GO.', '#/run'],
  ['work', 'Work on a project', 'Repositories, files, terminals, processes, and logs, in its computer.', '#/work'],
  ['add', 'Add another project', 'Put more software on a computer you already have.', '#/run?add=1'],
  ['build', 'Build and test', 'Run a project\'s build, tests, and checks in its computer.', '#/software'],
  ['publish', 'Publish a new version', 'Build, test, check, and record an immutable version.', '#/software'],
  ['deploy', 'Deploy', 'Put a version on a computer, in place.', '#/software'],
  ['promote', 'Move test → production', 'Review what changes, then promote the exact version.', '#/software'],
  ['rollback', 'Roll back', 'Choose an earlier version, review, GO.', '#/software'],
  ['agent', 'Run an agent', 'Start an agent in a computer, beside your software.', '#/work'],
  ['try', 'Try software', 'A temporary computer that expires and keeps its evidence.', '#/run?try=1'],
  ['operate', 'Operate production', 'Health, logs, restarts, configuration, versions.', '#/environments'],
  ['computer', 'Create a computer', 'Say what it needs; Compute places it.', null],
];

async function homeView() {
  const [software, environments] = await Promise.all([
    api('GET', '/software').catch(() => []),
    api('GET', '/environments').catch(() => []),
  ]);
  const computers = environments.filter((environment) => environment.computer);
  const production = computers.find((environment) => /prod/.test(environment.name));
  return [
    h('div', { class: 'title' }, h('h1', {}, 'What do you want to do?')),
    h('div', { class: 'subtitle' }, 'Put software here. Make it run. Change it. Test it. Publish it. Deploy it. Promote it. Operate it.'),
    h('div', { class: 'actions-grid' }, ACTIONS.map(([key, title, text, href]) =>
      h('button', { class: 'action-tile', 'data-action': key, onclick: () => {
        if (key === 'computer') return createEnvironment();
        location.hash = key === 'operate' && production ? `#/environments/${enc(production.name)}` : href;
      } }, h('strong', {}, title), h('span', {}, text)))),
    h('h2', {}, 'Your software'),
    software.length ? h('div', { class: 'cards' }, software.map((item) =>
      h('div', { class: 'panel card', role: 'link', tabindex: '0', 'data-software': item.project,
        onclick: () => { location.hash = `#/software/${enc(item.project)}`; } },
      h('div', { class: 'name' }, item.project),
      h('div', { class: 'meta' }, item.latest_version ? `latest ${item.latest_version}` : 'not published yet'),
      item.environments.map((placement) => h('div', { class: 'placement' },
        state(placement.processes.every(([, process]) => process === 'running') && placement.converged ? 'running' : 'pending',
          `${placement.environment}${placement.version ? ` · ${placement.version}` : ''}`))))))
      : h('div', { class: 'panel empty' }, 'Nothing here yet. Run a project to start.'),
    h('h2', {}, 'Computers'),
    computers.length ? h('div', { class: 'cards' }, computers.map((environment) =>
      h('div', { class: 'panel card', role: 'link', tabindex: '0', 'data-computer-card': environment.name,
        onclick: () => { location.hash = `#/work/${enc(environment.name)}`; } },
      h('div', { class: 'name' }, environment.name),
      h('div', { class: 'meta' }, environment.target ? `on ${environment.target}` : 'placing…'),
      state(environment.computer))))
      : h('div', { class: 'panel empty' }, 'No computers yet.'),
  ];
}

// ---- Run a project ------------------------------------------------------------------
//
// Source → computer → Compute inspects the source inside the computer and
// proposes an assembly → the user adjusts it → what will happen → GO.

const RUN = { step: 'source', source: {}, computer: {}, proposal: null, environment: null, status: null };

function resetRun(preset) {
  Object.assign(RUN, { step: 'source', source: { url: '', revision: '', name: '' }, computer: preset, proposal: null, environment: null, status: null, services: {} });
}

async function runView() {
  const query = new URLSearchParams(location.hash.split('?')[1] || '');
  const mode = query.has('try') ? 'try' : (query.has('add') ? 'add' : 'run');
  if (RUN.mode !== mode) {
    RUN.mode = mode;
    resetRun(mode === 'try'
      ? { kind: 'new', name: `try-${Math.random().toString(16).slice(2, 8)}`, cpu: 1, memory: 1, lifetime: 'ephemeral' }
      : { kind: 'existing', name: '' });
  }
  const environments = (await api('GET', '/environments')).filter((environment) => environment.computer
    && !['destroyed', 'expired', 'destroying'].includes(environment.computer));
  if (RUN.computer.kind === 'existing' && !RUN.computer.name) {
    if (environments.length) RUN.computer.name = environments[0].name;
    else Object.assign(RUN.computer, { kind: 'new', name: 'my-computer', cpu: 1, memory: 1, lifetime: 'persistent' });
  }
  const software = await api('GET', '/software').catch(() => []);
  const field = (object, key, attributes) => h('input', { value: object[key] === undefined ? '' : String(object[key]), autocomplete: 'off',
    oninput: (event) => { object[key] = event.target.value; }, ...attributes });
  const title = mode === 'try' ? 'Try software' : (mode === 'add' ? 'Add another project' : 'Run a project');
  const sourceStep = h('div', { class: 'panel wizard-step draft', 'data-step': 'source' },
    h('h3', {}, '1 · What do you want to run?'),
    h('label', { for: 'run-url' }, 'Git repository or local folder'),
    field(RUN.source, 'url', { id: 'run-url', placeholder: 'https://github.com/you/app.git or /home/you/app' }),
    h('div', { class: 'row' },
      h('div', {}, h('label', { for: 'run-revision' }, 'Branch, tag, or commit (optional)'), field(RUN.source, 'revision', { id: 'run-revision', placeholder: 'default branch' })),
      h('div', {}, h('label', { for: 'run-name' }, 'Name (optional)'), field(RUN.source, 'name', { id: 'run-name', placeholder: 'from the repository' }))),
    software.some((item) => item.latest_version) ? h('div', { class: 'meta' },
      'Or run a published version: ',
      software.filter((item) => item.latest_version).map((item) =>
        h('a', { href: `#/software/${enc(item.project)}`, class: 'chip' }, `${item.project} ${item.latest_version}`))) : null);
  const computerStep = h('div', { class: 'panel wizard-step draft', 'data-step': 'computer' },
    h('h3', {}, '2 · On which computer?'),
    environments.length ? h('label', { class: 'check' },
      h('input', { type: 'radio', name: 'run-computer', checked: RUN.computer.kind === 'existing', 'data-choice': 'existing',
        onchange: () => { RUN.computer.kind = 'existing'; RUN.computer.name = environments[0].name; render(); } }),
      ' A computer I have: ',
      h('select', { id: 'run-existing', disabled: RUN.computer.kind !== 'existing', onchange: (event) => { RUN.computer.name = event.target.value; } },
        environments.map((environment) => h('option', { value: environment.name, selected: RUN.computer.name === environment.name }, `${environment.name} (${environment.computer})`)))) : null,
    h('label', { class: 'check' },
      h('input', { type: 'radio', name: 'run-computer', checked: RUN.computer.kind === 'new', 'data-choice': 'new',
        onchange: () => { Object.assign(RUN.computer, { kind: 'new', name: 'my-computer', cpu: 1, memory: 1, lifetime: mode === 'try' ? 'ephemeral' : 'persistent' }); render(); } }),
      ' A new computer'),
    RUN.computer.kind === 'new' ? h('div', { class: 'row' },
      h('div', {}, h('label', { for: 'run-computer-name' }, 'Name'), field(RUN.computer, 'name', { id: 'run-computer-name' })),
      h('div', {}, h('label', { for: 'run-cpu' }, 'CPUs'), field(RUN.computer, 'cpu', { id: 'run-cpu', type: 'number', min: '1' })),
      h('div', {}, h('label', { for: 'run-memory' }, 'Memory (GiB)'), field(RUN.computer, 'memory', { id: 'run-memory', type: 'number', min: '1' })),
      h('div', {}, h('label', { for: 'run-lifetime' }, 'Lifetime'),
        h('select', { id: 'run-lifetime', onchange: (event) => { RUN.computer.lifetime = event.target.value; } },
          h('option', { value: 'persistent', selected: RUN.computer.lifetime === 'persistent' }, 'Keep running'),
          h('option', { value: 'ephemeral', selected: RUN.computer.lifetime === 'ephemeral' }, 'Temporary (1 hour)')))) : null,
    h('div', { class: 'actions row' },
      h('button', { class: 'primary', 'data-inspect': 'true', disabled: RUN.step === 'inspecting', onclick: inspectSource }, RUN.step === 'inspecting' ? 'Inspecting…' : 'Inspect'),
      RUN.status ? h('span', { class: 'meta', 'data-run-status': 'true' }, RUN.status) : null));
  return [
    h('div', { class: 'crumbs' }, h('a', { href: '#/' }, 'Home'), ' / ', title),
    h('div', { class: 'title' }, h('h1', {}, title)),
    h('div', { class: 'subtitle' }, 'Compute looks at the source inside the computer and proposes how to run it. Nothing changes until GO.'),
    sourceStep,
    computerStep,
    RUN.proposal ? proposalStep() : null,
  ];
}

async function waitForComputer(name, until) {
  for (let attempt = 0; attempt < 600; attempt += 1) {
    const computer = await api('GET', `/environments/${enc(name)}/computer`).catch(() => null);
    if (computer) {
      RUN.status = `Computer ${name}: ${computer.status}${computer.target ? ` on ${computer.target}` : ''}`;
      const box = document.querySelector('[data-run-status]');
      if (box) box.textContent = RUN.status;
      if (until(computer)) return computer;
      if (computer.failure && !computer.failure.retryable) throw new Error(`${computer.failure.code}: ${computer.failure.message}`);
    }
    await new Promise((resolve) => setTimeout(resolve, 500));
  }
  throw new Error(`computer ${name} did not start`);
}

async function inspectSource() {
  if (!RUN.source.url.trim()) { toast('Choose a repository or folder first', true); return; }
  RUN.step = 'inspecting';
  RUN.proposal = null;
  await render();
  try {
    let name = RUN.computer.name;
    if (RUN.computer.kind === 'new') {
      name = RUN.computer.name.trim();
      const existing = await api('GET', `/environments/${enc(name)}`).catch(() => null);
      if (!existing) {
        RUN.status = `Creating computer ${name}…`;
        await render();
        const computer = { lifecycle: RUN.computer.lifetime, requirements: {
          cpu_count: Number(RUN.computer.cpu) || undefined,
          memory_bytes: Number(RUN.computer.memory) ? Math.round(Number(RUN.computer.memory) * 2 ** 30) : undefined,
        } };
        if (RUN.computer.lifetime === 'ephemeral') computer.ttl_seconds = 3600;
        await api('POST', '/environments', { name, computer });
      }
      RUN.computer = { kind: 'existing', name };
    }
    await waitForComputer(name, (computer) => computer.status === 'running');
    RUN.status = 'Inspecting the source in the computer…';
    await render();
    const body = { url: RUN.source.url.trim() };
    if (RUN.source.revision.trim()) body.revision = RUN.source.revision.trim();
    if (RUN.source.name.trim()) body.name = RUN.source.name.trim();
    RUN.proposal = await api('POST', `/environments/${enc(name)}/propose`, body);
    RUN.environment = name;
    RUN.services = Object.fromEntries((RUN.proposal.services || []).map((service) => [service.name, false]));
    RUN.status = null;
  } catch (error) {
    RUN.status = `Could not inspect: ${error.message}`;
  }
  RUN.step = 'source';
  await render();
}

function commandText(argv) {
  if (!argv || !argv.length) return '';
  return argv[0] === 'sh' && argv[1] === '-c' ? argv.slice(2).join(' ') : argv.join(' ');
}

function asCommand(text) {
  return text.trim() ? ['sh', '-c', text.trim()] : [];
}

function proposalStep() {
  const proposal = RUN.proposal;
  const assembly = proposal.assembly;
  const project = assembly.project;
  const edit = (object, key, command) => h('input', {
    value: command ? commandText(object[key]) : (object[key] === undefined ? '' : String(object[key])),
    autocomplete: 'off',
    oninput: (event) => { object[key] = command ? asCommand(event.target.value) : event.target.value; },
    onchange: () => render(),
  });
  const processes = assembly.processes || [];
  const happen = [
    `Check out ${assembly.repository.name} at ${assembly.repository.revision}`,
    ...(assembly.packages || []).map((item) => `Install ${commandText(item.install)}`),
    project.build && project.build.length ? `Build: ${commandText(project.build)}` : null,
    ...processes.map((process) => `Start ${process.kind || 'application'} ${process.name}${process.port ? ` on port ${process.port}` : ''}`),
    ...(proposal.services || []).filter((service) => RUN.services[service.name]).map((service) => `Start service ${service.name}`),
    Object.keys(proposal.config || {}).length ? `Set configuration: ${Object.keys(proposal.config).join(', ')}` : null,
  ].filter(Boolean);
  return h('div', { class: 'panel wizard-step draft', 'data-step': 'proposal' },
    h('h3', {}, `3 · Compute proposes${proposal.runtime ? ` (${proposal.runtime})` : ''}`),
    (proposal.notes || []).map((note) => h('div', { class: 'meta' }, note)),
    h('div', { class: 'row' },
      h('div', {}, h('label', {}, 'Dependencies'), (assembly.packages || []).length
        ? assembly.packages.map((item) => edit(item, 'install', true)) : h('div', { class: 'meta' }, 'none')),
      h('div', {}, h('label', {}, 'Build'), edit(project, 'build', true)),
      h('div', {}, h('label', {}, 'Tests'), edit(project, 'test', true))),
    processes.map((process) => h('div', { class: 'row', 'data-proposed-process': process.name },
      h('div', {}, h('label', {}, `Start ${process.name}`), edit(process, 'command', true)),
      h('div', { class: 'narrow' }, h('label', {}, 'Port'), h('input', { type: 'number', value: process.port || '', oninput: (event) => { process.port = Number(event.target.value) || undefined; }, onchange: () => render() })))),
    (proposal.services || []).length ? h('div', {}, h('label', {}, 'Services it appears to need'),
      proposal.services.map((service) => h('label', { class: 'check' },
        h('input', { type: 'checkbox', checked: RUN.services[service.name], onchange: (event) => { RUN.services[service.name] = event.target.checked; render(); } }),
        ` ${service.name}: ${commandText(service.command)}`))) : null,
    Object.keys(proposal.config || {}).length ? h('div', {}, h('label', {}, 'Configuration'),
      Object.keys(proposal.config).map((key) => h('div', { class: 'row' },
        h('div', { class: 'mono narrow' }, key),
        h('div', {}, h('input', { value: proposal.config[key], 'aria-label': `${key} value`, oninput: (event) => { proposal.config[key] = event.target.value; } }))))) : null,
    h('h3', {}, '4 · What will happen'),
    h('ul', { 'data-happen': 'true' }, happen.map((item) => h('li', {}, item))),
    h('div', { class: 'meta' }, `On computer ${RUN.environment}. The computer is changed in place; nothing is redeployed.`),
    h('div', { class: 'actions row' },
      h('button', { onclick: () => { RUN.proposal = null; render(); } }, 'Start over'),
      h('button', { class: 'primary go', 'data-go': 'true', onclick: runGo }, 'GO')));
}

async function runGo() {
  const name = RUN.environment;
  const proposal = RUN.proposal;
  try {
    const computer = await api('GET', `/environments/${enc(name)}/computer`);
    const contents = structuredClone(computer.desired);
    for (const field of LIST_FIELDS) contents[field] = contents[field] || [];
    const assembly = proposal.assembly;
    const upsert = (list, item) => { const index = list.findIndex((entry) => entry.name === item.name); if (index >= 0) list[index] = item; else list.push(item); };
    upsert(contents.repositories, assembly.repository);
    for (const item of assembly.packages || []) upsert(contents.packages, item);
    const project = { ...assembly.project };
    for (const key of ['build', 'test']) if (!project[key] || !project[key].length) delete project[key];
    upsert(contents.projects, project);
    for (const process of assembly.processes || []) upsert(contents.processes, process);
    for (const service of proposal.services || []) if (RUN.services[service.name]) upsert(contents.processes, service);
    const body = { contents, expected_generation: computer.desired.generation || 0 };
    if (Object.keys(proposal.config || {}).length) body.config = { ...(computer.config || {}), ...proposal.config };
    await api('POST', `/environments/${enc(name)}/contents`, body);
    toast(`GO: ${proposal.name} is being assembled on ${name}`);
    RUN.mode = null;
    location.hash = `#/work/${enc(name)}`;
  } catch (error) {
    toast(`GO failed: ${error.message}`, true);
  }
}

// ---- Software: projects, versions, rollouts --------------------------------------------

function glyphOf(status) {
  return { succeeded: ['ok', '✓'], skipped: ['idle', '–'], running: ['warn', '●'], failed: ['bad', '×'], pending: ['idle', '○'] }[status] || ['idle', '○'];
}

function stepList(steps) {
  return h('ol', { class: 'steps-list' }, steps.map((item) => {
    const [tone, glyph] = glyphOf(item.status);
    return h('li', { class: `step ${tone}`, 'data-step-name': item.name, 'data-step-status': item.status },
      h('span', { class: 'glyph' }, glyph), h('strong', {}, item.name),
      item.detail ? h('span', { class: 'meta' }, ` ${item.detail}`) : null,
      item.job_id ? h('span', { class: 'chip mono', title: item.execution_id || '' }, short(item.job_id)) : null);
  }));
}

async function softwareListView() {
  const software = await api('GET', '/software');
  return [
    h('div', { class: 'title' }, h('h1', {}, 'Software'),
      h('div', { class: 'actions' }, h('a', { class: 'button primary', href: '#/run' }, 'Run a project'))),
    h('div', { class: 'subtitle' }, 'Your projects, where they run, and at which version.'),
    table(['Project', 'Latest version', 'Runs in'], software.map((item) => h('tr', { class: 'link', 'data-software': item.project,
      onclick: () => { location.hash = `#/software/${enc(item.project)}`; } },
    h('td', {}, h('strong', {}, item.project)),
    h('td', {}, item.latest_version || '—'),
    h('td', {}, item.environments.map((placement) => h('span', { class: 'chip' }, `${placement.environment}${placement.version ? ` ${placement.version}` : ''}`))))),
    'No software yet. Run a project.'),
  ];
}

async function softwareView(project) {
  const view = await api('GET', `/software/${enc(project)}`);
  const published = view.versions.filter((version) => version.status === 'published');
  const current = (environment) => view.rollouts.find((rollout) => rollout.environment === environment && rollout.status === 'active');
  const rows = view.environments.map((placement) => h('tr', { 'data-placement': placement.environment },
    h('td', {}, h('a', { href: `#/environments/${enc(placement.environment)}` }, h('strong', {}, placement.environment))),
    h('td', {}, placement.version || h('span', { class: 'meta' }, 'unversioned')),
    h('td', { class: 'mono' }, revisionText(placement.revision, placement.commit)),
    h('td', {}, placement.processes.map(([name, process]) => state(process, `${name} ${process}`))),
    h('td', {},
      h('button', { 'data-run': `${placement.environment}/build`, onclick: () => runProjectOperation(placement.environment, project, 'build') }, 'Build'),
      h('button', { 'data-run': `${placement.environment}/test`, onclick: () => runProjectOperation(placement.environment, project, 'test') }, 'Test'),
      h('a', { class: 'button', href: `#/work/${enc(placement.environment)}` }, 'Work'))));
  const versions = view.versions.map((version) => h('tr', { class: 'link', 'data-version': version.version,
    onclick: () => { location.hash = `#/software/${enc(project)}/versions/${enc(version.version)}`; } },
  h('td', {}, h('strong', {}, version.version)),
  h('td', {}, state(version.status === 'published' ? 'complete' : (version.status === 'failed' ? 'failed' : 'pending'), version.status)),
  h('td', { class: 'mono' }, short(version.commit)),
  h('td', {}, view.rollouts.filter((rollout) => rollout.version === version.version && rollout.status === 'active').map((rollout) => h('span', { class: 'chip' }, `● ${rollout.environment}`))),
  h('td', {}, version.created_by),
  h('td', {}, ago(version.created_at))));
  const rollouts = view.rollouts.map((rollout) => h('tr', { class: 'link', 'data-rollout': rollout.rollout_id,
    onclick: () => { location.hash = `#/operations/rollout/${enc(rollout.rollout_id)}`; } },
  h('td', {}, rollout.kind), h('td', {}, rollout.version), h('td', {}, rollout.environment),
  h('td', {}, state(rollout.status === 'applying' ? 'deploying' : (rollout.status === 'superseded' ? 'superseded' : rollout.status))),
  h('td', {}, rollout.created_by), h('td', {}, ago(rollout.created_at))));
  return [
    h('div', { class: 'crumbs' }, h('a', { href: '#/software' }, 'Software'), ' / ', project),
    h('div', { class: 'title', 'data-software-view': project }, h('h1', {}, project),
      view.latest_version ? h('span', { class: 'chip' }, `latest ${view.latest_version}`) : null,
      h('div', { class: 'actions' },
        h('button', { class: 'primary', 'data-publish': 'true', onclick: () => publishDialog(project, view) }, 'Publish new version'),
        h('button', { 'data-deploy': 'true', disabled: !published.length, onclick: () => deployDialog(project, view) }, 'Deploy'),
        h('button', { 'data-promote': 'true', disabled: view.rollouts.every((rollout) => rollout.status !== 'active'), onclick: () => promoteVersionDialog(project, view) }, 'Promote'),
        h('button', { 'data-rollback': 'true', disabled: published.length < 2, onclick: () => rollbackDialog(project, view, current) }, 'Roll back'))),
    h('div', { class: 'subtitle' }, `Next version: ${view.next_version}`),
    h('h2', {}, 'Where it runs'),
    table(['Environment', 'Version', 'Revision', 'Running', ''], rows, 'It does not run anywhere yet.'),
    OUTPUT.get(`software:${project}`) ? outputPanel(OUTPUT.get(`software:${project}`)) : null,
    h('h2', {}, 'Versions'),
    table(['Version', 'Status', 'Commit', 'Running in', 'By', 'When'], versions, 'No versions yet. Publish one.'),
    h('h2', {}, 'Deployments'),
    table(['Kind', 'Version', 'Environment', 'Status', 'By', 'When'], rollouts, 'Nothing deployed yet.'),
  ];
}

/// A revision and the commit it resolved to, once when they are the same.
function revisionText(revision, commit) {
  if (!revision) return commit ? short(commit) : '—';
  if (commit && commit.startsWith(revision)) return short(commit);
  return `${revision.length > 16 ? short(revision) : revision}${commit ? ` · ${short(commit)}` : ''}`;
}

function outputPanel(output) {
  return h('div', { class: 'panel terminal', 'data-output': output.status },
    h('div', { class: 'meta' }, `${output.title} · ${output.status}${output.job ? ` · job ${output.job}` : ''}`),
    h('pre', { class: 'mono' }, output.text || ''));
}

async function runProjectOperation(environment, project, command) {
  try {
    const submitted = await api('POST', `/environments/${enc(environment)}/run`, { project, command });
    const key = `software:${project}`;
    OUTPUT.set(key, { title: `${project} ${command} in ${environment}`, status: 'running', job: submitted.job_id, text: '' });
    render();
    for (let delay = 100; ; delay = Math.min(delay * 2, 1000)) {
      const job = await api('GET', `/environments/${enc(environment)}/jobs/${enc(submitted.job_id)}`);
      if (job.result || ['succeeded', 'failed', 'cancelled', 'timed_out', 'rejected'].includes(job.job.status)) {
        const result = job.result && job.result.result;
        OUTPUT.set(key, { title: `${project} ${command} in ${environment}`, status: job.job.status, job: submitted.job_id, text: result ? `${result.stdout.text}${result.stderr.text}` : (job.job.failure || '') });
        render();
        return;
      }
      await new Promise((resolve) => setTimeout(resolve, delay));
    }
  } catch (error) {
    toast(`${project} ${command} failed: ${error.message}`, true);
  }
}

function reviewDialog(title, body, go) {
  modal(title, body, [
    h('button', { onclick: close }, 'Cancel'),
    h('button', { class: 'primary go', 'data-go': 'true', onclick: async () => { close(); await go(); } }, 'GO'),
  ]);
}

async function publishDialog(project, view) {
  const environments = view.environments.map((placement) => placement.environment);
  if (!environments.length) { toast(`${project} runs nowhere to publish from`, true); return; }
  const source = h('select', { id: 'publish-environment' }, environments.map((name) => h('option', { value: name }, name)));
  const label = h('input', { id: 'publish-version', value: view.next_version, autocomplete: 'off' });
  const computer = await api('GET', `/environments/${enc(environments[0])}/computer`);
  const spec = (computer.desired.projects || []).find((item) => item.name === project) || {};
  reviewDialog(`Publish ${project}`, h('div', {},
    h('label', { for: 'publish-environment' }, 'From'), source,
    h('label', { for: 'publish-version' }, 'Version'), label,
    h('p', {}, 'Compute will, in that environment\'s computer:'),
    h('ul', {},
      h('li', {}, 'Record the exact commit'),
      h('li', {}, spec.build && spec.build.length ? `Build: ${commandText(spec.build)}` : 'Build: none'),
      h('li', {}, spec.test && spec.test.length ? `Tests: ${commandText(spec.test)}` : 'Tests: none'),
      h('li', {}, (spec.checks || []).length ? `Checks: ${spec.checks.join(', ')}` : 'Checks: none'),
      h('li', {}, 'Package the source and record its digest'),
      h('li', {}, 'Publish the version: immutable, with its evidence'))), async () => {
    const version = await act(`Publishing ${project} ${label.value}`, () => api('POST', `/software/${enc(project)}/versions`, { environment: source.value, version: label.value }));
    if (version) location.hash = `#/operations/version/${enc(project)}/${enc(version.version)}`;
  });
}

async function deployDialog(project, view, preset) {
  const environments = (await api('GET', '/environments')).filter((environment) => environment.computer && !['destroyed', 'expired'].includes(environment.computer));
  const published = view.versions.filter((version) => version.status === 'published');
  const version = h('select', { id: 'deploy-version' }, published.map((item) => h('option', { value: item.version, selected: preset && preset.version === item.version }, item.version)));
  const target = h('select', { id: 'deploy-environment' }, environments.map((environment) => h('option', { value: environment.name, selected: preset && preset.environment === environment.name }, environment.name)));
  const review = h('div', { 'data-review': 'true' });
  const refresh = async () => {
    const computer = await api('GET', `/environments/${enc(target.value)}/computer`);
    const chosen = published.find((item) => item.version === version.value);
    const running = view.rollouts.find((rollout) => rollout.environment === target.value && rollout.status === 'active');
    const has = (computer.desired.repositories || []).find((item) => item.name === (chosen.assembly.repository || {}).name);
    review.replaceChildren(h('ul', {},
      h('li', {}, `Now in ${target.value}: ${running ? running.version : 'no version'}`),
      h('li', {}, has ? `Move ${has.name} ${has.revision.length > 16 ? short(has.revision) : has.revision} → ${short(chosen.commit)}` : `Add ${project} at ${short(chosen.commit)}`),
      (chosen.assembly.processes || []).map((process) => h('li', {}, `${(computer.desired.processes || []).some((item) => item.name === process.name) ? 'Restart' : 'Start'} ${process.name}`)),
      h('li', {}, `On ${target.value}'s computer, in place: no new machine`)));
  };
  version.onchange = refresh;
  target.onchange = refresh;
  reviewDialog(`Deploy ${project}`, h('div', {},
    h('label', { for: 'deploy-version' }, 'Version'), version,
    h('label', { for: 'deploy-environment' }, 'To'), target, review), async () => {
    const rollout = await act(`Deploying ${project} ${version.value} to ${target.value}`, () => api('POST', `/software/${enc(project)}/deploy`, { environment: target.value, version: version.value }));
    if (rollout) location.hash = `#/operations/rollout/${enc(rollout.rollout_id)}`;
  });
  await refresh();
}

async function promoteVersionDialog(project, view) {
  const environments = (await api('GET', '/environments')).filter((environment) => environment.computer && !['destroyed', 'expired'].includes(environment.computer));
  const sources = [...new Set(view.rollouts.filter((rollout) => rollout.status === 'active').map((rollout) => rollout.environment))];
  const from = h('select', { id: 'promote-from' }, sources.map((name) => h('option', { value: name, selected: /test|stag/.test(name) }, name)));
  const to = h('select', { id: 'promote-to' }, environments.map((environment) => h('option', { value: environment.name, selected: /prod/.test(environment.name) }, environment.name)));
  const review = h('div', { 'data-review': 'true' });
  let plan = null;
  const refresh = async () => {
    try {
      plan = await api('GET', `/software/${enc(project)}/promotion?from=${enc(from.value)}&to=${enc(to.value)}`);
      review.replaceChildren(
        h('div', { class: 'promotion' },
          h('div', { class: 'panel fact' }, h('div', { class: 'label' }, from.value), h('div', { class: 'value' }, `Version ${plan.version}`), state(plan.from_healthy ? 'healthy' : 'unhealthy')),
          h('div', { class: 'arrow' }, '→'),
          h('div', { class: 'panel fact' }, h('div', { class: 'label' }, to.value), h('div', { class: 'value' }, plan.to_current ? `Version ${plan.to_current}` : 'Nothing yet'))),
        h('div', {}, 'What will change:'), h('ul', {}, plan.changes.map((change) => h('li', {}, change))),
        h('div', {}, 'Configuration:'), h('ul', {},
          plan.config_different.length ? h('li', {}, `Set differently: ${plan.config_different.join(', ')}`) : null,
          plan.config_only_in_from.length ? h('li', {}, `Only in ${from.value}: ${plan.config_only_in_from.join(', ')}`) : null,
          plan.config_only_in_to.length ? h('li', {}, `Only in ${to.value}: ${plan.config_only_in_to.join(', ')}`) : null,
          !plan.config_different.length && !plan.config_only_in_from.length && !plan.config_only_in_to.length ? h('li', {}, 'The same keys, set the same way') : null),
        h('div', { class: 'meta' }, `Authority: ${plan.authority}. Approvals required: ${plan.approvals.length ? plan.approvals.join(', ') : 'none'}.`));
    } catch (error) {
      plan = null;
      review.replaceChildren(h('div', { class: 'error' }, error.message));
    }
  };
  from.onchange = refresh;
  to.onchange = refresh;
  reviewDialog(`Promote ${project}`, h('div', {},
    h('label', { for: 'promote-from' }, 'From'), from,
    h('label', { for: 'promote-to' }, 'To'), to, review), async () => {
    if (!plan) return;
    const rollout = await act(`Promoting ${project} ${plan.version} to ${to.value}`, () => api('POST', `/software/${enc(project)}/promote`, { from: from.value, to: to.value, expected_generation: plan.expected_generation }));
    if (rollout) location.hash = `#/operations/rollout/${enc(rollout.rollout_id)}`;
  });
  await refresh();
}

async function rollbackDialog(project, view, current) {
  const places = [...new Set(view.rollouts.filter((rollout) => rollout.status === 'active').map((rollout) => rollout.environment))];
  const environment = h('select', { id: 'rollback-environment' }, places.map((name) => h('option', { value: name, selected: /prod/.test(name) }, name)));
  const version = h('select', { id: 'rollback-to' });
  const review = h('div', { 'data-review': 'true' });
  const refresh = () => {
    const active = current(environment.value);
    const choices = view.versions.filter((item) => item.status === 'published' && (!active || item.version !== active.version));
    version.replaceChildren(...choices.map((item) => h('option', { value: item.version, selected: active && item.version === active.previous_version }, item.version)));
    review.replaceChildren(h('ul', {},
      h('li', {}, `${environment.value} runs ${active ? active.version : 'no version'} now`),
      h('li', {}, `It will run ${version.value || '—'}: checked out, built, and restarted in place`),
      h('li', {}, 'The current version stays in the history')));
  };
  environment.onchange = refresh;
  version.onchange = refresh;
  refresh();
  reviewDialog(`Roll back ${project}`, h('div', {},
    h('label', { for: 'rollback-environment' }, 'Environment'), environment,
    h('label', { for: 'rollback-to' }, 'Version'), version, review), async () => {
    const rollout = await act(`Rolling ${project} back to ${version.value}`, () => api('POST', `/software/${enc(project)}/rollback`, { environment: environment.value, version: version.value }));
    if (rollout) location.hash = `#/operations/rollout/${enc(rollout.rollout_id)}`;
  });
}

async function versionView(project, label) {
  const version = await api('GET', `/software/${enc(project)}/versions/${enc(label)}`);
  const rollouts = await api('GET', `/rollouts?project=${enc(project)}`);
  const assembly = version.assembly || {};
  return [
    h('div', { class: 'crumbs' }, h('a', { href: '#/software' }, 'Software'), ' / ', h('a', { href: `#/software/${enc(project)}` }, project), ' / ', label),
    h('div', { class: 'title', 'data-version-view': label }, h('h1', {}, `${project} ${label}`), state(version.status === 'published' ? 'complete' : (version.status === 'failed' ? 'failed' : 'pending'), version.status),
      h('div', { class: 'actions' }, version.status === 'published'
        ? h('button', { class: 'primary', onclick: async () => deployDialog(project, await api('GET', `/software/${enc(project)}`), { version: label }) }, 'Deploy this version') : null)),
    h('div', { class: 'grid' },
      fact('Source', `${(assembly.repository || {}).url || '—'}`, true),
      fact('Commit', version.commit || '—', true),
      fact('Package', version.package_digest || '—', true),
      fact('Published from', `${version.environment} by ${version.created_by}`),
      fact('When', new Date(version.created_at).toLocaleString()),
      fact('Configuration keys', (version.config_keys || []).join(', ') || 'none')),
    h('h2', {}, 'Evidence'),
    stepList(version.steps),
    version.failure ? h('div', { class: 'panel error' }, version.failure) : null,
    h('h2', {}, 'Environments'),
    table(['Kind', 'Environment', 'Status', 'By', 'When'], rollouts.filter((rollout) => rollout.version === label).map((rollout) => h('tr', { class: 'link', onclick: () => { location.hash = `#/operations/rollout/${enc(rollout.rollout_id)}`; } },
      h('td', {}, rollout.kind), h('td', {}, rollout.environment), h('td', {}, state(rollout.status === 'applying' ? 'deploying' : rollout.status)), h('td', {}, rollout.created_by), h('td', {}, ago(rollout.created_at)))), 'Not deployed anywhere.'),
  ];
}

let operationTimer = null;
function pollWhile(inFlight) {
  clearTimeout(operationTimer);
  if (inFlight) operationTimer = setTimeout(() => { if (!dialog.open) render(); }, 700);
}

async function jobOutput(environment, job) {
  try {
    const result = await api('GET', `/environments/${enc(environment)}/jobs/${enc(job)}`);
    const output = result.result && result.result.result;
    showOutput(`Job ${job}`, output ? output.stdout.text : '', output ? output.stderr.text : (result.job.failure || ''));
  } catch (error) {
    toast(`Could not read job ${job}: ${error.message}`, true);
  }
}

async function operationView(kind, first, second) {
  if (kind === 'version') {
    const version = await api('GET', `/software/${enc(first)}/versions/${enc(second)}`);
    const failed = version.steps.find((item) => item.status === 'failed');
    pollWhile(version.status === 'publishing');
    return [
      h('div', { class: 'crumbs' }, h('a', { href: `#/software/${enc(first)}` }, first), ' / publish'),
      h('div', { class: 'title', 'data-operation': version.status }, h('h1', {}, `Publishing ${first} ${second}`),
        state(version.status === 'published' ? 'complete' : (version.status === 'failed' ? 'failed' : 'deploying'), version.status)),
      h('div', { class: 'subtitle' }, `In ${version.environment}'s computer`),
      stepList(version.steps),
      version.failure ? h('div', { class: 'panel error' }, version.failure) : null,
      h('div', { class: 'actions row' },
        failed && failed.job_id ? h('button', { onclick: () => jobOutput(version.environment, failed.job_id) }, 'Inspect failure') : null,
        version.status === 'failed' ? h('a', { class: 'button', href: `#/work/${enc(version.environment)}` }, 'Fix it in Work') : null,
        version.status === 'published' ? h('a', { class: 'button primary', href: `#/software/${enc(first)}` }, 'Deploy it') : null),
    ];
  }
  const rollout = await api('GET', `/rollouts/${enc(first)}`);
  const failed = rollout.steps.find((item) => item.status === 'failed');
  pollWhile(rollout.status === 'applying');
  const verb = { deploy: 'Deploying', promote: 'Promoting', rollback: 'Rolling back' }[rollout.kind];
  const computer = await api('GET', `/environments/${enc(rollout.environment)}/computer`).catch(() => null);
  return [
    h('div', { class: 'crumbs' }, h('a', { href: `#/software/${enc(rollout.project)}` }, rollout.project), ` / ${rollout.kind}`),
    h('div', { class: 'title', 'data-operation': rollout.status }, h('h1', {}, `${verb} ${rollout.project} ${rollout.version}`),
      state(rollout.status === 'active' ? 'running' : (rollout.status === 'failed' ? 'failed' : 'deploying'), rollout.status)),
    h('div', { class: 'subtitle' }, `${rollout.from_environment ? `${rollout.from_environment} → ` : ''}${rollout.environment}${rollout.previous_version ? ` · replacing ${rollout.previous_version}` : ''} · by ${rollout.created_by}`),
    stepList(rollout.steps),
    rollout.failure ? h('div', { class: 'panel error' }, rollout.failure) : null,
    computer && rollout.status === 'active' ? [h('h2', {}, 'Running now'), h('div', { class: 'grid' },
      Object.entries(computer.observed.processes || {}).map(([name, process]) => {
        const endpoint = (computer.endpoints || []).find((item) => item.process === name);
        return fact(name, [state(process.state), endpoint && endpoint.url ? [' · ', h('a', { href: endpoint.url, target: '_blank', rel: 'noopener' }, endpoint.url)] : null]);
      }))] : null,
    h('div', { class: 'actions row' },
      failed && failed.job_id ? h('button', { onclick: () => jobOutput(rollout.environment, failed.job_id) }, 'Inspect failure') : null,
      rollout.status === 'failed' ? h('button', { onclick: async () => {
        const again = await act('Retrying', () => api('POST', `/software/${enc(rollout.project)}/deploy`, { environment: rollout.environment, version: rollout.version }));
        if (again) location.hash = `#/operations/rollout/${enc(again.rollout_id)}`;
      } }, 'Retry') : null,
      h('a', { class: 'button', href: `#/environments/${enc(rollout.environment)}` }, `Operate ${rollout.environment}`),
      h('a', { class: 'button', href: `#/work/${enc(rollout.environment)}` }, 'Open in Work')),
  ];
}

// ---- Work additions: progress, files, templates -------------------------------------

/// What Compute is doing to make the computer match the environment, item by
/// item: done, in progress, waiting.
function reconcileProgress(computer) {
  const desired = computer.desired || {};
  const observed = computer.observed || {};
  const items = [];
  for (const repository of desired.repositories || []) {
    const seen = (observed.repositories || {})[repository.name];
    items.push([`Check out ${repository.name}`, seen && seen.revision === repository.revision ? seen.evidence.outcome : null]);
  }
  for (const item of desired.packages || []) {
    const seen = (observed.packages || {})[item.name];
    items.push([`Install ${item.name}`, seen ? seen.evidence.outcome : null]);
  }
  for (const project of desired.projects || []) {
    if (!project.build || !project.build.length) continue;
    const seen = (observed.builds || {})[project.name];
    const repository = (observed.repositories || {})[project.repository];
    items.push([`Build ${project.name}`, seen && repository && seen.commit === repository.commit ? seen.evidence.outcome : null]);
  }
  for (const process of desired.processes || []) {
    if ((process.desired || 'running') !== 'running') continue;
    const seen = (observed.processes || {})[process.name];
    items.push([`Start ${process.name}`, seen ? (seen.state === 'running' ? 'succeeded' : (seen.state === 'failed' ? 'failed' : null)) : null]);
  }
  let current = false;
  return items.map(([label, outcome]) => {
    let status = outcome === 'succeeded' ? 'succeeded' : (outcome === 'failed' ? 'failed' : 'pending');
    if (status === 'pending' && !current && !computer.converged) { status = 'running'; current = true; }
    return { name: label, status };
  });
}

async function listFiles(name, path) {
  try {
    const submitted = await api('POST', `/environments/${enc(name)}/exec`, { command: ['ls', '-la', '--', path || '.'] });
    await followJob(name, `Files: ${path || '.'}`, submitted);
  } catch (error) {
    toast(`Files failed: ${error.message}`, true);
  }
}

async function viewFile(name, path) {
  try {
    const submitted = await api('POST', `/environments/${enc(name)}/exec`, { command: ['head', '-c', '65536', '--', path] });
    await followJob(name, `File: ${path}`, submitted);
  } catch (error) {
    toast(`File failed: ${error.message}`, true);
  }
}

const TEMPLATES = {
  database: { name: 'database', kind: 'service', command: ['sh', '-c', 'mkdir -p .compute/postgres && (test -f .compute/postgres/PG_VERSION || initdb -D .compute/postgres) && exec postgres -D .compute/postgres -p $PORT'], port: 5432 },
  redis: { name: 'redis', kind: 'service', command: ['sh', '-c', 'exec redis-server --port $PORT'], port: 6379 },
  agent: { name: 'agent', kind: 'agent', command: ['sh', '-c', 'exec ./agent'] },
};

const TABS = ['Overview', 'Workloads', 'Deployments', 'Logs', 'Resources', 'Configuration', 'Receipts', 'Events'];

async function projectView(environment, name, tab) {
  const project = await api('GET', `/environments/${enc(environment)}/projects/${enc(name)}`);
  // A project in `applications` is an application: it is shown as one,
  // with its versions, active version, and endpoint.
  const application = environment === 'applications'
    ? await api('GET', `/applications/${enc(name)}`).catch(() => null) : null;
  const base = `#/environments/${enc(environment)}/projects/${enc(name)}`;
  const active = TABS.includes(tab) ? tab : 'Overview';
  if (application) {
    return [
      h('div', { class: 'crumbs' }, h('a', { href: '#/' }, 'Environments'), ' / ', h('a', { href: `#/environments/${enc(environment)}` }, 'Applications'), ' / ', name),
      h('div', { class: 'title' }, h('h1', {}, name), state(application.status),
        application.active ? h('span', { class: 'chip', 'data-version': String(application.active.version) }, `v${application.active.version}`) : null,
        h('div', { class: 'actions' },
          h('button', { onclick: () => { location.hash = `${base}/logs`; } }, 'Logs'),
          h('button', { onclick: () => rollbackApplication(name, application) }, 'Rollback'),
          h('button', { class: 'danger', onclick: () => stopApplication(name) }, 'Stop'))),
      h('div', { class: 'tabs', role: 'tablist' }, TABS.map((item) => h('button', {
        role: 'tab', class: item === active ? 'active' : '', 'aria-selected': String(item === active),
        onclick: () => { location.hash = `${base}/${item.toLowerCase()}`; },
      }, item))),
      await projectTab(environment, name, project, active, application),
    ];
  }
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

/// What a project's workloads require of their node, and what is enforced.
function resourceSummary(workloads) {
  const sum = (field) => workloads.map((workload) => workload.resources[field] || 0).reduce((a, b) => a + b, 0);
  const cpu = sum('cpu_required');
  const memory = sum('memory_required_bytes');
  const limit = sum('memory_limit_bytes');
  const required = [cpu ? `${cpu} CPU` : null, memory ? bytes(memory) : null].filter(Boolean).join(' · ');
  return [
    required ? `Requires ${required}` : 'No requirements declared',
    limit ? ` · memory limit ${bytes(limit)}` : ' · not enforced by process runtimes',
  ].join('');
}

/// The version rows of an application, newest first.
function versionRows(application) {
  return application.deployments.map((deployment) => h('tr', {
    class: 'link', 'data-deployment': deployment.deployment_id, 'data-state': deployment.state,
    onclick: () => { location.hash = `#/deployments/${enc(deployment.deployment_id)}`; },
  },
  h('td', {}, h('strong', {}, `v${deployment.version}`), deployment.active ? h('span', { class: 'chip' }, 'active') : null),
  h('td', {}, state(deployment.state)),
  h('td', {}, deployment.rollback_of ? `Rollback to v${deployment.rollback_of}` : (deployment.failure ? h('div', { class: 'error' }, deployment.failure) : '—')),
  h('td', {}, [deployment.runtime, deployment.runtime_version].filter(Boolean).join(' ') || '—'),
  h('td', {}, ago(deployment.created_at)),
  h('td', { class: 'mono' }, short(deployment.deployment_id))));
}

async function rollbackApplication(name, application) {
  const targets = application.deployments.filter((deployment) => !deployment.active && !['failed', 'rolled_back'].includes(deployment.state));
  if (!targets.length) { toast('No earlier version to roll back to', true); return; }
  const select = h('select', { id: 'rollback-version' }, targets.map((deployment) => h('option', { value: `v${deployment.version}` }, `v${deployment.version} · ${ago(deployment.created_at)}`)));
  modal(`Roll back ${name}`, h('div', {},
    h('p', {}, 'The chosen version\'s code is deployed again as the next version. History is not edited.'),
    h('label', { for: 'rollback-version' }, 'Version'), select), [
    h('button', { onclick: close }, 'Cancel'),
    h('button', { class: 'danger', onclick: async () => {
      close();
      await act(`Rolling ${name} back to ${select.value}`, () => api('POST', `/applications/${enc(name)}/rollback`, { target: select.value }));
    } }, 'Roll back'),
  ]);
}

async function stopApplication(name) {
  if (await confirmImpact(`Stop ${name}?`, [`${name} stops serving; its endpoint stays reserved`], ['Its versions and evidence', 'Other applications'], 'danger')) {
    await act(`Stopping ${name}`, () => api('POST', `/applications/${enc(name)}/stop`));
  }
}

async function projectTab(environment, name, project, tab, application) {
  const services = project.workloads.filter((workload) => workload.kind === 'service');
  switch (tab) {
    case 'Overview': {
      if (application) {
        const current = application.active || application.deploying;
        return h('div', {},
          h('div', { class: 'grid' },
            fact('Status', state(application.status)),
            fact('Version', current ? `v${current.version}` : '—'),
            fact('Endpoint', application.endpoint ? h('a', { href: application.endpoint, 'data-endpoint': application.endpoint }, application.endpoint) : '—'),
            fact('Provider', application.node, true),
            fact('Runtime', current ? [current.runtime, current.runtime_version].filter(Boolean).join(' ') : '—'),
            fact('Resources', resourceSummary(project.workloads))),
          h('h2', {}, 'Deployments'),
          table(['Version', 'State', 'Note', 'Runtime', 'Created', 'Deployment'], versionRows(application), 'No deployments.'));
      }
      const ports = project.workloads.flatMap((workload) => workload.ports.map((port) => `${workload.name} ${port.name}: endpoint port ${port.host}`));
      const evidence = project.workloads.find((workload) => workload.evidence.admission_id) || { evidence: {} };
      return h('div', {},
        h('div', { class: 'grid' },
          fact('Revision', project.revision || '—', true),
          fact('Desired state', project.desired_state),
          fact('Actual state', status(project)),
          fact('Latest deployment', project.deployment ? [project.deployment.revision, ' · ', state(project.deployment.status), ' · ', ago(project.deployment.created_at)] : '—'),
          fact('Resources', resourceSummary(project.workloads)),
          fact('Network', ports.length ? ports.join('\n') : 'No ports'),
          fact('Admission', [h('div', {}, `policy ${short(evidence.evidence.policy_id)}`), h('div', {}, `admission ${short(evidence.evidence.admission_id)}`)], true),
          fact('Revision digest', short(project.revision_digest), true)),
        h('h2', {}, 'Services'),
        table(['Service', 'Status', 'Endpoint ports'], services.map((workload) => h('tr', {},
          h('td', {}, workload.name), h('td', {}, workload.actual_state === 'running' ? state(workload.health) : state(workload.actual_state)),
          h('td', { class: 'mono' }, workload.ports.map((port) => `${port.name} ${port.host}`).join(', ') || '—'))), 'No services.'));
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
      if (application) {
        return table(['Version', 'State', 'Note', 'Runtime', 'Created', 'Deployment'], versionRows(application), 'No deployments.');
      }
      const deployments = await api('GET', `/deployments?environment=${enc(environment)}&project=${enc(name)}&limit=50`);
      const current = project.deployment && project.deployment.deployment_id;
      // A release that completed and is no longer current was replaced.
      const shown = (deployment) => (deployment.status === 'complete' && deployment.deployment_id !== current ? 'superseded' : deployment.status);
      return table(['Version', 'Deployment', 'Revision', 'Status', 'Admission', 'Created', 'Promoted from'], deployments.map((deployment) => h('tr', {
        class: 'link', 'data-deployment': deployment.deployment_id,
        onclick: () => { location.hash = `#/deployments/${enc(deployment.deployment_id)}`; },
      },
        h('td', {}, h('strong', {}, `v${deployment.version}`), deployment.deployment_id === current ? h('span', { class: 'chip' }, 'active') : null),
        h('td', { class: 'mono' }, deployment.deployment_id),
        h('td', { class: 'mono' }, deployment.revision),
        h('td', {}, state(shown(deployment)), (deployment.failure || deployment.rollback_reason) ? h('div', { class: 'error' }, deployment.failure || deployment.rollback_reason) : null),
        h('td', {}, deployment.workloads.map((workload) => h('div', { class: 'mono' }, `${workload.name}: ${workload.admitted ? 'admitted' : 'denied'} ${short(workload.admission_id)}`))),
        h('td', {}, ago(deployment.created_at)),
        h('td', { class: 'mono' }, deployment.promoted_from || '—'))), 'No deployments.');
    }
    case 'Logs': {
      if (application) {
        const logs = await api('GET', `/applications/${enc(name)}/logs`);
        return h('div', {}, h('pre', { class: 'log' }, (logs.stdout || '') + (logs.stderr ? `\n${logs.stderr}` : '') || '(no output yet)'));
      }
      const panes = [];
      for (const workload of project.workloads) {
        const logs = await api('GET', `/environments/${enc(environment)}/projects/${enc(name)}/workloads/${enc(workload.name)}/logs`);
        panes.push(h('h2', {}, workload.name), h('pre', { class: 'log' }, (logs.stdout || '') + (logs.stderr ? `\n${logs.stderr}` : '') || '(no output yet)'));
      }
      return h('div', {}, panes);
    }
    case 'Resources':
      return table(['Workload', 'CPU required', 'Memory required', 'Memory limit', 'Timeout', 'Disk (logs)', 'Network'], project.workloads.map((workload) => h('tr', {},
        h('td', {}, workload.name),
        h('td', {}, workload.resources.cpu_required ? String(workload.resources.cpu_required) : '—'),
        h('td', {}, workload.resources.memory_required_bytes ? bytes(workload.resources.memory_required_bytes) : '—'),
        h('td', {}, workload.resources.memory_limit_bytes ? bytes(workload.resources.memory_limit_bytes) : 'not enforced'),
        h('td', {}, workload.resources.timeout_ms ? `${workload.resources.timeout_ms} ms` : 'none'),
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

/// The mode the operator was last in, for pages that belong to both.
function lastMode() {
  try { return sessionStorage.getItem('compute.mode') || 'work'; } catch { return 'work'; }
}

function route() {
  const path = location.hash.split('?')[0];
  const parts = path.replace(/^#\/?/, '').split('/').filter(Boolean).map(decodeURIComponent);
  if (!parts.length) return { mode: lastMode(), nav: 'home', render: homeView };
  if (parts[0] === 'run') return { mode: lastMode(), nav: 'home', render: runView };
  if (parts[0] === 'software' && parts[2] === 'versions' && parts[3]) return { mode: 'manage', nav: 'software', render: () => versionView(parts[1], parts[3]) };
  if (parts[0] === 'software' && parts[1]) return { mode: lastMode(), nav: 'software', render: () => softwareView(parts[1]) };
  if (parts[0] === 'software') return { mode: lastMode(), nav: 'software', render: softwareListView };
  if (parts[0] === 'operations' && parts[1] === 'version') return { mode: 'manage', nav: 'software', render: () => operationView('version', parts[2], parts[3]) };
  if (parts[0] === 'operations' && parts[1] === 'rollout') return { mode: 'manage', nav: 'software', render: () => operationView('rollout', parts[2]) };
  if (parts[0] === 'environments' && !parts[1]) return { mode: 'manage', nav: 'environments', render: environmentsView };
  if (parts[0] === 'work' && parts[1]) return { mode: 'work', environment: parts[1], nav: 'work', render: () => workView(parts[1]) };
  if (parts[0] === 'work') return { mode: 'work', nav: 'work', render: workHomeView };
  if (parts[0] === 'environments' && parts[1] && !parts[2]) return { mode: 'manage', environment: parts[1], nav: 'environments', render: () => environmentView(parts[1]) };
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
  if (parts[0] === 'events') return { mode: 'manage', nav: 'events', render: eventsView };
  return { mode: lastMode(), nav: 'home', render: homeView };
}

let rendering = null;
async function render() {
  const { nav, render: renderView, mode = 'manage', environment } = route();
  try { sessionStorage.setItem('compute.mode', mode); } catch { /* storage unavailable */ }
  clearTimeout(operationTimer);
  for (const link of document.querySelectorAll('[data-nav]')) link.classList.toggle('active', link.dataset.nav === nav);
  // One control plane, two modes of the same environment: switching keeps it.
  document.body.dataset.mode = mode;
  for (const link of document.querySelectorAll('[data-mode-link]')) {
    const target = link.dataset.modeLink;
    link.classList.toggle('active', target === mode);
    link.setAttribute('aria-selected', String(target === mode));
    link.setAttribute('href', target === 'work'
      ? (environment ? `#/work/${enc(environment)}` : '#/work')
      : (environment ? `#/environments/${enc(environment)}` : '#/environments'));
  }
  const current = rendering = Symbol('render');
  try {
    const content = await renderView();
    if (current !== rendering) return;
    // Views may leave null placeholders for absent sections; they render
    // as nothing, never as the text "null".
    view.replaceChildren(...[content].flat(Infinity).filter((node) => node !== null && node !== undefined && node !== false));
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
  } catch (error) {
    box.replaceChildren(state('failed', error.kind === 'authentication_failed'
      ? 'credential required: set a token'
      : error.kind === 'authorization_denied' ? 'credential lacks compute.read' : 'daemon unreachable'));
  }
}

let refreshTimer = null;
function scheduleRefresh() {
  if (dialog.open) return;
  // Never re-render under an edit in progress; the next event catches up.
  if (document.activeElement && document.activeElement.closest('.draft')) return;
  clearTimeout(refreshTimer);
  refreshTimer = setTimeout(render, 250);
}

// Lifecycle events over fetch rather than EventSource, so the stream
// carries the same Authorization header as every other request and the
// credential never appears in a URL.
async function listen() {
  let after = 0;
  for (;;) {
    try {
      const headers = { Accept: 'text/event-stream' };
      if (token()) headers.Authorization = `Bearer ${token()}`;
      const response = await fetch(`/events/stream?after=${after}`, { headers });
      if (!response.ok || !response.body) throw new Error(`HTTP ${response.status}`);
      const reader = response.body.getReader();
      const decoder = new TextDecoder();
      let buffer = '';
      for (;;) {
        const { value, done } = await reader.read();
        if (done) break;
        buffer += decoder.decode(value, { stream: true });
        let end;
        while ((end = buffer.indexOf('\n\n')) >= 0) {
          const message = buffer.slice(0, end);
          buffer = buffer.slice(end + 2);
          for (const line of message.split('\n')) {
            if (line.startsWith('id: ')) after = Math.max(after, Number(line.slice(4)) || 0);
          }
          if (message.includes('data: ') || message.startsWith('event: lagged')) scheduleRefresh();
        }
      }
    } catch { /* reconnect below */ }
    refreshDaemon();
    await new Promise((resolve) => setTimeout(resolve, 1000));
  }
}

document.getElementById('token').addEventListener('click', () => {
  const input = h('input', { id: 'token-input', type: 'password', value: token(), autocomplete: 'off' });
  modal('API token', h('div', {},
    h('p', {}, 'A production daemon requires an operator credential (compute auth create) on every request. It is kept for this browser tab only.'),
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

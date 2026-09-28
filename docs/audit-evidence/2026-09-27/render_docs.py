"""Fill the generated tables in the audit documents from docs/audit.json.

A table sits between `<!-- audit:NAME args -->` and `<!-- /audit -->` in a
document; everything between the markers is replaced. The prose around it is
written by hand. Run after generate_audit_json.py:

    python3 docs/audit-evidence/2026-09-27/render_docs.py          # rewrite
    python3 docs/audit-evidence/2026-09-27/render_docs.py --check  # fail if stale
"""
import collections
import json
import os
import re
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
DOCS = os.path.normpath(os.path.join(HERE, '..', '..'))
AUDIT = json.load(open(os.path.join(DOCS, 'audit.json')))
FILES = ['audit.md', 'architecture.md', 'runtime-matrix.md', 'provider-matrix.md',
         'product-surface.md', 'gap-analysis.md', 'base-vs-complete-compute.md']


def cell(value):
    if value is True:
        return 'yes'
    if value is False:
        return 'no'
    if value is None or value == '' or value == []:
        return '—'
    if isinstance(value, list):
        return ', '.join(cell(v) for v in value)
    return str(value).replace('|', '\\|').replace('\n', ' ')


def table(headers, rows):
    out = ['| ' + ' | '.join(headers) + ' |', '| ' + ' | '.join('---' for _ in headers) + ' |']
    out += ['| ' + ' | '.join(cell(v) for v in row) + ' |' for row in rows]
    return '\n'.join(out)


def evidence(e):
    parts = []
    for key in ('source', 'tests'):
        parts += [f'`{p}`' for p in e.get(key, [])]
    parts += [f'CLI `{c}`' for c in e.get('cli') or []]
    parts += [f'API `{a}`' for a in e.get('api') or []]
    if e.get('journey'):
        parts.append(f"journey `{e['journey']}`")
    return '<br>'.join(parts)


def capabilities(args):
    areas = args.get('area', '').split(',') if args.get('area') else None
    statuses = args.get('status', '').replace('_', ' ').split(',') if args.get('status') else None
    rows = [(f"`{c['id']}`", c['name'], f"**{c['status']}**", c['user_can_use'],
             c['in_complete_model'], c.get('notes', ''), evidence(c['evidence']))
            for c in AUDIT['capabilities']
            if (not areas or c['area'] in areas) and (not statuses or c['status'] in statuses)]
    return table(['ID', 'Capability', 'Status', 'User can use', 'In complete model', 'Notes', 'Evidence'], rows)


def capability_summary(_):
    counts = collections.Counter(c['status'] for c in AUDIT['capabilities'])
    return table(['Status', 'Capabilities'],
                 [(s, counts.get(s, 0)) for s in AUDIT['audit']['status_vocabulary']])


def journeys(_):
    return table(['Journey', 'Steps', 'Result', 'Notes', 'Evidence'],
                 [(f"`{j['id']}`", j['journey'], f"**{j['status']}**", j['notes'],
                   '<br>'.join(f'`{e}`' for e in j['evidence'])) for j in AUDIT['journeys']])


def runtimes(_):
    keys = ['runtime', 'implemented', 'discoverable', 'placeable', 'executable', 'ui', 'cli', 'tests',
            'production_ready']
    return table(['Runtime', 'Implemented', 'Discoverable', 'Placeable', 'Executable', 'UI', 'CLI', 'Tests',
                  'Production ready'], [[r[k] for k in keys] for r in AUDIT['runtimes']])


def models(_):
    return table(['Term', 'What it is in the code', 'Source', 'Collision'],
                 [(f"**{m['term']}**", m['is'], f"`{m['source']}`", m['collision']) for m in AUDIT['models']])


def execution_paths(_):
    return table(['Path', 'Where it executes', 'Authority', 'Durable record', 'Canonical job path'],
                 [(p['path'], p['where'], p['authority'], p['durable'], p['canonical_job_path'])
                  for p in AUDIT['execution_paths']])


def state(_):
    return table(['What', 'Where', 'Survives a control-plane restart', 'Survives a machine restart'],
                 [(s['what'], s['where'], s['survives_daemon_restart'], s['survives_machine_restart'])
                  for s in AUDIT['state']])


def authorization(_):
    return table(['Operation', 'Check', 'On every operation'],
                 [(a['operation'], a['check'], a['every_operation']) for a in AUDIT['authorization']])


def placement(_):
    p = AUDIT['placement']
    return table(['Placement understands', 'How'], sorted(p['understands'].items()))


def documentation(_):
    return table(['Document', 'Status', 'Notes'],
                 [(f"`{d['doc']}`", f"**{d['status']}**", d['notes']) for d in AUDIT['documentation']])


def security(_):
    return table(['ID', 'Severity', 'Status', 'Finding', 'Evidence'],
                 [(s['id'], f"**{s['severity']}**", s.get('status', 'open'), s['finding'], s['evidence'])
                  for s in AUDIT['security']])


def gaps(args):
    areas = args.get('area', '').split(',') if args.get('area') else None
    out = []
    by_area = collections.OrderedDict()
    for g in AUDIT['gaps']:
        if not areas or g['area'] in areas:
            by_area.setdefault(g['area'], []).append(g)
    for area, items in by_area.items():
        out.append(f'### {area}\n')
        for g in items:
            label = ' (closed)' if g.get('status') == 'closed' else ''
            current = 'Was' if g.get('status') == 'closed' else 'Current'
            out.append(f"**{g['id']}**{label}\n\n"
                       f"- {current}: {g['current']}\n- Desired: {g['desired']}\n- Impact: {g['impact']}\n"
                       f"- Evidence: {g['evidence']}\n- Next: {g['next']}\n")
    return '\n'.join(out).rstrip()


def gap_index(_):
    return table(['Gap', 'Area', 'Status', 'Current'],
                 [(g['id'], g['area'], g.get('status', 'open'), g['next'] if g.get('status') == 'closed' else g['current'])
                  for g in AUDIT['gaps']])


def base_vs_complete(_):
    return table(['Capability', 'Base capture (today)', 'Complete Compute', 'Gap'],
                 [(b['capability'], b['base_capture'], b['complete_compute'], b['gap'])
                  for b in AUDIT['base_vs_complete']])


def readiness(_):
    return table(['Area', 'Status', 'Evidence', 'Blocking gap'],
                 [(r['area'], f"**{r['status']}**", r['evidence'], r['blocking_gap']) for r in AUDIT['readiness']])


def backlog(_):
    out = []
    for i, stage in enumerate(AUDIT['backlog'], 1):
        out.append(f"{i}. **{stage['stage']}**")
        out += [f'   - {item}' for item in stage['items']]
    return '\n'.join(out)


def performance(_):
    p = AUDIT['performance']
    rows = [(k.replace('_', ' '), v) for k, v in p.items() if k != 'notes']
    return table(['Measurement', 'Value'], rows) + '\n\n' + '\n'.join(f'- {n}' for n in p['notes'])


def cli_summary(_):
    groups = collections.OrderedDict()
    for c in AUDIT['cli']:
        words = c['command'].split()
        group = ' '.join(words[:2])
        g = groups.setdefault(group, {'n': 0, 'defects': 0, 'untested': 0, 'interface': set()})
        g['n'] += 1
        g['defects'] += bool(c['help_defect'])
        g['untested'] += not c['tests']
        g['interface'].add(c['interface'].split(':')[0].split(' (')[0])
    return table(['Group', 'Commands', 'Help defects', 'Never invoked by a test', 'Talks to'],
                 [(f'`{k}`', v['n'], v['defects'], v['untested'], sorted(v['interface']))
                  for k, v in groups.items()])


def cli(_):
    return table(['Command', 'What it does (its help)', 'Help defect', 'Talks to', 'State', 'Authority', 'UI',
                  'Tests'],
                 [(f"`{c['command']}`", c['about'], c['help_defect'], c['interface'], c['state'], c['authority'],
                   c['ui'], [f'`{t}`' for t in c['tests']]) for c in AUDIT['cli']])


def api(_):
    return table(['Method', 'Path', 'Scope', 'UI', 'CLI', 'AppPort', 'Exercised over HTTP by a test'],
                 [(a['method'], f"`{a['path']}`", a['scope'], a['ui'], a['cli'], a['appport'],
                   a['http_path_in_tests']) for a in AUDIT['api']])


def api_summary(_):
    api = AUDIT['api']
    scopes = collections.Counter(a['scope'] for a in api)
    rows = [('routes', len(api))] + [(f'scope {k}', v) for k, v in sorted(scopes.items())]
    rows += [('used by the UI', sum(a['ui'] for a in api)), ('used by the CLI', sum(a['cli'] for a in api)),
             ('used by AppPort', sum(a['appport'] for a in api)),
             ('no client at all', sum(not (a['ui'] or a['cli'] or a['appport']) for a in api)),
             ('path exercised over HTTP by a test', sum(a['http_path_in_tests'] for a in api))]
    return table(['API', 'Count'], rows)


def api_orphans(_):
    return '\n'.join(f"- `{a['method']} {a['path']}` ({a['scope']})" for a in AUDIT['api']
                     if not (a['ui'] or a['cli'] or a['appport']))


def ui_routes(_):
    return table(['Route', 'View', 'Mode'], [(f'`{r}`', v, m) for r, v, m in AUDIT['ui']['routes']])


def not_in_ui(_):
    return '\n'.join(f'- {x}' for x in AUDIT['ui']['not_in_ui'])


def tests(_):
    files = AUDIT['tests']['files']
    kinds = collections.Counter()
    ignored = 0
    for f in files:
        kinds[f['kind']] += f['tests']
        ignored += f['ignored_without_feltdb']
    rows = [(k, v) for k, v in sorted(kinds.items())] + [('total', sum(kinds.values())),
                                                          ('ignored unless FELTDB_SERVER_BIN is set', ignored)]
    out = table(['Kind', 'Tests'], rows) + '\n\n'
    ci = AUDIT['tests']['ci']
    out += table(['CI workflow', 'Runs'], [(f'`{k}`', v) for k, v in ci.items() if k != 'not_in_ci'])
    out += '\n\nNot in CI:\n\n' + '\n'.join(f'- {x}' for x in ci['not_in_ci'])
    return out


def test_files(_):
    return table(['File', 'Kind', 'Tests', 'Ignored without FeltDB'],
                 [(f"`{f['path']}`", f['kind'], f['tests'], f['ignored_without_feltdb'])
                  for f in AUDIT['tests']['files']])


def experiments(_):
    e = AUDIT['experiments']
    keep = [k for k in e if k not in ('environment', 'launch_output', 'targets', 'manual')]
    return table(['Experiment', 'Observed'], [(f'`{k}`', json.dumps(e[k])) for k in keep])


RENDER = {f.__name__: f for f in [
    capabilities, capability_summary, journeys, runtimes, models, execution_paths, state, authorization,
    placement, documentation, security, gaps, gap_index, base_vs_complete, readiness, backlog, performance,
    cli_summary, cli, api, api_summary, api_orphans, ui_routes, not_in_ui, tests, test_files, experiments]}

BLOCK = re.compile(r'(<!-- audit:(\w+)([^>]*?) -->\n)(.*?)(<!-- /audit -->)', re.S)


def render(text):
    def fill(m):
        args = dict(a.split('=', 1) for a in m.group(3).split())
        return m.group(1) + RENDER[m.group(2)](args) + '\n' + m.group(5)
    return BLOCK.sub(fill, text)


stale = []
for name in FILES:
    path = os.path.join(DOCS, name)
    text = open(path).read()
    new = render(text)
    if new != text:
        stale.append(name)
        if '--check' not in sys.argv:
            open(path, 'w').write(new)
if '--check' in sys.argv and stale:
    sys.exit('stale tables: ' + ', '.join(stale))
print('rendered' if stale else 'up to date', stale)

#!/usr/bin/env bash
# dataset-snapshot.sh — Capture a dataset profile for comparison
#
# Queries Fuseki for triple counts per graph, key predicate distributions,
# and dependency model values. Outputs a JSON snapshot that can be diffed
# against future runs.
#
# Usage:
#   ./dataset-snapshot.sh [fuseki-endpoint] [output-file]
#
# Defaults:
#   endpoint: http://localhost:3031/packagegraph
#   output:   snapshots/dataset-$(date +%Y%m%d-%H%M%S).json
#
# Compare two snapshots:
#   ./dataset-snapshot.sh --diff snapshot-a.json snapshot-b.json

set -euo pipefail

if [[ "${1:-}" == "--diff" ]]; then
    if [[ $# -lt 3 ]]; then
        echo "Usage: $0 --diff <before.json> <after.json>"
        exit 1
    fi
    python3 -c "
import json, sys

with open('$2') as f: before = json.load(f)
with open('$3') as f: after = json.load(f)

print(f\"=== DATASET COMPARISON ===\")
print(f\"Before: {before['timestamp']}  ({before['label']})\")
print(f\"After:  {after['timestamp']}  ({after['label']})\")
print()

# Graph-level comparison
bg = {g['graph']: g['triples'] for g in before['graphs']}
ag = {g['graph']: g['triples'] for g in after['graphs']}
all_graphs = sorted(set(bg) | set(ag))

print(f\"{'Graph':<60s} {'Before':>12s} {'After':>12s} {'Delta':>12s} {'%':>8s}\")
print('-' * 104)
total_b, total_a = 0, 0
for g in all_graphs:
    b, a = bg.get(g, 0), ag.get(g, 0)
    total_b += b
    total_a += a
    delta = a - b
    pct = f'{delta/b*100:+.1f}%' if b > 0 else 'NEW'
    if delta != 0:
        print(f'{g:<60s} {b:>12,} {a:>12,} {delta:>+12,} {pct:>8s}')
print('-' * 104)
delta_t = total_a - total_b
pct_t = f'{delta_t/total_b*100:+.1f}%' if total_b > 0 else 'NEW'
print(f\"{'TOTAL':<60s} {total_b:>12,} {total_a:>12,} {delta_t:>+12,} {pct_t:>8s}\")
print()

# Predicate comparison
bp = {p['predicate']: p['count'] for p in before.get('predicates', [])}
ap = {p['predicate']: p['count'] for p in after.get('predicates', [])}
all_preds = sorted(set(bp) | set(ap))

print(f\"{'Predicate':<70s} {'Before':>10s} {'After':>10s} {'Delta':>10s}\")
print('-' * 100)
for p in all_preds:
    b, a = bp.get(p, 0), ap.get(p, 0)
    delta = a - b
    short = p.split('#')[-1] if '#' in p else p.split('/')[-1]
    if delta != 0:
        print(f'{short:<70s} {b:>10,} {a:>10,} {delta:>+10,}')
print()

# dependencyType value migration check
bdt = {v['value']: v['count'] for v in before.get('dependency_type_values', [])}
adt = {v['value']: v['count'] for v in after.get('dependency_type_values', [])}
if bdt or adt:
    print('dependencyType values:')
    print(f\"  Before: {', '.join(f\\\"{k}={v}\\\" for k,v in sorted(bdt.items(), key=lambda x: -x[1])[:5])}\")
    print(f\"  After:  {', '.join(f\\\"{k}={v}\\\" for k,v in sorted(adt.items(), key=lambda x: -x[1])[:5])}\")
    string_vals = sum(1 for k in adt if not k.startswith('http'))
    uri_vals = sum(1 for k in adt if k.startswith('http'))
    print(f\"  Migration: {uri_vals} URI values, {string_vals} string values {'✅ COMPLETE' if string_vals == 0 else '⚠️  INCOMPLETE'}\")
"
    exit 0
fi

ENDPOINT="${1:-http://localhost:3031/packagegraph}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SNAPSHOT_DIR="${SCRIPT_DIR}/../snapshots"
mkdir -p "$SNAPSHOT_DIR"
OUTPUT="${2:-${SNAPSHOT_DIR}/dataset-$(date +%Y%m%d-%H%M%S).json}"
LABEL="${DATASET_LABEL:-$(date +%Y-%m-%d)}"

echo "Capturing dataset snapshot from ${ENDPOINT}..."

# Triple counts per graph
GRAPHS=$(curl -sf -H 'Accept: application/sparql-results+json' \
    "${ENDPOINT}/sparql" \
    --data-urlencode 'query=SELECT ?g (COUNT(*) AS ?triples) WHERE { GRAPH ?g { ?s ?p ?o } } GROUP BY ?g ORDER BY DESC(?triples)')

# Key predicate counts
PREDICATES=$(curl -sf -H 'Accept: application/sparql-results+json' \
    "${ENDPOINT}/sparql" \
    --data-urlencode "query=SELECT ?p (COUNT(*) AS ?c) WHERE {
  GRAPH ?g { ?s ?p ?o }
  FILTER(?p IN (
    <https://purl.org/packagegraph/ontology/core#dependencyType>,
    <https://purl.org/packagegraph/ontology/core#directlyDependsOn>,
    <https://purl.org/packagegraph/ontology/core#hasDependency>,
    <https://purl.org/packagegraph/ontology/core#upstreamEcosystem>,
    <https://purl.org/packagegraph/ontology/core#buildDependsOn>,
    <https://purl.org/packagegraph/ontology/core#recommends>,
    <https://purl.org/packagegraph/ontology/core#suggests>,
    <https://purl.org/packagegraph/ontology/core#enhances>,
    <https://purl.org/packagegraph/ontology/core#preDepends>,
    <https://purl.org/packagegraph/ontology/core#checkRequires>,
    <https://purl.org/packagegraph/ontology/security#hasCVSSScore>,
    <https://purl.org/packagegraph/ontology/security#hasAffectedRange>,
    <https://purl.org/packagegraph/ontology/security#cvssVector>,
    <https://purl.org/packagegraph/ontology/security#affectsPackage>,
    <http://www.w3.org/1999/02/22-rdf-syntax-ns#type>
  ))
} GROUP BY ?p ORDER BY DESC(?c)")

# dependencyType value distribution (string vs URI)
DEP_TYPES=$(curl -sf -H 'Accept: application/sparql-results+json' \
    "${ENDPOINT}/sparql" \
    --data-urlencode 'query=SELECT ?val (COUNT(*) AS ?c) WHERE { GRAPH ?g { ?s <https://purl.org/packagegraph/ontology/core#dependencyType> ?val } } GROUP BY ?val ORDER BY DESC(?c) LIMIT 30')

# upstreamEcosystem value distribution (string vs URI)
ECO_TYPES=$(curl -sf -H 'Accept: application/sparql-results+json' \
    "${ENDPOINT}/sparql" \
    --data-urlencode 'query=SELECT ?val (COUNT(*) AS ?c) WHERE { GRAPH ?g { ?s <https://purl.org/packagegraph/ontology/core#upstreamEcosystem> ?val } } GROUP BY ?val ORDER BY DESC(?c) LIMIT 30')

# Assemble JSON snapshot
python3 -c "
import json, sys

graphs_raw = json.loads('''${GRAPHS}''')
preds_raw = json.loads('''${PREDICATES}''')
dt_raw = json.loads('''${DEP_TYPES}''')
eco_raw = json.loads('''${ECO_TYPES}''')

def extract_bindings(raw, key1, key2):
    return [
        {key1: b[key1]['value'], key2: int(b[key2]['value'])}
        for b in raw.get('results', {}).get('bindings', [])
    ]

graphs = extract_bindings(graphs_raw, 'g', 'triples')
preds = extract_bindings(preds_raw, 'p', 'c')
dt_vals = extract_bindings(dt_raw, 'val', 'c')
eco_vals = extract_bindings(eco_raw, 'val', 'c')

snapshot = {
    'timestamp': '$(date -u +%Y-%m-%dT%H:%M:%SZ)',
    'label': '${LABEL}',
    'endpoint': '${ENDPOINT}',
    'graphs': [{'graph': b['g'], 'triples': b['triples']} for b in graphs],
    'total_triples': sum(b['triples'] for b in graphs),
    'predicates': [{'predicate': b['p'], 'count': b['c']} for b in preds],
    'dependency_type_values': [{'value': b['val'], 'count': b['c']} for b in dt_vals],
    'upstream_ecosystem_values': [{'value': b['val'], 'count': b['c']} for b in eco_vals],
}

with open('${OUTPUT}', 'w') as f:
    json.dump(snapshot, f, indent=2)
print(f\"Snapshot saved to ${OUTPUT}\")
print(f\"  Total graphs: {len(snapshot['graphs'])}\")
print(f\"  Total triples: {snapshot['total_triples']:,}\")
print(f\"  dependencyType values: {len(snapshot['dependency_type_values'])}\")
print(f\"  upstreamEcosystem values: {len(snapshot['upstream_ecosystem_values'])}\")
"

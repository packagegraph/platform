#!/usr/bin/env python3
"""
CQ Validation Harness — Freeze, Re-Run, Reclassify

Frozen CQ version: 7dbe46ee9e45f675d1b2c5bd221bde836d765b01
Source: ontology/docs/competency-questions.md
"""

import os
import re
import subprocess
import json
import sys
from pathlib import Path

FROZEN_COMMIT = "7db2f9961999b2509e11101f0d4f3e0c9b0fd411"
ENDPOINT = os.environ.get("FUSEKI_ENDPOINT", "http://localhost:3031/packagegraph/sparql")
TIMEOUT_SECONDS = 310

SCRIPT_DIR = Path(__file__).resolve().parent
REPO_ROOT = SCRIPT_DIR.parent.parent
ONTOLOGY_REPO = os.environ.get("ONTOLOGY_REPO", str(REPO_ROOT.parent / "ontology"))
OUTPUT_DIR = REPO_ROOT / "output"

# Aggregate CQs that intentionally return a single summary row
# These are semantically PASS even though row count < 5
AGGREGATE_CQS = {
    'CQ-PM-04',   # Dependency Chain Depth (COUNT aggregate)
    'CQ-PM-06',   # Architecture Support Coverage (COUNT aggregate)
    'CQ-PM-09',   # Package Size Distribution (AVG/MIN/MAX aggregate)
    'CQ-DEP-01',  # Direct vs Transitive Dependencies (COUNT aggregate)
    'CQ-DEP-02',  # Dependency Type Distribution (GROUP BY single type)
    'CQ-ECO-03',  # npm Dependency Depth (AVG aggregate)
    'CQ-TEMP-03', # Maintainer Tenure Analysis (AVG/MIN/MAX aggregate)
}

def verify_connectivity():
    """Test west-3 Fuseki connectivity before running full suite."""
    test_query = "SELECT (1 AS ?ok) WHERE {}"
    try:
        result = subprocess.run(
            ['curl', '-s', '-m', '5', '-H', 'Accept: text/tab-separated-values',
             ENDPOINT, '--data-urlencode', f'query={test_query}'],
            capture_output=True, text=True, timeout=10
        )
        if result.returncode == 0 and result.stdout.strip():
            print(f"✓ West-3 Fuseki connectivity verified at {ENDPOINT}")
            return True
        else:
            print(f"✗ Fuseki not responding at {ENDPOINT}")
            print(f"  Endpoint is west-3 NodePort. Verify cluster is up.")
            return False
    except Exception as e:
        print(f"✗ Connectivity check failed: {e}")
        return False

def extract_cqs_from_frozen_commit():
    """Extract all CQs from the frozen commit, not working tree."""
    # Get file content from the frozen commit
    result = subprocess.run(
        ['git', '-C', ONTOLOGY_REPO,
         'show', f'{FROZEN_COMMIT}:docs/competency-questions.md'],
        capture_output=True, text=True, timeout=10
    )
    if result.returncode != 0:
        raise Exception(f"Failed to read frozen CQ file from commit {FROZEN_COMMIT}")
    content = result.stdout

    # Pattern: ### CQ-XXX-NN: Title ... ```sparql ... ``` ... **Status:** ...
    pattern = r'### (CQ-[A-Z]+-\d+[a-z]?):\s*(.*?)\n.*?```sparql\n(.*?)```.*?(?:\*\*Status:\*\*\s*(.*?))\n'
    matches = re.findall(pattern, content, re.DOTALL)

    cqs = []
    for cq_id, title, query, status in matches:
        cqs.append({
            'id': cq_id,
            'title': title.strip(),
            'query': query.strip(),
            'status': status.strip()
        })

    return cqs

def run_query(query):
    """Run a SPARQL query against Fuseki with timeout."""
    # Add LIMIT if not present
    if 'LIMIT' not in query.upper():
        query += '\nLIMIT 5'

    try:
        result = subprocess.run(
            ['curl', '-s', '-w', '\n%{http_code}', '-m', str(TIMEOUT_SECONDS),
             '-H', 'Accept: text/tab-separated-values',
             ENDPOINT, '--data-urlencode', f'query={query}'],
            capture_output=True, text=True, timeout=TIMEOUT_SECONDS + 5
        )

        output = result.stdout.strip()
        lines = output.split('\n')

        # Last line is HTTP status from -w
        http_status = lines[-1] if lines else '000'
        body_lines = lines[:-1] if len(lines) > 1 else []
        body = '\n'.join(body_lines)

        if result.returncode != 0:
            return {'result': 'ERROR', 'rows': 0, 'output': f'curl failed: {result.returncode}'}
        elif not http_status.startswith('2'):
            return {'result': 'ERROR', 'rows': 0, 'output': f'HTTP {http_status}'}
        elif 'Query timed out' in body:
            return {'result': 'TIMEOUT', 'rows': 0, 'output': ''}
        elif body:
            data_lines = [l for l in body_lines[1:] if l.strip()]
            rows = len(data_lines)
            sample = data_lines[0][:200] if data_lines else ''

            if rows == 0:
                classification = 'EMPTY'
            elif rows < 5:
                classification = 'MARGINAL'
            else:
                classification = 'PASS'

            return {
                'result': classification,
                'rows': rows,
                'output': sample
            }
    except subprocess.TimeoutExpired:
        return {'result': 'TIMEOUT', 'rows': 0, 'output': ''}
    except Exception as e:
        return {'result': 'ERROR', 'rows': 0, 'output': str(e)[:200]}

def main():
    # Verify connectivity first
    if not verify_connectivity():
        sys.exit(1)

    print(f"\nFrozen CQ version: {FROZEN_COMMIT}")
    print(f"CQ source: ontology/docs/competency-questions.md (from git commit)\n")

    # Extract CQs from frozen commit
    cqs = extract_cqs_from_frozen_commit()
    print(f"Extracted {len(cqs)} CQs\n")

    # Run queries
    print("Running queries...")
    results = []
    for i, cq in enumerate(cqs, 1):
        print(f"[{i}/{len(cqs)}] {cq['id']}...", end=' ', flush=True)

        query_result = run_query(cq['query'])

        # Reclassify aggregate CQs: single-row aggregates are semantically PASS
        # BUT: empty aggregates (all-zero output) should remain MARGINAL
        if cq['id'] in AGGREGATE_CQS and query_result.get('rows', 0) >= 1:
            output_sample = str(query_result.get('output', ''))
            # Check if output contains only zeros/tabs (empty aggregate)
            # Strip tabs and check if remaining chars are all 0 or empty
            data_chars = output_sample.replace('\t', '').strip()
            is_empty_aggregate = (data_chars == '0' or all(c == '0' for c in data_chars)) if data_chars else True

            if not is_empty_aggregate:
                query_result['result'] = 'PASS_AGGREGATE'

        result = {
            **cq,
            **query_result
        }
        results.append(result)

        print(f"{result['result']} ({result['rows']} rows)")

    # Save raw results
    OUTPUT_DIR.mkdir(parents=True, exist_ok=True)
    output_file = OUTPUT_DIR / "cq-results.json"
    with open(output_file, 'w') as f:
        json.dump({
            'frozen_commit': FROZEN_COMMIT,
            'endpoint': ENDPOINT,
            'timeout_seconds': TIMEOUT_SECONDS,
            'results': results
        }, f, indent=2)

    print(f"\nRaw results saved to {output_file}")

    # Summary
    pass_count = sum(1 for r in results if r['result'] == 'PASS')
    pass_aggregate_count = sum(1 for r in results if r['result'] == 'PASS_AGGREGATE')
    marginal_count = sum(1 for r in results if r['result'] == 'MARGINAL')
    empty_count = sum(1 for r in results if r['result'] == 'EMPTY')
    timeout_count = sum(1 for r in results if r['result'] == 'TIMEOUT')
    error_count = sum(1 for r in results if r['result'] == 'ERROR')
    effective_pass = pass_count + pass_aggregate_count

    print(f"\nSummary: {pass_count} PASS, {pass_aggregate_count} PASS_AGGREGATE (effective {effective_pass}), {marginal_count} MARGINAL, {empty_count} EMPTY, {timeout_count} TIMEOUT, {error_count} ERROR")

    # Generate markdown report from measured results only
    report_path = OUTPUT_DIR / "cq-validation-report.md"
    with open(report_path, 'w') as f:
        f.write(f"# CQ Validation Report\n\n")
        f.write(f"**Generated by:** `cq-validate.py` (all claims below are measured, not inferred)\n")
        f.write(f"**Frozen CQ commit:** `{FROZEN_COMMIT}`\n")
        f.write(f"**Endpoint:** {ENDPOINT}\n")
        f.write(f"**Client timeout:** {TIMEOUT_SECONDS}s\n\n")
        f.write(f"## Summary\n\n")
        f.write(f"| Result | Count |\n|--------|------:|\n")
        f.write(f"| PASS (≥5 rows) | {pass_count} |\n")
        f.write(f"| PASS_AGGREGATE (semantic success) | {pass_aggregate_count} |\n")
        f.write(f"| **Effective PASS** | **{effective_pass}** |\n")
        f.write(f"| MARGINAL (1-4 rows) | {marginal_count} |\n")
        f.write(f"| EMPTY | {empty_count} |\n")
        f.write(f"| TIMEOUT | {timeout_count} |\n")
        f.write(f"| ERROR | {error_count} |\n")
        f.write(f"| **Total** | **{len(results)}** |\n\n")
        f.write(f"PASS_AGGREGATE = aggregate queries (COUNT/AVG/MIN/MAX) that intentionally return a single summary row. These are semantically correct.\n")
        f.write(f"MARGINAL = non-aggregate queries returning 1-4 rows — may indicate missing data.\n\n")

        # Group by domain
        domains = {}
        for r in results:
            domain = r['id'].split('-')[1]
            domains.setdefault(domain, []).append(r)

        f.write(f"## Results by Domain\n\n")
        for domain, cqs in sorted(domains.items()):
            domain_pass = sum(1 for c in cqs if c['result'] in ('PASS', 'PASS_AGGREGATE'))
            f.write(f"### {domain} ({domain_pass}/{len(cqs)} PASS)\n\n")
            f.write(f"| CQ | Title | Result | Rows |\n")
            f.write(f"|----|-------|--------|-----:|\n")
            for c in cqs:
                if c['result'] == 'PASS':
                    icon = '✅'
                elif c['result'] == 'PASS_AGGREGATE':
                    icon = '✅🔢'
                elif c['result'] == 'MARGINAL':
                    icon = '⚠️'
                elif c['result'] == 'ERROR':
                    icon = '❌'
                elif c['result'] == 'TIMEOUT':
                    icon = '⏱️'
                else:
                    icon = '⬚'
                f.write(f"| {c['id']} | {c['title']} | {icon} {c['result']} | {c['rows']} |\n")
            f.write(f"\n")

        # Non-success details (exclude PASS and PASS_AGGREGATE)
        non_success = [r for r in results if r['result'] not in ('PASS', 'PASS_AGGREGATE')]
        if non_success:
            f.write(f"## Non-PASS Details\n\n")
            for r in non_success:
                f.write(f"- **{r['id']}** ({r['result']}): {r.get('output', 'no details')}\n")

    print(f"Report saved to {report_path}")

if __name__ == '__main__':
    main()

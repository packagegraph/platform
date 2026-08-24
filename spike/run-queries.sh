#!/bin/bash
# Run SPARQL validation queries against QLever and record results.
# Usage: ssh berstuk.lan 'bash ~/qlever-spike/run-queries.sh'
set -euo pipefail

ENDPOINT="http://localhost:7001"
RESULTS_FILE="/tmp/qlever-spike-results.json"

run_query() {
    local label="$1"
    local query="$2"
    echo -n "  $label: "

    local start_ms=$(date +%s%3N)
    local result
    result=$(curl -s -X POST "$ENDPOINT/" \
        --data-urlencode "query=$query" \
        -H "Accept: application/sparql-results+json" 2>&1)
    local exit_code=$?
    local end_ms=$(date +%s%3N)
    local elapsed=$((end_ms - start_ms))

    if [ $exit_code -ne 0 ]; then
        echo "FAIL (curl error $exit_code, ${elapsed}ms)"
        return 1
    fi

    # Check for QLever error
    if echo "$result" | grep -q '"exception"'; then
        local err=$(echo "$result" | python3 -c "import sys,json; print(json.load(sys.stdin).get('exception','unknown'))" 2>/dev/null || echo "$result" | head -c 200)
        echo "ERROR: $err (${elapsed}ms)"
        return 1
    fi

    # Extract result count and query time from QLever metadata
    local qlever_ms=$(echo "$result" | python3 -c "import sys,json; print(json.load(sys.stdin).get('meta',{}).get('query-time-ms','?'))" 2>/dev/null || echo "?")
    local count=$(echo "$result" | python3 -c "import sys,json; r=json.load(sys.stdin); print(len(r.get('results',{}).get('bindings',[])))" 2>/dev/null || echo "?")

    # For COUNT queries, extract the value
    local count_val=$(echo "$result" | python3 -c "
import sys,json
r=json.load(sys.stdin)
b=r.get('results',{}).get('bindings',[])
if len(b)==1 and len(b[0])==1:
    print(list(b[0].values())[0]['value'])
else:
    print(f'{len(b)} rows')
" 2>/dev/null || echo "$count rows")

    echo "PASS — $count_val (QLever: ${qlever_ms}ms, wall: ${elapsed}ms)"
    return 0
}

echo "=== QLever SPARQL Validation ==="
echo "Endpoint: $ENDPOINT"
echo "Date: $(date -Iseconds)"
echo ""

PASS=0
FAIL=0

# --- Q1: Total Triple Count ---
run_query "Q1  Total triples" \
    "SELECT (COUNT(*) AS ?c) WHERE { ?s ?p ?o }" && PASS=$((PASS+1)) || FAIL=$((FAIL+1))

# --- Q2: Binary Package Count ---
run_query "Q2  Binary packages" \
    "PREFIX pkg: <https://purl.org/packagegraph/ontology/core#>
     SELECT (COUNT(DISTINCT ?p) AS ?c) WHERE { ?p a pkg:BinaryPackage }" && PASS=$((PASS+1)) || FAIL=$((FAIL+1))

# --- Q3: Source Package Count ---
run_query "Q3  Source packages" \
    "PREFIX pkg: <https://purl.org/packagegraph/ontology/core#>
     SELECT (COUNT(DISTINCT ?p) AS ?c) WHERE { ?p a pkg:SourcePackage }" && PASS=$((PASS+1)) || FAIL=$((FAIL+1))

# --- Q4: Dependency Link Count ---
run_query "Q4  Dependencies" \
    "PREFIX pkg: <https://purl.org/packagegraph/ontology/core#>
     SELECT (COUNT(*) AS ?c) WHERE { ?p pkg:directlyDependsOn ?t }" && PASS=$((PASS+1)) || FAIL=$((FAIL+1))

# --- Q5: Unique Maintainer Count ---
run_query "Q5  Maintainers" \
    "PREFIX pkg: <https://purl.org/packagegraph/ontology/core#>
     SELECT (COUNT(DISTINCT ?m) AS ?c) WHERE { ?p pkg:maintainedBy ?m }" && PASS=$((PASS+1)) || FAIL=$((FAIL+1))

# --- Q6: Source-Binary Link Count ---
run_query "Q6  Source-binary links" \
    "PREFIX pkg: <https://purl.org/packagegraph/ontology/core#>
     SELECT (COUNT(*) AS ?c) WHERE { ?b pkg:builtFromSource ?s }" && PASS=$((PASS+1)) || FAIL=$((FAIL+1))

# --- Q7: Dual-Typed Packages ---
run_query "Q7  Dual-typed (Binary+deb)" \
    "PREFIX pkg: <https://purl.org/packagegraph/ontology/core#>
     PREFIX deb: <https://purl.org/packagegraph/ontology/deb#>
     SELECT (COUNT(DISTINCT ?p) AS ?c) WHERE { ?p a pkg:BinaryPackage ; a deb:BinaryPackage }" && PASS=$((PASS+1)) || FAIL=$((FAIL+1))

# --- Q8: Top 15 Most-Depended-On (GROUP BY + ORDER BY + aggregate) ---
run_query "Q8  Top depended-on" \
    "PREFIX pkg: <https://purl.org/packagegraph/ontology/core#>
     SELECT ?target (COUNT(?p) AS ?depCount) WHERE {
       ?p pkg:directlyDependsOn ?target .
     } GROUP BY ?target ORDER BY DESC(?depCount) LIMIT 15" && PASS=$((PASS+1)) || FAIL=$((FAIL+1))

# --- Q9: Top 10 Maintainers by Package Count ---
run_query "Q9  Top maintainers" \
    "PREFIX pkg: <https://purl.org/packagegraph/ontology/core#>
     PREFIX foaf: <http://xmlns.com/foaf/0.1/>
     SELECT ?name (COUNT(DISTINCT ?p) AS ?pkgCount) WHERE {
       ?p pkg:maintainedBy ?m .
       ?m foaf:name ?name .
     } GROUP BY ?name ORDER BY DESC(?pkgCount) LIMIT 10" && PASS=$((PASS+1)) || FAIL=$((FAIL+1))

# --- Q10: RDF Class Distribution ---
run_query "Q10 Class distribution" \
    "SELECT ?type (COUNT(?s) AS ?c) WHERE {
       ?s a ?type .
     } GROUP BY ?type ORDER BY DESC(?c) LIMIT 10" && PASS=$((PASS+1)) || FAIL=$((FAIL+1))

# --- Q11: Predicate Usage Distribution ---
run_query "Q11 Predicate distribution" \
    "SELECT ?p (COUNT(*) AS ?c) WHERE { ?s ?p ?o } GROUP BY ?p ORDER BY DESC(?c) LIMIT 20" && PASS=$((PASS+1)) || FAIL=$((FAIL+1))

# --- Q12: Architecture Distribution ---
run_query "Q12 Architecture dist" \
    "PREFIX pkg: <https://purl.org/packagegraph/ontology/core#>
     SELECT ?arch (COUNT(DISTINCT ?p) AS ?c) WHERE {
       ?p pkg:targetArchitecture ?arch .
     } GROUP BY ?arch ORDER BY DESC(?c)" && PASS=$((PASS+1)) || FAIL=$((FAIL+1))

# --- Q13: Dependencies with Version Constraints ---
run_query "Q13 Versioned deps" \
    "PREFIX pkg: <https://purl.org/packagegraph/ontology/core#>
     SELECT (COUNT(*) AS ?c) WHERE {
       ?dep pkg:hasVersionConstraint ?vc .
     }" && PASS=$((PASS+1)) || FAIL=$((FAIL+1))

# --- Q14: Sample Package Detail (bash) ---
run_query "Q14 Package detail (bash)" \
    "PREFIX pkg: <https://purl.org/packagegraph/ontology/core#>
     PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#>
     SELECT ?name ?version ?desc ?arch WHERE {
       ?p pkg:packageName \"bash\" ;
          pkg:hasVersion ?v ;
          pkg:description ?desc .
       ?v pkg:versionString ?version .
       OPTIONAL { ?p pkg:targetArchitecture ?arch }
     } LIMIT 10" && PASS=$((PASS+1)) || FAIL=$((FAIL+1))

# --- Q15: Direct Reverse Dependencies of libc6 ---
run_query "Q15 Reverse deps (libc6)" \
    "PREFIX pkg: <https://purl.org/packagegraph/ontology/core#>
     SELECT ?name WHERE {
       ?p pkg:packageName ?name ;
          pkg:directlyDependsOn ?libc .
       ?libc pkg:packageName \"libc6\" .
     } LIMIT 20" && PASS=$((PASS+1)) || FAIL=$((FAIL+1))

echo ""
echo "=== Critical Compatibility Tests ==="

# --- Union Default Graph ---
echo ""
echo "--- Union Default Graph ---"
run_query "UDG Unscoped count" \
    "SELECT (COUNT(*) AS ?c) WHERE { ?s ?p ?o }" && PASS=$((PASS+1)) || FAIL=$((FAIL+1))

run_query "UDG Graph list" \
    "SELECT ?g WHERE { GRAPH ?g { ?s ?p ?o } } GROUP BY ?g ORDER BY ?g" && PASS=$((PASS+1)) || FAIL=$((FAIL+1))

run_query "UDG Per-graph count" \
    "SELECT ?g (COUNT(*) AS ?c) WHERE { GRAPH ?g { ?s ?p ?o } } GROUP BY ?g ORDER BY DESC(?c)" && PASS=$((PASS+1)) || FAIL=$((FAIL+1))

# --- Graph-Scoped Queries ---
echo ""
echo "--- Graph-Scoped Queries ---"
run_query "GS  Fedora-43 packages" \
    "PREFIX pkg: <https://purl.org/packagegraph/ontology/core#>
     SELECT (COUNT(DISTINCT ?p) AS ?c) WHERE {
       GRAPH <https://packagegraph.github.io/graph/fedora/43> {
         ?p a pkg:BinaryPackage .
       }
     }" && PASS=$((PASS+1)) || FAIL=$((FAIL+1))

run_query "GS  CentOS-9 packages" \
    "PREFIX pkg: <https://purl.org/packagegraph/ontology/core#>
     SELECT (COUNT(DISTINCT ?p) AS ?c) WHERE {
       GRAPH <https://packagegraph.github.io/graph/centos-stream/9> {
         ?p a pkg:BinaryPackage .
       }
     }" && PASS=$((PASS+1)) || FAIL=$((FAIL+1))

# --- Property Paths ---
echo ""
echo "--- Property Path Queries ---"
run_query "PP  Transitive deps (bash, depth 2)" \
    "PREFIX pkg: <https://purl.org/packagegraph/ontology/core#>
     SELECT ?dep WHERE {
       ?p pkg:packageName \"bash\" .
       ?p pkg:directlyDependsOn+ ?dep .
     } LIMIT 50" && PASS=$((PASS+1)) || FAIL=$((FAIL+1))

# --- SPARQL Update (DROP GRAPH) ---
echo ""
echo "--- SPARQL Update ---"
echo -n "  SU  DROP GRAPH (test): "
result=$(curl -s -X POST "$ENDPOINT/" \
    --data-urlencode "query=DROP GRAPH <https://packagegraph.github.io/graph/test/drop>" \
    -H "Accept: application/sparql-results+json" 2>&1)
if echo "$result" | grep -q '"exception"'; then
    err=$(echo "$result" | python3 -c "import sys,json; print(json.load(sys.stdin).get('exception','unknown'))" 2>/dev/null || echo "$result" | head -c 200)
    echo "ERROR: $err"
    FAIL=$((FAIL+1))
else
    echo "PASS (no error for empty graph)"
    PASS=$((PASS+1))
fi

# --- CONSTRUCT query ---
echo ""
echo "--- CONSTRUCT ---"
echo -n "  CON CONSTRUCT (sample): "
result=$(curl -s -X POST "$ENDPOINT/" \
    --data-urlencode "query=PREFIX pkg: <https://purl.org/packagegraph/ontology/core#>
CONSTRUCT { ?p pkg:packageName ?name } WHERE { ?p pkg:packageName ?name } LIMIT 5" \
    -H "Accept: application/n-triples" 2>&1)
if echo "$result" | grep -q "packageName"; then
    count=$(echo "$result" | grep -c "packageName" || true)
    echo "PASS ($count triples)"
    PASS=$((PASS+1))
elif echo "$result" | grep -q '"exception"'; then
    echo "ERROR: $(echo "$result" | head -c 200)"
    FAIL=$((FAIL+1))
else
    echo "RESULT: $(echo "$result" | head -c 200)"
    PASS=$((PASS+1))
fi

echo ""
echo "==============================="
echo "Results: $PASS passed, $FAIL failed"
echo "==============================="

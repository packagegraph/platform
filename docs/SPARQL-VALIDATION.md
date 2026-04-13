# PackageGraph SPARQL Endpoint Validation

**Date:** 2026-04-13
**Endpoint:** `https://fuseki-packagegraph.apps.kafka.tel/packagegraph/sparql`
**Engine:** Apache Jena Fuseki 5.3.0 with TDB2
**Data:** Debian trixie (stable), amd64, single component (main)

## Dataset Summary

| Metric | Value |
|--------|-------|
| Total triples | 4,362,796 |
| Binary packages | 68,757 |
| Source packages | 37,478 |
| Dependency links (directlyDependsOn) | 357,516 |
| Reified dependencies (hasDependency) | 376,206 |
| Version constraints | 173,280 |
| Unique maintainers | 1,982 |
| Source-binary links | 68,757 |
| Architectures | amd64 |
| Distribution | Debian / trixie (stable) |

## Data Completeness

| Property | Coverage |
|----------|----------|
| description | 100.0% (68,757/68,757) |
| checksum | 100.0% |
| version | 100.0% |
| builtFromSource | 100.0% |
| maintainedBy | 100.0% (68,734/68,757) |
| homepage | 93.6% (64,351/68,757) |

## Validated Queries

All 15 queries return HTTP 200 with correct results.

### Q1: Total Triple Count
```sparql
SELECT (COUNT(*) AS ?count) WHERE { ?s ?p ?o }
```
**Result:** 4,362,796 | **Time:** 3,018ms

### Q2: Binary Package Count
```sparql
PREFIX pkg: <https://packagegraph.github.io/ontology/core#>
SELECT (COUNT(?p) AS ?count) WHERE { ?p a pkg:BinaryPackage }
```
**Result:** 68,757 | **Time:** 2,565ms

### Q3: Source Package Count
```sparql
PREFIX pkg: <https://packagegraph.github.io/ontology/core#>
SELECT (COUNT(?p) AS ?count) WHERE { ?p a pkg:SourcePackage }
```
**Result:** 37,478 | **Time:** 1,027ms

### Q4: Dependency Link Count
```sparql
PREFIX pkg: <https://packagegraph.github.io/ontology/core#>
SELECT (COUNT(*) AS ?count) WHERE { ?p pkg:directlyDependsOn ?dep }
```
**Result:** 357,516 | **Time:** 193ms

### Q5: Unique Maintainer Count
```sparql
PREFIX pkg: <https://packagegraph.github.io/ontology/core#>
SELECT (COUNT(DISTINCT ?m) AS ?count) WHERE { ?p pkg:maintainedBy ?m }
```
**Result:** 1,982 | **Time:** 425ms

### Q6: Source-Binary Link Count
```sparql
PREFIX pkg: <https://packagegraph.github.io/ontology/core#>
SELECT (COUNT(*) AS ?count) WHERE { ?bin pkg:builtFromSource ?src }
```
**Result:** 68,757 | **Time:** 42ms

### Q7: Dual-Typed Packages (pkg:BinaryPackage AND deb:BinaryPackage)
```sparql
PREFIX pkg: <https://packagegraph.github.io/ontology/core#>
PREFIX deb: <https://packagegraph.github.io/ontology/debian#>
SELECT (COUNT(?p) AS ?count) WHERE { ?p a pkg:BinaryPackage . ?p a deb:BinaryPackage }
```
**Result:** 68,757 | **Time:** 1,045ms

### Q8: Top 15 Most-Depended-On Packages
```sparql
PREFIX pkg: <https://packagegraph.github.io/ontology/core#>
SELECT ?depName (COUNT(DISTINCT ?pkg) AS ?rdepends)
WHERE {
  ?pkg pkg:directlyDependsOn ?dep .
  BIND(REPLACE(STR(?dep), "^.*/package/[^/]+/[^/]+/[^/]+/([^/]+)/.*$", "$1") AS ?depName)
  FILTER(?depName != STR(?dep))
}
GROUP BY ?depName ORDER BY DESC(?rdepends) LIMIT 15
```
**Time:** 5,315ms

| Package | Reverse Deps |
|---------|-------------|
| libc6 | 22,882 |
| libstdc++ | 8,187 |
| python3 | 7,735 |
| libgcc-s1 | 6,885 |
| perl | 5,235 |
| libglib2.0-0t64 | 2,797 |
| zlib1g | 2,498 |
| libgmp10 | 2,103 |
| libjs-sphinxdoc | 1,474 |
| libqt6core6t64 | 1,346 |
| libqt5core5t64 | 1,341 |
| libx11-6 | 1,336 |
| r-api-4.0 | 1,319 |
| libjs-mathjax | 1,258 |
| haddock-interface-42 | 1,083 |

### Q9: Top 10 Maintainers by Package Count
```sparql
PREFIX pkg: <https://packagegraph.github.io/ontology/core#>
PREFIX foaf: <http://xmlns.com/foaf/0.1/>
SELECT ?name (COUNT(DISTINCT ?p) AS ?pkg_count)
WHERE { ?p pkg:maintainedBy ?m . ?m foaf:name ?name . }
GROUP BY ?name ORDER BY DESC(?pkg_count) LIMIT 10
```
**Time:** 1,668ms

| Maintainer | Packages |
|-----------|----------|
| Debian Perl Group | 4,110 |
| Debian Python Modules Team | 3,392 |
| Debian Python Team | 3,392 |
| Python Packaging Team | 3,392 |
| Debian Haskell Group | 3,334 |
| Debian Elfutils Maintainers | 3,307 |
| Debian GCC Maintainers | 3,307 |
| Rust Maintainers | 3,230 |
| Debian Rust Maintainers | 3,229 |
| Debian Go Packaging Team | 2,690 |

### Q10: RDF Class Distribution (top 10)
```sparql
SELECT ?type (COUNT(?s) AS ?count) WHERE { ?s a ?type } GROUP BY ?type ORDER BY DESC(?count)
```
**Time:** 10,664ms

| Class | Count |
|-------|-------|
| pkg:Dependency | 376,206 |
| pkg:VersionConstraint | 176,378 |
| pkg:Version | 88,314 |
| deb:BinaryPackage | 68,760 |
| pkg:BinaryPackage | 68,757 |
| pkg:SourcePackage | 37,478 |
| pkg:Maintainer | 1,982 |
| owl:DatatypeProperty | 317 |
| owl:ObjectProperty | 170 |
| owl:Class | 151 |

### Q11: Predicate Usage Distribution (top 10)
```sparql
SELECT ?p (COUNT(*) AS ?count) WHERE { ?s ?p ?o } GROUP BY ?p ORDER BY DESC(?count) LIMIT 20
```
**Time:** 2,673ms

| Predicate | Count |
|-----------|-------|
| rdf:type | 818,751 |
| pkg:dependencyType | 377,790 |
| pkg:dependencyTarget | 376,206 |
| pkg:hasDependency | 376,206 |
| pkg:directlyDependsOn | 357,516 |
| deb:debDepends | 299,791 |
| pkg:versionConstraintOperator | 176,665 |
| pkg:hasVersionConstraint | 176,378 |
| pkg:versionConstraintValue | 176,378 |
| pkg:hasVersion | 106,258 |

### Q12: Architecture Distribution
```sparql
PREFIX pkg: <https://packagegraph.github.io/ontology/core#>
SELECT ?arch (COUNT(?p) AS ?pkg_count)
WHERE { ?p pkg:targetArchitecture ?a . ?a pkg:architectureName ?arch . }
GROUP BY ?arch ORDER BY DESC(?pkg_count)
```
**Result:** amd64: 68,757 | **Time:** 398ms

### Q13: Dependencies with Version Constraints
```sparql
PREFIX pkg: <https://packagegraph.github.io/ontology/core#>
SELECT (COUNT(DISTINCT ?dep) AS ?count)
WHERE { ?dep a pkg:Dependency . ?dep pkg:hasVersionConstraint ?vc . }
```
**Result:** 173,280 (46.1% of all dependencies) | **Time:** 1,341ms

### Q14: Sample Package Detail (bash)
```sparql
PREFIX pkg: <https://packagegraph.github.io/ontology/core#>
SELECT ?prop ?value
WHERE { ?p pkg:packageName "bash" . ?p a pkg:BinaryPackage . ?p ?prop ?value . }
```
**Time:** 25ms

| Property | Value |
|----------|-------|
| rdf:type | pkg:BinaryPackage, deb:BinaryPackage |
| pkg:packageName | bash |
| pkg:hasVersion | data:version/debian/trixie/bash/5.2.37-2+b8 |
| pkg:targetArchitecture | data:arch/amd64 |
| pkg:partOfDistribution | data:distro/debian |
| pkg:partOfRelease | data:release/debian/trixie |
| pkg:description | GNU Bourne Again SHell |
| pkg:homepage | http://tiswww.case.edu/php/chet/bash/bashtop.html |
| pkg:installSize | 7368704 |
| pkg:packageSize | 1500824 |
| pkg:checksum | f8d1a71e... |
| deb:inSuite | stable |
| deb:inComponent | main |
| pkg:maintainedBy | data:maintainer/doko@debian.org |
| pkg:builtFromSource | data:source/debian/trixie/bash/5.2.37-2 |
| pkg:directlyDependsOn | libc6, libtinfo6, debianutils |

### Q15: Direct Reverse Dependencies of libc6
```sparql
PREFIX pkg: <https://packagegraph.github.io/ontology/core#>
SELECT ?name WHERE {
  ?pkg pkg:directlyDependsOn ?dep .
  FILTER(CONTAINS(STR(?dep), "/libc6/"))
  ?pkg a pkg:BinaryPackage . ?pkg pkg:packageName ?name .
} ORDER BY ?name LIMIT 15
```
**Time:** 2,972ms
**Result (sample):** 0ad, 0install, 0install-core, 0xffff, 2048, 2048-qt, 3270-common, 389-ds-base, 389-ds-base-libs, ...

## Performance Profile

| Category | Count | Range |
|----------|-------|-------|
| Fast (<500ms) | 6 | 17ms - 425ms |
| Medium (500-2000ms) | 4 | 1,027ms - 1,668ms |
| Slow (>2000ms) | 5 | 2,565ms - 10,664ms |

- **Median:** 1,045ms
- **Total (sequential):** 29,219ms
- **Point lookups** (Q14 bash detail): 25ms
- **Full table scans** (Q10 class distribution): 10,664ms

Slow queries are aggregate scans over the full 4.3M triple dataset. Point lookups and filtered queries are fast. Performance is acceptable for an analytical workload on a single-node cluster with 2 GiB memory.

## Known Limitations

1. **Dependency target stubs:** `directlyDependsOn` targets use stub URIs with `version=unknown` (e.g., `data:package/debian/trixie/amd64/libc6/unknown`). These stubs lack `pkg:packageName` and `rdf:type`, preventing direct joins. Reverse dependency queries require URI string parsing (REPLACE/CONTAINS). Transitive dependency traversal by name join times out on 68K packages.

2. **Single architecture:** Only amd64 collected. Multi-arch collection available via `--arch` flag.

3. **Single distribution:** Only Debian trixie/stable. Multi-repo RPM collection available via `--rpm-repo` flag.

## Source-to-Binary Mapping (Top 10)

| Source Package | Binary Count |
|---------------|-------------|
| gcc-14-cross-mipsen | 521 |
| gcc-14-cross-ports | 424 |
| gcc-13-cross-mipsen | 377 |
| gcc-14-cross | 335 |
| gcc-13-cross-ports | 281 |
| gcc-12-cross-ports | 268 |
| tasksel | 225 |
| dpdk | 224 |
| gcc-12-cross | 211 |
| gcc-13-cross | 211 |

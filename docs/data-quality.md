# Data Quality System

PackageGraph records data quality issues as first-class triples in the knowledge graph using the `dq:` ontology namespace (`https://purl.org/packagegraph/ontology/dq#`). Issues are linked to affected resources and queryable via SPARQL.

## How It Works

During ETL (collection and enrichment), components call `flag_quality_issue()` to record problems instead of silently skipping bad data:

```python
self.writer.flag_quality_issue(
    subject_uri,          # The affected resource URI
    "malformed-email",    # Issue type
    "commit.author.email",# Which field has the problem
    raw_value,            # The actual bad value
    "enrich-github-vcs"   # Which component found it
)
```

This emits triples:
```turtle
<dq:issue/abc123> a dq:DataQualityIssue ;
    dq:issueType "malformed-email" ;
    dq:field "commit.author.email" ;
    dq:rawValue "Peter Wang" ;
    dq:detectedBy "enrich-github-vcs" .
<repo-uri> dq:hasQualityIssue <dq:issue/abc123> .
```

## Issue Types

| Type | Severity | Source | Description |
|------|----------|--------|-------------|
| `dead-repo` | warning | enrich-github-vcs | Repository returned HTTP 404 (deleted, private, or moved) |
| `malformed-email` | info | enrich-github-vcs | Commit author email contains spaces, obfuscation, or garbage |
| `invalid-homepage` | warning | collectors | Package homepage is not a valid HTTP(S) URL |
| `missing-field` | error | collectors | Required field is absent or empty |
| `stale-data` | info | validators | Cached data exceeds expected freshness threshold |
| `encoding-error` | warning | collectors | Field contains invalid characters or encoding |

## SPARQL Queries

### Summary: Issues by Type

```sparql
PREFIX dq: <https://purl.org/packagegraph/ontology/dq#>
SELECT ?type (COUNT(*) AS ?count) WHERE {
  ?issue a dq:DataQualityIssue .
  ?issue dq:issueType ?type .
} GROUP BY ?type ORDER BY DESC(?count)
```

### Find Packages with Dead Upstream Repos

```sparql
PREFIX dq: <https://purl.org/packagegraph/ontology/dq#>
PREFIX vcs: <https://purl.org/packagegraph/ontology/vcs#>
PREFIX pkg: <https://purl.org/packagegraph/ontology/core#>
SELECT ?name ?repoUrl ?checkedAt WHERE {
  ?repo dq:hasQualityIssue ?issue .
  ?issue dq:issueType "dead-repo" .
  ?repo vcs:repositoryURL ?repoUrl .
  ?repo vcs:statusCheckedAt ?checkedAt .
  ?pkg pkg:homepage ?repoUrl .
  ?pkg pkg:packageName ?name .
} ORDER BY ?name
```

### Find Repos with Malformed Commit Author Emails

```sparql
PREFIX dq: <https://purl.org/packagegraph/ontology/dq#>
PREFIX vcs: <https://purl.org/packagegraph/ontology/vcs#>
SELECT ?repoUrl ?email WHERE {
  ?repo dq:hasQualityIssue ?issue .
  ?issue dq:issueType "malformed-email" .
  ?issue dq:rawValue ?email .
  ?repo vcs:repositoryURL ?repoUrl .
} ORDER BY ?repoUrl
```

### All Issues for a Specific Component

```sparql
PREFIX dq: <https://purl.org/packagegraph/ontology/dq#>
SELECT ?type ?field ?value WHERE {
  ?issue a dq:DataQualityIssue .
  ?issue dq:detectedBy "enrich-github-vcs" .
  ?issue dq:issueType ?type .
  ?issue dq:field ?field .
  ?issue dq:rawValue ?value .
} LIMIT 100
```

### Packages with Any Data Quality Issue

```sparql
PREFIX dq: <https://purl.org/packagegraph/ontology/dq#>
PREFIX pkg: <https://purl.org/packagegraph/ontology/core#>
SELECT ?name ?type ?field ?value WHERE {
  ?resource dq:hasQualityIssue ?issue .
  ?issue dq:issueType ?type .
  ?issue dq:field ?field .
  ?issue dq:rawValue ?value .
  ?pkg pkg:homepage ?homepage .
  ?pkg pkg:packageName ?name .
  FILTER(STR(?resource) = STR(?homepage) || ?resource = ?pkg)
} ORDER BY ?name LIMIT 100
```

## Ontology

- **Namespace:** `https://purl.org/packagegraph/ontology/dq#`
- **File:** `ontology/dq.ttl`
- **Version:** 0.1.0

### Classes

| Class | Description |
|-------|-------------|
| `dq:DataQualityIssue` | A recorded data quality issue found during ETL |

### Properties

| Property | Domain | Range | Description |
|----------|--------|-------|-------------|
| `dq:hasQualityIssue` | any resource | `dq:DataQualityIssue` | Links resource to its quality issues |
| `dq:issueType` | `dq:DataQualityIssue` | `xsd:string` | Category (e.g., `dead-repo`, `malformed-email`) |
| `dq:field` | `dq:DataQualityIssue` | `xsd:string` | Affected field (dot notation for nested) |
| `dq:rawValue` | `dq:DataQualityIssue` | `xsd:string` | The problematic value (truncated to 500 chars) |
| `dq:detectedBy` | `dq:DataQualityIssue` | `xsd:string` | ETL component that found the issue |
| `dq:detectedAt` | `dq:DataQualityIssue` | `xsd:dateTime` | When the issue was first detected |
| `dq:severity` | `dq:DataQualityIssue` | `xsd:string` | `info`, `warning`, or `error` |

## Adding Quality Checks to New Components

Any enricher extending `BaseEnricher` can flag issues:

```python
# In _process_item():
if is_bad(value):
    self.writer.flag_quality_issue(
        subject_uri=resource_uri,
        issue_type="my-issue-type",
        field="field.name",
        value=raw_value,
        source="my-enricher-name"
    )
```

Issues are written to the enrichment output `.nt` file and loaded into the enrichment graph alongside the enrichment triples.

## Incremental Enrichment

Enrichers track per-resource freshness via `pkg:enrichedAt` timestamps written into their named graph. On subsequent runs, only stale or new resources are processed.

### How It Works

1. Each enricher writes `<resource> pkg:enrichedAt "2026-04-14T..."` for every resource it processes
2. The named graph scopes the timestamp to the enricher (`graph/enrichment/github-vcs`, etc.)
3. On the next run, `_query_packages()` queries the enrichment graph for resources enriched within `ENRICHER_FRESHNESS_DAYS` and skips them
4. New triples are POSTed (appended) to the graph — no DROP, so prior data persists
5. Set `ENRICHER_FRESHNESS_DAYS=0` to force full re-enrichment (drops graph first)

### Querying Freshness

```sparql
# Repos not enriched by VCS enricher in the last 7 days
PREFIX vcs: <https://purl.org/packagegraph/ontology/vcs#>
PREFIX pkg: <https://purl.org/packagegraph/ontology/core#>
SELECT ?repoUrl ?lastEnriched WHERE {
  GRAPH <https://packagegraph.github.io/graph/enrichment/github-vcs> {
    ?r vcs:repositoryURL ?repoUrl .
    ?r pkg:enrichedAt ?lastEnriched .
    FILTER(?lastEnriched < "2026-04-07T00:00:00"^^xsd:dateTime)
  }
}

# Repos enriched by VCS but NOT by license enricher
PREFIX vcs: <https://purl.org/packagegraph/ontology/vcs#>
PREFIX pkg: <https://purl.org/packagegraph/ontology/core#>
SELECT ?repoUrl WHERE {
  GRAPH <https://packagegraph.github.io/graph/enrichment/github-vcs> {
    ?r vcs:repositoryURL ?repoUrl .
  }
  FILTER NOT EXISTS {
    GRAPH <https://packagegraph.github.io/graph/enrichment/license> {
      ?r2 pkg:enrichedAt ?ts .
    }
  }
}
```

## Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `ENRICHER_CACHE_DISABLED` | `0` | Set to `1` to skip Minio cache sync |
| `ENRICHER_CACHE_SYNC_INTERVAL` | `500` | Items between periodic cache syncs to Minio |
| `ENRICHER_FRESHNESS_DAYS` | `7` | Skip repos enriched within this many days. Set to `0` for full re-enrichment |
| `PYTHONUNBUFFERED` | `1` (set in jobs) | Unbuffered output for real-time logs |
| `GITHUB_TOKEN` | (from `github-token` secret) | GitHub API authentication (fine-grained PAT) |

### GitHub PAT Scopes (Fine-Grained)

- **Repository access:** Public Repositories (read-only)
- **Permissions:** Metadata: Read-only, Contents: Read-only

## Validation Job

The `validate-data` CronJob runs weekly and checks for common issues:

- Homepage set to `"None"` (Python None serialized)
- Non-HTTP homepages (FTP, etc.)
- Package names with spaces
- Empty version strings
- Packages without versions
- Dead upstream repos (404)
- Graph statistics summary

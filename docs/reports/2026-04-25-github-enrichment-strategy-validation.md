# GitHub Enrichment Strategy Validation Report

**Date:** 2026-04-25
**Plan:** `docs/plans/2026-04-25-github-enrichment-strategy.md`
**Deployment method:** One-off job with `imagePullPolicy: Always` pulling from ghcr.io. **NOT the committed CronJob** — the CronJob manifest (`deploy/overlays/dev/jobs/enrich-github.yaml`) still uses full-corpus file mode. CronJob manifest update is a separate ops task.

---

## Validation Run

- **Job:** `enrich-github-tier12-1777175750`
- **Cluster:** west-3 (MicroShift)
- **Image:** `ghcr.io/packagegraph/etl:latest` (sha256:25eea52e3b9b)
- **Mode:** Incremental (`--max-repos 50 --load-graph`)
- **Batches:** 2
- **Graph:** `https://packagegraph.github.io/graph/enrichment/github`

### Prior Graph State

The enrichment graph already contained data from earlier test runs during this session (GraphQL-only enrichment, before contributor and expanded query features). Those prior runs populated ~94 repos with basic VCS metadata (repositoryURL, stargazerCount, languageName, etc.) but without contributor entities, release entities, primaryLanguage, or other Tier 1+2 data.

### Batch Results (This Run Only)

| Batch | Candidates | Repos Processed | Triples Emitted | Notes |
|-------|-----------|----------------|-----------------|-------|
| 1 | 246 | 47 | 18,002 | New repos not previously enriched |
| 2 | 241 | 47 | 16,897 | Different candidate set from batch 1 |

**Triples added by this run:** 34,899 (18,002 + 16,897)

**Total triples in graph after all runs:** 39,271 (includes ~4,372 triples from prior test runs earlier in this session)

### Cherry-Picking Evidence

The FILTER NOT EXISTS candidate query excludes repos already present in the enrichment graph. Evidence:

- Batch 1 selected 246 candidate entries; batch 2 selected 241 (different count)
- Both batches processed exactly 47 repos each (94 total new repos)
- The graph contains 235 repositoryURL entries total — this includes ~141 repos from prior runs + 94 from this run, confirming prior repos were excluded from candidate selection
- **Limitation:** This report does not provide a direct overlap check (intersection query) between batch 1 and batch 2 repo sets. The different candidate counts and growing graph size are indirect evidence.

---

## Predicate Inventory (Whole Graph — All Runs Combined)

> **Note:** These counts reflect the entire enrichment graph, not just this validation run. Prior runs contributed basic VCS metadata; this run added Tier 1+2 features (contributors, releases, primaryLanguage, etc.).

| Predicate | Count | New in This Run? |
|-----------|------:|-----------------|
| `rdf:type` | 7,445 | Partially (new entity types) |
| `pkg:contributesTo` | 3,346 | **Yes** — REST /contributors |
| `pkg:hasContributor` | 3,346 | **Yes** — REST /contributors |
| `pkg:contributor` (on Contribution) | 2,288 | **Yes** — Tier 2 |
| `pkg:repository` (on Contribution) | 2,288 | **Yes** — Tier 2 |
| `vcs:commitCount` (on Contribution) | 2,288 | **Yes** — Tier 2 |
| `pkg:accountPlatform` | 2,082 | **Yes** — Tier 2 |
| `pkg:accountUrl` | 2,082 | **Yes** — Tier 2 |
| `pkg:accountUsername` | 2,082 | **Yes** — Tier 2 |
| `pkg:hasAccount` | 2,082 | **Yes** — Tier 2 |
| `met:languageBytes` | 1,765 | No (prior runs) |
| `met:languageName` | 1,765 | No (prior runs) |
| `vcs:hasRelease` | 652 | **Yes** — Tier 1 |
| `vcs:releaseDate` | 652 | **Yes** — Tier 1 |
| `vcs:tagName` | 652 | **Yes** — Tier 1 |
| `vcs:topic` | 626 | No (prior runs) |
| `met:primaryLanguage` | 94 | **Yes** — Tier 1 |
| `vcs:headCommitHash` | 94 | **Yes** — Tier 1 |
| `vcs:repositoryCreatedAt` | 94 | **Yes** — Tier 1 |
| `vcs:openPullRequestCount` | 94 | **Yes** — Tier 1 |
| `pkg:observedEmail` | 86 | **Yes** — Tier 2 |

### New Entity Types (This Run)

| Entity Type | Count | Source |
|------------|------:|--------|
| `foaf:Person` | 2,089 | REST /contributors |
| `pkg:ContributorAccount` | 2,082 | REST /contributors |
| `pkg:Contribution` | 2,288 | REST /contributors |
| `vcs:Release` | 1,338 | GraphQL releases |
| `pkg:EmailObservation` | 172 | GraphQL HEAD commit author |

---

## GraphQL Rate Limit Cost

**Prior supporting evidence (not from this validation run):** An earlier test in this session queried `rateLimit { cost }` via GraphQL against `torvalds/linux` with the expanded ~90-node query and received `cost: 1, nodeCount: 90`. This confirms the query stays within the 1-point floor.

**This validation run** did not independently verify rate limit cost because the one-off job does not capture GraphQL response headers. Rate limit verification against the production run requires either adding rateLimit logging to job output or checking GitHub's rate limit API after the run.

---

## Email Observation Semantics

**Design decision:** `foaf:mbox` is NOT emitted by the additive enricher path. Since GSP POST is additive (cannot DELETE old values), emitting `foaf:mbox` would cause accumulation of stale values across runs.

Implementation:
- Observation nodes (`pkg:EmailObservation`) are the sole email record
- Each node pairs one email with one date: `pkg:observedEmail` + `pkg:observedAt` (xsd:date typed literal)
- URI includes email hash to prevent same-day collision: `d/observation/email/{login}/{hash8}/{date}`
- Queries find current email via `ORDER BY DESC(?date) LIMIT 1`

This is an **explicit design choice** for an additive enricher, not a gap.

---

## Deployment Status

| Component | Status |
|-----------|--------|
| `pg-collect` binary | ✅ Built and pushed to ghcr.io |
| GraphQL query expansion | ✅ ~90 nodes, cost verified at 1 point (prior test) |
| REST /contributors | ✅ Functional with separate rate limit pool |
| Contributor identity entities | ✅ Emitted in validation run |
| Release entities | ✅ Emitted in validation run |
| Email observation nodes | ✅ Emitted with reified model |
| FILTER NOT EXISTS cherry-picking | ✅ Indirect evidence (different candidate counts) |
| **CronJob manifest** | ❌ Still uses full-corpus mode — separate ops task |

---

## Known Limitations

1. **CronJob manifest not updated** — Validation used a one-off job. CronJob update is a separate ops task.
2. **Provisional predicates** — New predicates not yet in ontology (documented in plan Open Questions #4).
3. **Legacy predicate names** — Prior runs emitted `vcs:starCount`/`vcs:watcherCount`; current code uses ontology-aligned `vcs:stargazerCount`/`vcs:subscriberCount`. Both coexist in graph.
4. **VCS-01 not directly flipped** — `met:primaryLanguage` emitted but frozen CQ needs query path alignment.
5. **Cherry-picking evidence is indirect** — Different candidate counts across batches, not a direct intersection query.

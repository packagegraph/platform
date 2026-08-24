# Fuseki Archive — Emergency Rollback

Archived when QLever was deployed alongside Fuseki. Fuseki remains active
during migration; these copies exist for reference if the base manifests
are removed later.

## Rollback procedure

1. Re-add Fuseki resources to `deploy/base/kustomization.yaml`
2. Re-apply: `oc apply -k deploy/base/`
3. Reload data from Minio .nt files via `pg-collect load`

#!/usr/bin/env bash
# v060-pipeline.sh — Orchestrate the full v0.6.0 rebuild pipeline
#
# Phase 1: Wait for current collectors (pypi, chocolatey, advisory)
# Phase 2: rebuild-tdb2 → loads all N-Triples into Fuseki
# Phase 3: Fuseki-dependent ecosystem collectors (maven, hackage, hex, nuget, npm)
# Phase 4: Second rebuild-tdb2 → incorporates ecosystem data
#
# Usage: ./v060-pipeline.sh

set -euo pipefail
export KUBECONFIG="${KUBECONFIG:-$HOME/k8s1.west-3.kafka.tel.yaml}"
NS=packagegraph

wait_for_jobs() {
    local jobs=("$@")
    echo "  Waiting for: ${jobs[*]}"
    while true; do
        all_done=true
        for job in "${jobs[@]}"; do
            status=$(oc get job "$job" -n "$NS" -o jsonpath='{.status.conditions[0].type}' 2>/dev/null || echo "Unknown")
            if [ "$status" = "Complete" ]; then
                continue
            elif [ "$status" = "Failed" ]; then
                echo "  ✗ $job FAILED"
                oc logs -n "$NS" -l "job-name=$job" --tail=5 2>/dev/null
                return 1
            else
                all_done=false
            fi
        done
        if [ "$all_done" = true ]; then
            echo "  ✓ All jobs complete"
            return 0
        fi
        sleep 30
        # Progress indicator
        for job in "${jobs[@]}"; do
            status=$(oc get job "$job" -n "$NS" -o jsonpath='{.status.conditions[0].type}' 2>/dev/null || echo "Running")
            printf "    %-50s %s\n" "$job" "$status"
        done
    done
}

echo "=== v0.6.0 Rebuild Pipeline ==="
echo "Started: $(date -Iseconds)"
echo ""

# ─── PHASE 1: Wait for current collectors ───
echo "── Phase 1: Waiting for pypi + chocolatey + advisory ──"
PHASE1_JOBS=()
for job in $(oc get jobs -n "$NS" --no-headers 2>/dev/null | grep v060 | awk '$2 != "Failed" {print $1}'); do
    PHASE1_JOBS+=("$job")
done
if [ ${#PHASE1_JOBS[@]} -gt 0 ]; then
    wait_for_jobs "${PHASE1_JOBS[@]}" || echo "  (continuing despite failures)"
else
    echo "  No Phase 1 jobs running"
fi
echo ""

# ─── PHASE 2: Rebuild TDB2 ───
echo "── Phase 2: Rebuild TDB2 (all N-Triples → Fuseki) ──"
oc create job -n "$NS" rebuild-tdb2-v060 --from=cronjob/rebuild-tdb2 2>/dev/null && echo "  ✓ rebuild-tdb2 triggered"
wait_for_jobs "rebuild-tdb2-v060" || { echo "TDB2 rebuild failed!"; exit 1; }
echo ""

# ─── PHASE 3: Fuseki-dependent ecosystem collectors ───
echo "── Phase 3: Ecosystem collectors (seeded from rebuilt Fuseki) ──"
PHASE3_JOBS=()
for cj in collect-maven collect-hackage collect-hex collect-nuget collect-npm; do
    oc create job -n "$NS" "${cj}-v060p3" --from="cronjob/${cj}" 2>/dev/null && echo "  ✓ $cj" && PHASE3_JOBS+=("${cj}-v060p3")
done
wait_for_jobs "${PHASE3_JOBS[@]}" || echo "  (continuing despite failures)"
echo ""

# ─── PHASE 4: Second TDB2 rebuild ───
echo "── Phase 4: Final TDB2 rebuild (incorporating ecosystem data) ──"
oc create job -n "$NS" rebuild-tdb2-v060-final --from=cronjob/rebuild-tdb2 2>/dev/null && echo "  ✓ rebuild-tdb2 triggered"
wait_for_jobs "rebuild-tdb2-v060-final" || { echo "Final TDB2 rebuild failed!"; exit 1; }
echo ""

echo "=== Pipeline complete: $(date -Iseconds) ==="
echo ""
echo "Next steps:"
echo "  1. Take a post-migration snapshot:"
echo "     DATASET_LABEL=v0.6.0-post-migration bash etl/scripts/dataset-snapshot.sh"
echo "  2. Compare with baseline:"
echo "     bash etl/scripts/dataset-snapshot.sh --diff etl/snapshots/dataset-v05x-baseline.json etl/snapshots/dataset-<new>.json"

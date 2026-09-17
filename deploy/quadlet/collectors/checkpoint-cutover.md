# Coordinated collector checkpoint cutover

Checkpointing stays automatic in `rpm-full` when `--cache-dir` is supplied.
There is no compatibility flag. The binary, host-mounted wrappers, and image's
`upload-nt.sh` are a single release. A merge is **not** a deployment transaction:
the existing host can auto-update its image without updating its wrappers.

This procedure is an operator action, not something CI executes. Do the freeze
**before merging/publishing the first checkpoint-enabled image**. If that image
has already published, freeze immediately and inspect installed wrappers and
active generations before running another collection. Do not claim the rollout
is complete merely because the checks in the checkout pass.

The shared template runs non-RPM collectors too. This maintenance window pauses
all collectors using either template, plus host-wide Podman auto-update. Agree
the window with the host owner. Never reboot during this procedure: runtime
masks do not survive reboot. Stop if an involved unit is already masked; preserve
and reconcile its prior state rather than blindly unmasking it later.

## 1. Freeze and save a rollback pair

Run as root on the host in Bash, with `set -euo pipefail`. Use a private backup
directory: the Quadlet tree includes credential files. Keep the directory and
the recorded image digest until the release has been accepted.

```bash
rollback_dir=$(mktemp -d /var/tmp/collector-cutover.XXXXXX)
chmod 700 "$rollback_dir"
systemctl list-unit-files 'pg-collect-*.timer' --no-legend --no-pager > "$rollback_dir/timers"
mapfile -t collector_timers < <(awk '{print $1}' "$rollback_dir/timers")
test "${#collector_timers[@]}" -gt 0
: > "$rollback_dir/previously-active"
for unit in podman-auto-update.timer podman-auto-update.service \
    pg-collect@.service pg-collect-rhel@.service "${collector_timers[@]}"; do
    enabled_state=$(systemctl is-enabled "$unit" || true)
    printf '%s %s\n' "$unit" "$enabled_state" >> "$rollback_dir/enabled-state"
    case "$enabled_state" in masked*) echo "Reconcile existing mask: $unit" >&2; exit 1 ;; esac
    if systemctl is-active --quiet "$unit"; then
        echo "$unit" >> "$rollback_dir/previously-active"
    fi
done

# Stop any updater already running, not just its timer. No image may change
# while the rollback pair is captured and the new release is installed.
systemctl mask --runtime --now podman-auto-update.timer podman-auto-update.service
systemctl mask --runtime --now "${collector_timers[@]}"
systemctl stop 'pg-collect@*.service' 'pg-collect-rhel@*.service'
systemctl mask --runtime pg-collect@.service pg-collect-rhel@.service
cp -a /etc/containers/systemd "$rollback_dir/quadlet"
systemctl list-units --all 'pg-collect@*.service' 'pg-collect-rhel@*.service' \
    --no-pager > "$rollback_dir/collector-state"
```

Check that **no collector instance is running, activating, or deactivating** and
that no separately defined instance service bypasses the template masks. A
stopped collection leaves its active generation intact; do not commit it merely
to facilitate maintenance. Draining already-running collections before stopping
is also valid, provided scheduling and auto-update have already been frozen.

Record the currently known-good ETL digest (not just `devel-latest`) in
`$rollback_dir/previous-image`, and retain/pull that image. Use the digest of the
actual running release from the host's deployment record/container inspection;
do not assume the current floating tag still identifies it. If no known-good
image/wrapper pair can be established, stop here and resolve that first.

## 2. Stage a matching, immutable release

Use the exact clean checkout whose CI built the new image. Obtain its immutable
digest from that build's published image, not an unrelated floating tag. The
image workflow labels images with `org.opencontainers.image.revision` so the
association can be checked on the host.

Set `ETL_IMAGE` to `ghcr.io/packagegraph/etl@sha256:<64 hex>` and `release_dir` to
a **new, non-existing** directory under an existing private staging directory.
These commands fail on missing variables, floating tags, existing staging
directories, incorrect revisions, or a missing checkpoint command:

```bash
test -z "$(git status --porcelain)"
revision=$(git rev-parse HEAD)
python3 deploy/quadlet/collectors/checkpoint-release.py stage "${ETL_IMAGE:?}" "${release_dir:?}"
podman pull "$ETL_IMAGE"
test "$(podman image inspect --format '{{ index .Labels "org.opencontainers.image.revision" }}' "$ETL_IMAGE")" = "$revision"
podman run --rm --pull=never --network=none --entrypoint pg-collect "$ETL_IMAGE" checkpoint commit --help
```

The staged templates pin `Image=` to that digest and remove `AutoUpdate=`.
Both templates and all ten RPM wrappers are included. Do not subsequently run
the generic template install command from the README over these files.
Future collector upgrades use another coordinated release, not image-only
auto-update. Other applications' auto-update settings are unchanged.

## 3. Install and verify while scheduling is still frozen

```bash
install -m 644 "$release_dir/pg-collect@.container" "$release_dir/pg-collect-rhel@.container" /etc/containers/systemd/
install -d /etc/containers/systemd/scripts/collectors
install -m 755 "$release_dir"/scripts/collectors/*.sh /etc/containers/systemd/scripts/collectors/
python3 deploy/quadlet/collectors/checkpoint-release.py verify "$ETL_IMAGE" /etc/containers/systemd
systemctl unmask --runtime pg-collect@.service pg-collect-rhel@.service
systemctl daemon-reload
```

The verifier compares installed files with this checkout's pinned release,
requires executable wrappers, runs the wrapper contract check, and rejects
local collector/generic Quadlet overrides. It does **not** resolve all Quadlet
search paths or systemd service drop-ins. Inspect the generated effective units
before allowing a run:

```bash
systemctl cat pg-collect@fedora-44-full.service pg-collect-rhel@rhel-9-full.service
systemctl show -p ExecStart -p DropInPaths pg-collect@fedora-44-full.service pg-collect-rhel@rhel-9-full.service
```

Require the expected image digest, the expected host script bind mount, and no
`io.containers.autoupdate=registry` label in **both** effective templates. Inspect
any instance overrides for every installed collector, including alternative
Quadlet roots such as `/run/containers/systemd` and
`/usr/share/containers/systemd`. Reconcile overrides explicitly; do not silently
delete them. A failed verification leaves timers and auto-update masked. Do not
resume on failure or install one half of a release and hope the other follows.

## 4. Rehearse, smoke-test, then resume

Before deployment, CI must have passed all of:

```bash
cargo test --manifest-path etl/pg-collect/Cargo.toml --all-targets --locked
python3 -m unittest discover -s deploy/quadlet/collectors/tests -p test_checkpoint_release.py
PG_COLLECT_BIN="$PWD/etl/pg-collect/target/debug/pg-collect" \
    python3 deploy/quadlet/collectors/tests/checkpoint_mutations.py
```

The lifecycle integration test runs the real shell wrapper, collector, caches,
upload script and commit. It covers failed data upload, failed sidecar upload,
failed collection, retryable RPC, inconclusive spec fetch, disabled checkpoint
setup, retained-generation replay, successful retirement, and a fresh
generation reusing valid source entries. Both checkpointed stages are covered:
specs are served from a local dist-git fixture via
`PG_COLLECT_DIST_GIT_BASE`, so the rehearsal issues no outbound requests. CI
also runs the image's pinned `mc` on local directories to test its actual
exclusions, not the object-store adapter's implementation.

With timers still masked, manually run one known RPM collector against the new
release (for example `systemctl start pg-collect@fedora-44-full.service`). This
publishes real data: run it only inside the agreed maintenance window. Verify
its upload succeeds **before** its generation becomes `complete`; verify the
local checkpoint directory remains intact until the next generation; verify
no checkpoint objects are mirrored to Minio. A pre-existing remote `output/`
requires investigation, not deletion with these credentials. Check one actual
timeout/restart and record replay time, bytes and inodes before accepting the
feature operationally. A repeated collection must mint a different generation.

After these checks, unmask the timers changed in step 1 and the auto-updater;
start **only** timers recorded as previously active. Do not change enablement,
unmask pre-existing administrative masks, or force an immediate auto-update:

```bash
systemctl unmask --runtime "${collector_timers[@]}" podman-auto-update.timer podman-auto-update.service
while IFS= read -r unit; do
    case "$unit" in *.timer) systemctl start "$unit" ;; esac
done < "$rollback_dir/previously-active"
```

## Rollback is also a paired cutover

On any failure, leave scheduling and auto-update frozen. Reapply the freeze if
they have already resumed. Restore **both** saved templates and all saved RPM
wrapper files from `$rollback_dir/quadlet`, including the contract script if it
existed. Render the restored templates with `Image=` pinned to the recorded
known-good digest and remove `AutoUpdate=`; do not restore a floating image
reference. Restore any overrides deliberately changed during the rollout too.
Pull that exact old image, reload systemd, inspect effective units, and smoke-test
the old image/wrapper pair before restoring the previously active timers.

Do not delete caches or manually commit an unpublished generation. If the old
wrappers predate checkpoint exclusions, move each collector's local `output/`
directory to a private rollback archive **outside its mirrored cache root**
before starting the old release. This preserves recovery data and prevents the
old mirror commands uploading checkpoints to Minio. Move the archived directory
back only during a later frozen cutover to the matching checkpoint-aware release.

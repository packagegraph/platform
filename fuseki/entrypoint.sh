#!/bin/sh
set -eu

TDB2_DIR="/data/tdb2"
MINIO_ALIAS="pgraph"

# Load TDB2 from Minio if credentials are configured
if [ -n "${MINIO_ACCESS_KEY:-}" ]; then
    echo "=== Fuseki: Loading TDB2 from Minio ==="

    # Configure mc alias
    mc alias set "${MINIO_ALIAS}" "${MINIO_ENDPOINT}" "${MINIO_ACCESS_KEY}" "${MINIO_SECRET_KEY}" --api S3v4 >/dev/null 2>&1

    # Read latest pointer
    CONTENT_HASH=$(mc cat "${MINIO_ALIAS}/${MINIO_BUCKET}/tdb2/latest")
    echo "Latest snapshot: ${CONTENT_HASH}"

    # Check if TDB2 already loaded with this hash
    HASH_FILE="/data/.content-hash"
    if [ -f "$HASH_FILE" ] && [ "$(cat "$HASH_FILE")" = "$CONTENT_HASH" ]; then
        echo "TDB2 already loaded with ${CONTENT_HASH}, skipping download."
    else
        # Download and extract TDB2 snapshot
        REMOTE_PATH="tdb2/${CONTENT_HASH}/tdb2.tar.gz"
        echo "Downloading ${REMOTE_PATH}..."
        mc cp "${MINIO_ALIAS}/${MINIO_BUCKET}/${REMOTE_PATH}" /tmp/tdb2.tar.gz

        echo "Extracting TDB2..."
        rm -rf "$TDB2_DIR"
        mkdir -p "$TDB2_DIR"
        tar -xzf /tmp/tdb2.tar.gz -C /data/
        rm /tmp/tdb2.tar.gz
        echo "$CONTENT_HASH" > "$HASH_FILE"
    fi

    echo "TDB2 ready at ${TDB2_DIR} (${CONTENT_HASH})"

    # If --init-only, exit here (used as init container)
    if [ "${1:-}" = "--init-only" ]; then
        echo "Init complete."
        exit 0
    fi
else
    echo "=== Fuseki: No Minio credentials, starting with local data ==="
fi

echo "=== Starting Fuseki ==="
exec "$JAVA_HOME/bin/java" $JAVA_OPTIONS -jar "${FUSEKI_DIR}/${FUSEKI_JAR}" \
    --config /fuseki/config.ttl \
    --port 3030

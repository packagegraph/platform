# RHEL Package Collection

Collecting Red Hat Enterprise Linux package metadata requires TLS client certificate
authentication against the Red Hat CDN (`cdn.redhat.com`). This guide covers setup
and operation for RHEL 9 and RHEL 10.

## Prerequisites

A RHEL system registered with `subscription-manager` and a valid entitlement.
Simple Content Access (SCA) subscriptions work — no per-product attachment needed.

### Required Files (from the registered host)

| File | Location | Purpose |
|------|----------|---------|
| Entitlement certificate | `/etc/pki/entitlement/<ID>.pem` | Client TLS certificate |
| Entitlement key | `/etc/pki/entitlement/<ID>-key.pem` | Client TLS private key |
| Red Hat CA | `/etc/rhsm/ca/redhat-uep.pem` | CA to verify CDN server |

The entitlement certificate ID is unique per system. Find it with:

```bash
ls /etc/pki/entitlement/*.pem | grep -v key
```

Certificates are auto-renewed by `subscription-manager` (typically valid for ~1 year).
Check expiration with:

```bash
openssl x509 -in /etc/pki/entitlement/<ID>.pem -noout -enddate
```

## Podman (Local Testing)

Bind-mount the entitlement certs into the container:

```bash
CERT=$(ls /etc/pki/entitlement/*.pem | grep -v key | head -1)
KEY=$(ls /etc/pki/entitlement/*-key.pem | head -1)
CA=/etc/rhsm/ca/redhat-uep.pem

# RHEL 9 — BaseOS + AppStream
podman run --rm --entrypoint bash \
  -v /etc/pki/entitlement:/etc/pki/entitlement:ro \
  -v /etc/rhsm/ca:/etc/rhsm/ca:ro \
  ghcr.io/packagegraph/etl:latest -c "
    pg-collect rpm \
      --rpm-repo rhel:9:https://cdn.redhat.com/content/dist/rhel9/9/x86_64/baseos/os \
      --rpm-repo rhel:9:https://cdn.redhat.com/content/dist/rhel9/9/x86_64/appstream/os \
      --sslclientcert $CERT \
      --sslclientkey $KEY \
      --sslcacert $CA \
      --output /tmp/rhel9.nt
  "

# RHEL 10 — BaseOS + AppStream
podman run --rm --entrypoint bash \
  -v /etc/pki/entitlement:/etc/pki/entitlement:ro \
  -v /etc/rhsm/ca:/etc/rhsm/ca:ro \
  ghcr.io/packagegraph/etl:latest -c "
    pg-collect rpm \
      --rpm-repo rhel:10:https://cdn.redhat.com/content/dist/rhel10/10/x86_64/baseos/os \
      --rpm-repo rhel:10:https://cdn.redhat.com/content/dist/rhel10/10/x86_64/appstream/os \
      --sslclientcert $CERT \
      --sslclientkey $KEY \
      --sslcacert $CA \
      --output /tmp/rhel10.nt
  "
```

### Additional Repos

| Repo | CDN Path |
|------|----------|
| BaseOS | `content/dist/rhel{VER}/{VER}/x86_64/baseos/os` |
| AppStream | `content/dist/rhel{VER}/{VER}/x86_64/appstream/os` |
| CodeReady Builder / CRB | `content/dist/rhel{VER}/{VER}/x86_64/codeready-builder/os` |

## Kubernetes Deployment

### 1. Create the Entitlement Secret

Copy the three files from a registered RHEL host into a K8s secret:

```bash
# On the registered host:
CERT=$(ls /etc/pki/entitlement/*.pem | grep -v key | head -1)
KEY=$(ls /etc/pki/entitlement/*-key.pem | head -1)

oc create secret generic rhel-entitlement -n packagegraph \
  --from-file=cert.pem=$CERT \
  --from-file=key.pem=$KEY \
  --from-file=ca.pem=/etc/rhsm/ca/redhat-uep.pem
```

### 2. CronJob Manifest

```yaml
apiVersion: batch/v1
kind: CronJob
metadata:
  name: collect-rhel-9
  namespace: packagegraph
spec:
  schedule: "0 2 * * 0"
  concurrencyPolicy: Forbid
  successfulJobsHistoryLimit: 1
  failedJobsHistoryLimit: 1
  jobTemplate:
    spec:
      backoffLimit: 2
      ttlSecondsAfterFinished: 600
      template:
        spec:
          restartPolicy: Never
          imagePullSecrets: [{name: ghcr-pull-secret}]
          securityContext:
            runAsNonRoot: true
            seccompProfile: {type: RuntimeDefault}
          containers:
            - name: etl
              image: ghcr.io/packagegraph/etl:latest
              imagePullPolicy: Always
              securityContext:
                allowPrivilegeEscalation: false
                capabilities: {drop: [ALL]}
              env:
                - {name: HOME, value: /tmp}
                - {name: FUSEKI_ENDPOINT, value: "http://fuseki.packagegraph.svc:3030/packagegraph"}
              command: ["/bin/sh", "-c"]
              args:
                - |
                  set -eu
                  pg-collect rpm \
                    --rpm-repo rhel:9:https://cdn.redhat.com/content/dist/rhel9/9/x86_64/baseos/os \
                    --rpm-repo rhel:9:https://cdn.redhat.com/content/dist/rhel9/9/x86_64/appstream/os \
                    --sslclientcert /etc/pki/entitlement/cert.pem \
                    --sslclientkey /etc/pki/entitlement/key.pem \
                    --sslcacert /etc/pki/entitlement/ca.pem \
                    --output /tmp/rhel9.nt && \
                  pg-collect drop \
                    --graph "https://packagegraph.github.io/graph/rhel/9" \
                    --endpoint "$FUSEKI_ENDPOINT" && \
                  pg-collect load /tmp/rhel9.nt \
                    --graph "https://packagegraph.github.io/graph/rhel/9" \
                    --endpoint "$FUSEKI_ENDPOINT"
              volumeMounts:
                - name: entitlement
                  mountPath: /etc/pki/entitlement
                  readOnly: true
              resources:
                requests: {cpu: 500m, memory: 1Gi}
                limits: {cpu: "2", memory: 4Gi}
          volumes:
            - name: entitlement
              secret:
                secretName: rhel-entitlement
```

### 3. Certificate Renewal

Entitlement certificates are renewed automatically by `subscription-manager` on the
registered host. When certificates are renewed (typically yearly), update the K8s
secret:

```bash
# On the registered host:
CERT=$(ls /etc/pki/entitlement/*.pem | grep -v key | head -1)
KEY=$(ls /etc/pki/entitlement/*-key.pem | head -1)

oc delete secret rhel-entitlement -n packagegraph
oc create secret generic rhel-entitlement -n packagegraph \
  --from-file=cert.pem=$CERT \
  --from-file=key.pem=$KEY \
  --from-file=ca.pem=/etc/rhsm/ca/redhat-uep.pem
```

## Tested Results

| Release | Repos | Packages | Triples | Time |
|---------|-------|----------|---------|------|
| RHEL 9 | BaseOS | 13,532 | 2,016,799 | 27s |
| RHEL 10 | BaseOS + AppStream | 13,221 | 2,033,435 | 17s |

## Troubleshooting

**"Failed to parse client certificate + key"** — Verify the cert and key files are
valid PEM format. The key should start with `-----BEGIN PRIVATE KEY-----` (PKCS#8).

**"error sending request for url"** — Check that the CA cert (`redhat-uep.pem`) is
mounted and the path is correct. This cert is needed to verify the CDN's TLS
certificate.

**"404 Not Found" on repodata** — The CDN path structure follows
`content/dist/rhel{MAJOR}/{MAJOR}/x86_64/{repo}/os/`. Verify the version number
matches an active RHEL release.

**Certificate expired** — Run `subscription-manager refresh` on the registered host,
then recreate the K8s secret.

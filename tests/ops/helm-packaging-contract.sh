#!/bin/sh
set -eu

repository=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
chart="$repository/charts/memeloop-token-center"
workspace=$(mktemp -d "${TMPDIR:-/tmp}/mtc-helm-contract.XXXXXX")
trap 'rm -rf -- "$workspace"' EXIT HUP INT TERM

helm_binary=${HELM_BIN:-helm}
kubeconform_binary=${KUBECONFORM_BIN:-}

"$helm_binary" lint --strict "$chart"
"$helm_binary" lint --strict "$chart" --values "$chart/values-dev.yaml"

"$helm_binary" template token-center "$chart" \
  --namespace token-center >"$workspace/default.yaml"
"$helm_binary" template token-center-dev "$chart" \
  --namespace token-center-dev \
  --values "$chart/values-dev.yaml" >"$workspace/dev.yaml"
"$helm_binary" template token-center-observed "$chart" \
  --namespace token-center \
  --set serviceMonitor.enabled=true \
  --set roles.gateway.autoscaling.enabled=true >"$workspace/observed.yaml"
reviewed_digest=sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
"$helm_binary" template token-center-digest "$chart" \
  --namespace token-center \
  --set-string image.tag=must-not-render \
  --set-string image.digest="$reviewed_digest" >"$workspace/digest.yaml"
"$helm_binary" template token-center-configmap-plugin "$chart" \
  --namespace token-center \
  --set plugins.enabled=true \
  --set plugins.existingConfigMap=token-center-plugins >"$workspace/configmap-plugin.yaml"
"$helm_binary" template token-center-pvc-plugin "$chart" \
  --namespace token-center \
  --set plugins.enabled=true \
  --set plugins.existingClaim=token-center-plugins >"$workspace/pvc-plugin.yaml"
"$helm_binary" template token-center-gateway-ingress "$chart" \
  --namespace token-center \
  --show-only templates/ingress.yaml \
  --set ingress.gateway.enabled=true \
  --set ingress.gateway.className=public-gateway \
  --set-string ingress.gateway.annotations.marker=gateway-only \
  --set ingress.gateway.host=gateway.example.test \
  --set ingress.gateway.tlsSecretName=gateway-tls >"$workspace/gateway-ingress.yaml"
"$helm_binary" template token-center-control-ingress "$chart" \
  --namespace token-center \
  --show-only templates/ingress.yaml \
  --set ingress.control.enabled=true \
  --set ingress.control.className=higress-private \
  --set ingress.control.sourceRanges[0]=10.0.0.0/8 \
  --set-string ingress.control.annotations.marker=control-only \
  --set ingress.control.host=control.internal.example.test \
  --set ingress.control.tlsSecretName=control-tls >"$workspace/control-ingress.yaml"
"$helm_binary" template token-center-both-ingresses "$chart" \
  --namespace token-center \
  --show-only templates/ingress.yaml \
  --set ingress.gateway.enabled=true \
  --set ingress.gateway.className=public-gateway \
  --set-string ingress.gateway.annotations.marker=gateway-only \
  --set ingress.gateway.host=gateway.example.test \
  --set ingress.gateway.tlsSecretName=gateway-tls \
  --set ingress.control.enabled=true \
  --set ingress.control.className=higress-private \
  --set ingress.control.sourceRanges[0]=10.0.0.0/8 \
  --set-string ingress.control.annotations.marker=control-only \
  --set ingress.control.host=control.internal.example.test \
  --set ingress.control.tlsSecretName=control-tls >"$workspace/both-ingresses.yaml"
"$helm_binary" template token-center-gateway-load-balancer "$chart" \
  --namespace token-center \
  --show-only templates/service.yaml \
  --set roles.gateway.service.type=LoadBalancer >"$workspace/gateway-load-balancer.yaml"

grep -q 'kind: NetworkPolicy' "$workspace/default.yaml"
grep -q 'kind: PodDisruptionBudget' "$workspace/default.yaml"
grep -q 'kind: HorizontalPodAutoscaler' "$workspace/observed.yaml"
grep -q 'kind: ServiceMonitor' "$workspace/observed.yaml"
test "$(grep -c 'image: \"ghcr.io/linonetwo/memeloop-token-center:0.1.0\"' "$workspace/default.yaml")" -eq 4
test "$(grep -c "image: \"ghcr.io/linonetwo/memeloop-token-center@$reviewed_digest\"" "$workspace/digest.yaml")" -eq 4
if grep -Fq 'must-not-render' "$workspace/digest.yaml"; then
  echo 'A reviewed image digest must take precedence over the mutable tag' >&2
  exit 1
fi
test "$(grep -c 'type: RollingUpdate' "$workspace/default.yaml")" -eq 3
"$helm_binary" template token-center-recreate "$chart" \
  --namespace token-center \
  --set deploymentStrategy=Recreate >"$workspace/recreate.yaml"
test "$(grep -c 'type: Recreate' "$workspace/recreate.yaml")" -eq 3
grep -q 'configMap:' "$workspace/configmap-plugin.yaml"
grep -q 'persistentVolumeClaim:' "$workspace/pvc-plugin.yaml"
test "$(grep -c 'name: MTC_RUN_MIGRATIONS_ON_START' "$workspace/default.yaml")" -eq 3
grep -Fq 'args: ["migrate"]' "$workspace/default.yaml"
test "$(grep -c 'name: MTC_ARCHIVE_BACKEND' "$workspace/default.yaml")" -eq 3
test "$(grep -A1 'name: MTC_ARCHIVE_BACKEND' "$workspace/default.yaml" | grep -c 'value: "s3"')" -eq 3
test "$(grep -c '^kind: Ingress$' "$workspace/default.yaml" || true)" -eq 0
test "$(grep -c '^kind: Ingress$' "$workspace/gateway-ingress.yaml")" -eq 1
test "$(grep -c '^kind: Ingress$' "$workspace/control-ingress.yaml")" -eq 1
test "$(grep -c '^kind: Ingress$' "$workspace/both-ingresses.yaml")" -eq 2
grep -Fq 'ingressClassName: public-gateway' "$workspace/gateway-ingress.yaml"
grep -Fq 'marker: gateway-only' "$workspace/gateway-ingress.yaml"
grep -Fq 'host: "gateway.example.test"' "$workspace/gateway-ingress.yaml"
grep -Fq 'secretName: gateway-tls' "$workspace/gateway-ingress.yaml"
test "$(grep -c -- '- path:' "$workspace/gateway-ingress.yaml")" -eq 5
grep -Fq -- '- path: /v1' "$workspace/gateway-ingress.yaml"
grep -Fq -- '- path: /self' "$workspace/gateway-ingress.yaml"
grep -Fq -- '- path: /portal' "$workspace/gateway-ingress.yaml"
grep -Fq -- '- path: /ui-assets' "$workspace/gateway-ingress.yaml"
if grep -Eq 'control\.internal\.example\.test|private-control|control-only|control-tls|component: control|path:[[:space:]]*/internal|path:[[:space:]]*/operator|path:[[:space:]]*/$' \
  "$workspace/gateway-ingress.yaml"; then
  echo 'Gateway-only ingress must not render any control-plane route or configuration' >&2
  exit 1
fi
grep -Fq 'ingressClassName: higress-private' "$workspace/control-ingress.yaml"
grep -Fq 'marker: control-only' "$workspace/control-ingress.yaml"
grep -Fq 'nginx.ingress.kubernetes.io/whitelist-source-range: 10.0.0.0/8' "$workspace/control-ingress.yaml"
grep -Fq 'nginx.ingress.kubernetes.io/ssl-redirect: "true"' "$workspace/control-ingress.yaml"
grep -Fq 'nginx.ingress.kubernetes.io/force-ssl-redirect: "true"' "$workspace/control-ingress.yaml"
grep -Fq 'host: "control.internal.example.test"' "$workspace/control-ingress.yaml"
grep -Fq 'secretName: control-tls' "$workspace/control-ingress.yaml"
test "$(grep -c -- '- path:' "$workspace/control-ingress.yaml")" -eq 3
grep -Fq -- '- path: /operator' "$workspace/control-ingress.yaml"
grep -Fq -- '- path: /ui-assets' "$workspace/control-ingress.yaml"
grep -Fq -- '- path: /internal/v1' "$workspace/control-ingress.yaml"
if grep -Eq 'gateway\.example\.test|public-gateway|gateway-only|gateway-tls|component: gateway|path:[[:space:]]*/v1|path:[[:space:]]*/self|path:[[:space:]]*/portal|path:[[:space:]]*/$' \
  "$workspace/control-ingress.yaml"; then
  echo 'Control-only ingress must not inherit public gateway configuration' >&2
  exit 1
fi
test "$(grep -c -- '- path:' "$workspace/both-ingresses.yaml")" -eq 8
test "$(grep -c 'marker: gateway-only' "$workspace/both-ingresses.yaml")" -eq 1
test "$(grep -c 'marker: control-only' "$workspace/both-ingresses.yaml")" -eq 1
grep -Fq 'type: LoadBalancer' "$workspace/gateway-load-balancer.yaml"
if grep -Eq 'type:[[:space:]]*(NodePort|LoadBalancer)' "$workspace/default.yaml"; then
  echo 'Control-bearing Services must remain ClusterIP by default' >&2
  exit 1
fi

if grep -A1 'name: MTC_RUN_MIGRATIONS_ON_START' "$workspace/default.yaml" | grep -q '"true"'; then
  echo 'Application Deployments must not run migrations on startup' >&2
  exit 1
fi
if grep -Eq '^kind:[[:space:]]*Secret[[:space:]]*$' \
  "$workspace/default.yaml" "$workspace/dev.yaml" "$workspace/observed.yaml"; then
  echo 'The chart must reference externally managed Secrets, never render them' >&2
  exit 1
fi
if grep -R -Fq 'kubectl.kubernetes.io/last-applied-configuration' "$chart"; then
  echo 'Chart sources must never add the last-applied Secret disclosure annotation' >&2
  exit 1
fi
if grep -Eq '^[[:space:]]*-[[:space:]]*\{\}[[:space:]]*$' \
  "$workspace/default.yaml" "$workspace/observed.yaml"; then
  echo 'Rendered policies must not contain an allow-all empty rule' >&2
  exit 1
fi
if grep -Eq 'port:[[:space:]]*(1080|5432|9000)([^0-9]|$)' "$workspace/default.yaml"; then
  echo 'Default policy must not grant private dependencies or a proxy by CIDR' >&2
  exit 1
fi
if grep -Eq 'MTC_ARCHIVE_PATH|mountPath:[[:space:]]*/.*archive' "$workspace/default.yaml"; then
  echo 'The S3-only chart must not expose a misleading local archive path' >&2
  exit 1
fi

assert_invalid() {
  case_name=$1
  shift
  if "$helm_binary" template "invalid-$case_name" "$chart" "$@" \
    >"$workspace/invalid-$case_name.yaml" 2>"$workspace/invalid-$case_name.err"; then
    echo "Values schema unexpectedly accepted invalid case: $case_name" >&2
    exit 1
  fi
}

assert_invalid private-egress-without-targets \
  --set networkPolicy.egress.clusterDependencies.enabled=true
assert_invalid filesystem-archive \
  --set config.archiveBackend=filesystem
assert_invalid memory-archive \
  --set config.archiveBackend=memory
assert_invalid malformed-image-digest \
  --set-string image.digest=sha256:abc123
assert_invalid uppercase-image-digest \
  --set-string image.digest=sha256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA
assert_invalid plugin-without-source \
  --set plugins.enabled=true
assert_invalid plugin-with-two-sources \
  --set plugins.enabled=true \
  --set plugins.existingConfigMap=token-center-plugins \
  --set plugins.existingClaim=token-center-plugins
assert_invalid misspelled-role-field \
  --set roles.gateway.replicaCounnt=2
assert_invalid misspelled-ingress-field \
  --set ingress.gateway.classname=nginx
assert_invalid legacy-shared-ingress-switch \
  --set ingress.enabled=true
assert_invalid gateway-ingress-without-host \
  --set ingress.gateway.enabled=true
assert_invalid control-ingress-without-host \
  --set ingress.control.enabled=true
assert_invalid control-ingress-without-explicit-class \
  --set ingress.control.enabled=true \
  --set ingress.control.host=control.internal.example.test
assert_invalid control-ingress-without-source-range \
  --set ingress.control.enabled=true \
  --set ingress.control.className=higress-private \
  --set ingress.control.host=control.internal.example.test \
  --set ingress.control.tlsSecretName=control-tls
assert_invalid control-ingress-without-tls \
  --set ingress.control.enabled=true \
  --set ingress.control.className=higress-private \
  --set ingress.control.sourceRanges[0]=10.0.0.0/8 \
  --set ingress.control.host=control.internal.example.test
assert_invalid control-ingress-with-ipv4-anywhere \
  --set ingress.control.enabled=true \
  --set ingress.control.className=higress-private \
  --set ingress.control.sourceRanges[0]=0.0.0.0/0 \
  --set ingress.control.host=control.internal.example.test \
  --set ingress.control.tlsSecretName=control-tls
assert_invalid control-ingress-with-ipv6-anywhere \
  --set ingress.control.enabled=true \
  --set ingress.control.className=higress-private \
  --set ingress.control.sourceRanges[0]=::/0 \
  --set ingress.control.host=control.internal.example.test \
  --set ingress.control.tlsSecretName=control-tls
assert_invalid control-ingress-with-noncanonical-anywhere \
  --set ingress.control.enabled=true \
  --set ingress.control.className=higress-private \
  --set ingress.control.sourceRanges[0]=1.2.3.4/0 \
  --set ingress.control.host=control.internal.example.test \
  --set ingress.control.tlsSecretName=control-tls
assert_invalid control-ingress-with-invalid-ipv4-octets \
  --set ingress.control.enabled=true \
  --set ingress.control.className=higress-private \
  --set ingress.control.sourceRanges[0]=999.999.999.999/24 \
  --set ingress.control.host=control.internal.example.test \
  --set ingress.control.tlsSecretName=control-tls
assert_invalid control-ingress-with-invalid-ipv4-prefix \
  --set ingress.control.enabled=true \
  --set ingress.control.className=higress-private \
  --set ingress.control.sourceRanges[0]=10.0.0.0/33 \
  --set ingress.control.host=control.internal.example.test \
  --set ingress.control.tlsSecretName=control-tls
assert_invalid control-ingress-with-unsupported-class \
  --set ingress.control.enabled=true \
  --set ingress.control.className=public-gateway \
  --set ingress.control.sourceRanges[0]=10.0.0.0/8 \
  --set ingress.control.host=control.internal.example.test \
  --set ingress.control.tlsSecretName=control-tls
assert_invalid gateway-ingress-with-disabled-role \
  --set ingress.gateway.enabled=true \
  --set ingress.gateway.host=gateway.example.test \
  --set roles.gateway.enabled=false
assert_invalid gateway-ingress-with-disabled-service \
  --set ingress.gateway.enabled=true \
  --set ingress.gateway.host=gateway.example.test \
  --set roles.gateway.service.enabled=false
assert_invalid control-nodeport-service \
  --set roles.control.service.type=NodePort
assert_invalid control-load-balancer-service \
  --set roles.control.service.type=LoadBalancer
assert_invalid all-nodeport-service \
  --set roles.all.service.type=NodePort
assert_invalid all-load-balancer-service \
  --set roles.all.service.type=LoadBalancer
assert_invalid misspelled-service-account-field \
  --set serviceAccount.automount=true
assert_invalid misspelled-plugin-field \
  --set plugins.mountpath=/plugins
assert_invalid misspelled-affinity-field \
  --set affinity.nodeAffinty.requiredDuringSchedulingIgnoredDuringExecution.nodeSelectorTerms[0].matchExpressions[0].key=kubernetes.io/arch
assert_invalid misspelled-topology-field \
  --set topologySpreadConstraints[0].maxSkew=1 \
  --set topologySpreadConstraints[0].topologyKey=kubernetes.io/hostname \
  --set topologySpreadConstraints[0].whenUnsatisfiable=DoNotSchedule \
  --set topologySpreadConstraints[0].minDomain=2
assert_invalid misspelled-config-field \
  --set config.databaseMaxConnection=8

if [ -n "$kubeconform_binary" ]; then
  cat \
    "$workspace/default.yaml" \
    "$workspace/dev.yaml" \
    "$workspace/observed.yaml" \
    "$workspace/digest.yaml" \
    "$workspace/configmap-plugin.yaml" \
    "$workspace/pvc-plugin.yaml" \
    "$workspace/gateway-ingress.yaml" \
    "$workspace/control-ingress.yaml" \
    "$workspace/both-ingresses.yaml" \
    "$workspace/gateway-load-balancer.yaml" \
    | "$kubeconform_binary" -strict -summary -ignore-missing-schemas
fi

echo 'Helm packaging contract OK'

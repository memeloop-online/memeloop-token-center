{{- define "memeloop-token-center.runtimeInventory.validate" -}}
{{- $runtime := .Values.plugins.runtimeInventory -}}
{{- if $runtime.enabled -}}
{{- if or .Values.plugins.enabled .Values.plugins.ociInstaller.enabled -}}
{{- fail "plugins.runtimeInventory cannot be combined with legacy plugins.enabled/ociInstaller" -}}
{{- end -}}
{{- if $runtime.persistence.create -}}
{{- if $runtime.existingClaim -}}{{- fail "runtimeInventory chooses existingClaim or persistence.create, not both" -}}{{- end -}}
{{- $_ := required "runtimeInventory persistence.create requires an explicitly RWX-capable storageClass" $runtime.persistence.storageClass -}}
{{- else -}}
{{- $_ := required "runtimeInventory requires existingClaim or persistence.create" $runtime.existingClaim -}}
{{- end -}}
{{- if not (or .Values.roles.control.enabled .Values.roles.all.enabled) -}}
{{- fail "runtimeInventory requires Control or all to initialize the shared inventory" -}}
{{- end -}}
{{- if $runtime.installationEnabled -}}
{{- $_ := required "runtimeInventory installation requires policyConfigMap" $runtime.policyConfigMap -}}
{{- if ne ($runtime.signaturePolicy | default "cosign-public-key") "cosign-keyless" -}}
{{- $_ := required "runtimeInventory installation requires cosignPublicKeysSecret.name" $runtime.cosignPublicKeysSecret.name -}}
{{- if not $runtime.cosignPublicKeysSecret.keys -}}{{- fail "runtimeInventory installation requires public-key items" -}}{{- end -}}
{{- end -}}
{{- end -}}

{{- else if $runtime.installationEnabled -}}
{{- fail "runtimeInventory installation requires runtimeInventory.enabled" -}}
{{- end -}}
{{- end -}}

{{- define "memeloop-token-center.runtimeInventory.claimName" -}}
{{- if .Values.plugins.runtimeInventory.persistence.create -}}
{{- printf "%s-plugin-runtime" (include "memeloop-token-center.fullname" .) | trunc 63 | trimSuffix "-" -}}
{{- else -}}{{- .Values.plugins.runtimeInventory.existingClaim -}}{{- end -}}
{{- end -}}

{{- define "memeloop-token-center.runtimeInventory.env" -}}
{{- if .root.Values.plugins.runtimeInventory.enabled }}
- name: MTC_PLUGIN_INVENTORY_FILE
  value: /var/lib/memeloop-token-center/plugin-runtime/inventory.json
{{- if and .writer .root.Values.plugins.runtimeInventory.installationEnabled }}
- name: MTC_PLUGIN_INSTALL_POLICY_FILE
  value: /var/run/mtc-plugin-policy/policy.json
{{- end }}
{{- end }}
{{- end -}}

{{- define "memeloop-token-center.runtimeInventory.mounts" -}}
{{- $runtime := .root.Values.plugins.runtimeInventory -}}
{{- if $runtime.enabled }}
- name: plugin-runtime-inventory
  mountPath: /var/lib/memeloop-token-center/plugin-runtime
  readOnly: {{ not .writer }}
{{- if and .writer $runtime.installationEnabled }}
- name: plugin-runtime-policy
  mountPath: /var/run/mtc-plugin-policy
  readOnly: true
{{- if ne ($runtime.signaturePolicy | default "cosign-public-key") "cosign-keyless" }}
- name: plugin-runtime-trust
  mountPath: /var/run/mtc-plugin-trust
  readOnly: true
{{- end }}
- name: plugin-runtime-tmp
  mountPath: /tmp
{{- range $index, $secret := $runtime.registrySecrets }}
- name: plugin-runtime-registry-{{ $index }}
  mountPath: /var/run/mtc-plugin-registry/{{ $index }}
  readOnly: true
{{- end }}
{{- end }}
{{- end }}
{{- end -}}

{{- define "memeloop-token-center.runtimeInventory.volumes" -}}
{{- $runtime := .root.Values.plugins.runtimeInventory -}}
{{- if $runtime.enabled }}
- name: plugin-runtime-inventory
  persistentVolumeClaim:
    claimName: {{ include "memeloop-token-center.runtimeInventory.claimName" .root | quote }}
    readOnly: {{ not .writer }}
{{- if and .writer $runtime.installationEnabled }}
- name: plugin-runtime-policy
  configMap:
    name: {{ $runtime.policyConfigMap | quote }}
    items: [{key: policy.json, path: policy.json}]
{{- if ne ($runtime.signaturePolicy | default "cosign-public-key") "cosign-keyless" }}
- name: plugin-runtime-trust
  secret:
    secretName: {{ $runtime.cosignPublicKeysSecret.name | quote }}
    defaultMode: 0440
    items:
      {{- range $runtime.cosignPublicKeysSecret.keys }}
      - key: {{ . | quote }}
        path: {{ . | quote }}
      {{- end }}
{{- end }}
- name: plugin-runtime-tmp
  emptyDir:
    sizeLimit: {{ $runtime.tmpSizeLimit | quote }}
{{- range $index, $secret := $runtime.registrySecrets }}
- name: plugin-runtime-registry-{{ $index }}
  secret:
    secretName: {{ $secret.name | quote }}
    defaultMode: 0440
    items:
      {{- range $secret.keys }}
      - key: {{ . | quote }}
        path: {{ . | quote }}
      {{- end }}
{{- end }}
{{- end }}
{{- end }}
{{- end -}}

{{- define "memeloop-token-center.rustfsName" -}}
{{- printf "%s-rustfs" (include "memeloop-token-center.fullname" . | trunc 55 | trimSuffix "-") -}}
{{- end -}}
{{- define "memeloop-token-center.rustfsSelector" -}}
app.kubernetes.io/name: mtc-rustfs
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/component: archive-storage
{{- end -}}

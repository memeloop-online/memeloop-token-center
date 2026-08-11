{{- define "memeloop-token-center.name" -}}
memeloop-token-center
{{- end -}}
{{- define "memeloop-token-center.fullname" -}}
{{- printf "%s-%s" .Release.Name (include "memeloop-token-center.name" .) | trunc 63 | trimSuffix "-" -}}
{{- end -}}
{{- define "memeloop-token-center.labels" -}}
app.kubernetes.io/name: {{ include "memeloop-token-center.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end -}}


{{- define "memeloop-token-center.name" -}}
memeloop-token-center
{{- end -}}
{{- define "memeloop-token-center.fullname" -}}
{{- if .Values.fullnameOverride -}}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- printf "%s-%s" .Release.Name (default (include "memeloop-token-center.name" .) .Values.nameOverride) | trunc 63 | trimSuffix "-" -}}
{{- end -}}
{{- end -}}
{{- define "memeloop-token-center.roleName" -}}
{{- printf "%s-%s" (include "memeloop-token-center.fullname" .root) .role | trunc 63 | trimSuffix "-" -}}
{{- end -}}
{{- define "memeloop-token-center.serviceAccountName" -}}
{{- if .Values.serviceAccount.create -}}
{{- default (include "memeloop-token-center.fullname" .) .Values.serviceAccount.name -}}
{{- else -}}
{{- default "default" .Values.serviceAccount.name -}}
{{- end -}}
{{- end -}}
{{- define "memeloop-token-center.labels" -}}
app.kubernetes.io/name: {{ include "memeloop-token-center.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end -}}

{{- define "memeloop-token-center.image" -}}
{{- if .Values.image.digest -}}
{{- printf "%s@%s" .Values.image.repository .Values.image.digest -}}
{{- else -}}
{{- printf "%s:%s" .Values.image.repository .Values.image.tag -}}
{{- end -}}
{{- end -}}

{{- define "memeloop-token-center.env" -}}
- name: MTC_DATABASE_URL
  valueFrom:
    secretKeyRef:
      name: {{ .Values.config.databaseUrlSecret.name }}
      key: {{ .Values.config.databaseUrlSecret.key }}
- name: MTC_DATABASE_MAX_CONNECTIONS
  value: {{ .Values.config.databaseMaxConnections | quote }}
- name: MTC_PROXY_LIFECYCLE_CONCURRENCY
  value: {{ .Values.config.proxyLifecycleConcurrency | quote }}
- name: MTC_GATEWAY_BODY_READ_CONCURRENCY
  value: {{ .Values.config.gatewayBodyReadConcurrency | quote }}
- name: MTC_KEY_PEPPER
  valueFrom:
    secretKeyRef:
      name: {{ .Values.config.keyPepperSecret.name }}
      key: {{ .Values.config.keyPepperSecret.key }}
- name: MTC_ARCHIVE_BACKEND
  value: {{ .Values.config.archiveBackend | quote }}
- name: MTC_S3_BUCKET
  value: {{ .Values.config.s3.bucket | quote }}
- name: MTC_S3_ENDPOINT
  value: {{ .Values.config.s3.endpoint | quote }}
- name: MTC_S3_REGION
  value: {{ .Values.config.s3.region | quote }}
- name: MTC_S3_ALLOW_HTTP
  value: {{ .Values.config.s3.allowHttp | quote }}
- name: MTC_S3_ACCESS_KEY
  valueFrom:
    secretKeyRef:
      name: {{ .Values.config.s3.credentialsSecret.name }}
      key: {{ .Values.config.s3.credentialsSecret.accessKey }}
- name: MTC_S3_SECRET_KEY
  valueFrom:
    secretKeyRef:
      name: {{ .Values.config.s3.credentialsSecret.name }}
      key: {{ .Values.config.s3.credentialsSecret.secretKey }}
- name: MTC_UPSTREAM_OPENAI_URL
  value: {{ .Values.config.upstream.openaiUrl | quote }}
- name: MTC_UPSTREAM_ANTHROPIC_URL
  value: {{ .Values.config.upstream.anthropicUrl | quote }}
{{- if .Values.plugins.enabled }}
- name: MTC_PLUGIN_DIR
  value: {{ .Values.plugins.mountPath | quote }}
{{- end }}
{{- end -}}

{{- define "memeloop-token-center.proxyEnv" -}}
{{- with .Values.config.outboundProxy }}
{{- if .url }}
- name: HTTP_PROXY
  value: {{ .url | quote }}
- name: HTTPS_PROXY
  value: {{ .url | quote }}
- name: NO_PROXY
  value: {{ .noProxy | quote }}
- name: http_proxy
  value: {{ .url | quote }}
- name: https_proxy
  value: {{ .url | quote }}
- name: no_proxy
  value: {{ .noProxy | quote }}
{{- end }}
{{- end }}
{{- end -}}

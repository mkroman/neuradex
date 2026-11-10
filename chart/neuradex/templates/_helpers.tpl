{{/* The expanded chart name + release name. */}}
{{- define "neuradex.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" }}
{{- end }}

{{/* The fully qualified app name, namespaced for multi-release installs. */}}
{{- define "neuradex.fullname" -}}
{{- if .Values.fullnameOverride }}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- $name := default .Chart.Name .Values.nameOverride }}
{{- if contains $name .Release.Name }}
{{- .Release.Name | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- printf "%s-%s" .Release.Name $name | trunc 63 | trimSuffix "-" }}
{{- end }}
{{- end }}
{{- end }}

{{/* Chart name and version, usable as a label value. */}}
{{- define "neuradex.chart" -}}
{{- printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
{{- end }}

{{/* Common labels. */}}
{{- define "neuradex.labels" -}}
helm.sh/chart: {{ include "neuradex.chart" . }}
{{ include "neuradex.selectorLabels" . }}
{{- if .Chart.AppVersion }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
{{- end }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
app.kubernetes.io/component: api
{{- end }}

{{/* Selector labels — must stay stable across upgrades. */}}
{{- define "neuradex.selectorLabels" -}}
app.kubernetes.io/name: {{ include "neuradex.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end }}

{{/* Service account name. */}}
{{- define "neuradex.serviceAccountName" -}}
{{- if .Values.serviceAccount.create }}
{{- default (include "neuradex.fullname" .) .Values.serviceAccount.name }}
{{- else }}
{{- default "default" .Values.serviceAccount.name }}
{{- end }}
{{- end }}

{{/* The container image reference, honoring optional digest pinning. */}}
{{- define "neuradex.image" -}}
{{- $tag := .Values.image.tag | default .Chart.AppVersion -}}
{{- if .Values.image.digest -}}
{{- printf "%s@%s" .Values.image.repository .Values.image.digest -}}
{{- else -}}
{{- printf "%s:%s" .Values.image.repository $tag -}}
{{- end -}}
{{- end }}

{{/* Name of the chart-managed audit secret. */}}
{{- define "neuradex.auditSecretName" -}}
{{- printf "%s-audit" (include "neuradex.fullname" .) }}
{{- end }}

{{/* Name of the chart-managed content S3 secret. */}}
{{- define "neuradex.contentSecretName" -}}
{{- printf "%s-content" (include "neuradex.fullname" .) }}
{{- end }}

{{/* Name of the content PersistentVolumeClaim (existing claim wins). */}}
{{- define "neuradex.contentClaimName" -}}
{{- .Values.content.local.persistence.existingClaim | default (printf "%s-content" (include "neuradex.fullname" .)) }}
{{- end }}

{{/*
Validate mutually exclusive or incomplete audit/content settings. Included
from deployment.yaml (always rendered), so a failed render points at the
invalid values rather than at a silently skipped template.
*/}}
{{- define "neuradex.validate" -}}
{{- if and .Values.audit.hmacKey .Values.audit.existingSecret -}}
{{- fail "audit.hmacKey and audit.existingSecret are mutually exclusive: use existingSecret to reference an existing Secret, or hmacKey to let the chart create one" -}}
{{- end -}}
{{- $store := .Values.content.store -}}
{{- if not (has $store (list "off" "local" "s3")) -}}
{{- fail (printf "content.store must be one of off, local, or s3 (got %q)" $store) -}}
{{- end -}}
{{- if eq $store "local" -}}
{{- if not (trim .Values.content.local.path) -}}
{{- fail "content.store=local requires content.local.path (the root filesystem is read-only, so the path must be a mounted volume)" -}}
{{- end -}}
{{- if and .Values.content.local.persistence.existingClaim (not .Values.content.local.persistence.enabled) -}}
{{- fail "content.local.persistence.existingClaim requires content.local.persistence.enabled=true, or remove existingClaim to fall back to an emptyDir" -}}
{{- end -}}
{{- end -}}
{{- if eq $store "s3" -}}
{{- if not (trim .Values.content.s3.endpoint) -}}
{{- fail "content.store=s3 requires content.s3.endpoint" -}}
{{- end -}}
{{- if not (trim .Values.content.s3.region) -}}
{{- fail "content.store=s3 requires content.s3.region" -}}
{{- end -}}
{{- if not (trim .Values.content.s3.bucket) -}}
{{- fail "content.store=s3 requires content.s3.bucket" -}}
{{- end -}}
{{- if .Values.content.s3.existingSecret -}}
{{- if or .Values.content.s3.accessKeyId .Values.content.s3.secretAccessKey -}}
{{- fail "content.s3.existingSecret cannot be combined with content.s3.accessKeyId or content.s3.secretAccessKey: choose one credential source" -}}
{{- end -}}
{{- else if not (and .Values.content.s3.accessKeyId .Values.content.s3.secretAccessKey) -}}
{{- fail "content.store=s3 requires either content.s3.existingSecret or both content.s3.accessKeyId and content.s3.secretAccessKey (the chart then creates a Secret)" -}}
{{- end -}}
{{- end -}}
{{- end }}

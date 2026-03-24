{{/*
Expand the name of the chart.
*/}}
{{- define "loadpilot.name" -}}
{{- .Chart.Name }}
{{- end }}

{{/*
Common labels.
*/}}
{{- define "loadpilot.labels" -}}
app.kubernetes.io/managed-by: {{ .Release.Service }}
app.kubernetes.io/instance: {{ .Release.Name }}
helm.sh/chart: {{ .Chart.Name }}-{{ .Chart.Version }}
{{- end }}

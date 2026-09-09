# Ollama Cloud (API key) — example plugin

Reads `https://ollama.com/api/usage` with a Bearer API key (keys are created at
`ollama.com/settings/keys`). Reports the monthly included-usage window, the trailing
four-week cost, and per-model usage.

The 5-hour/weekly session windows the settings page renders are not part of this
endpoint's vocabulary; the built-in `ollama` provider (browser session) tracks those.
This plugin is the keyed complement — no browser profile, no HTML.

## Install

```bash
tidemarkctl plugin install ollama-cloud.tidemark-provider
tidemarkctl provider add com.ollama.cloud
tidemarkctl plugin endpoint com.ollama.cloud https://ollama.com/api/usage
printf '%s' "$OLLAMA_API_KEY" | tidemarkctl auth set-key com.ollama.cloud
tidemarkctl refresh com.ollama.cloud
```

The response fixture (`ollama-cloud-response.json`) shows the paid-plan shape; the
parser also handles the free plan, whose `limits.monthly` carries no absolute cap —
in that shape the reading reports usage without a window, and nothing is invented.

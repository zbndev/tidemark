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
tidemarkctl provider add io.github.riccelso.ollama-cloud
tidemarkctl plugin endpoint io.github.riccelso.ollama-cloud https://ollama.com/api/usage
printf '%s' "$OLLAMA_API_KEY" | tidemarkctl auth set-key io.github.riccelso.ollama-cloud
tidemarkctl refresh io.github.riccelso.ollama-cloud
```

Only the free-plan shape has been observed live; the paid-plan branches
(`usage_limit`, per-model rows) are a hypothesis the parser spells out and the
fixture pins — nothing is invented on the free plan, and a wrong guess degrades to
the honest uncapped reading instead of failing.

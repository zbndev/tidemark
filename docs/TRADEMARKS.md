# Trademarks

Tidemark's own code is MIT-licensed. **The provider marks it ships are not.**

The files under `data/icons/hicolor/symbolic/apps/` are the trademarks of the companies
whose services the cards are about:

| File | Mark of |
| --- | --- |
| `tidemark-abacus-symbolic.svg` | Abacus AI, Inc. |
| `tidemark-aiand-symbolic.svg` | ai& (console.aiand.com) |
| `tidemark-amp-symbolic.svg` | Amp (ampcode.com) |
| `tidemark-antigravity-symbolic.svg` | Google LLC (Antigravity) |
| `tidemark-augment-symbolic.svg` | Augment Code, Inc. |
| `tidemark-chutes-symbolic.svg` | Chutes AI (Chutes) |
| `tidemark-claude-symbolic.svg` | Anthropic PBC (Claude) |
| `tidemark-clawrouter-symbolic.svg` | The OpenClaw project (ClawRouter) |
| `tidemark-clinepass-symbolic.svg` | Cline (ClinePass) |
| `tidemark-codex-symbolic.svg` | OpenAI (Codex) |
| `tidemark-codebuff-symbolic.svg` | Codebuff, Inc. |
| `tidemark-commandcode-symbolic.svg` | CommandCode (commandcode.ai) |
| `tidemark-crof-symbolic.svg` | Crof |
| `tidemark-cursor-symbolic.svg` | Anysphere, Inc. (Cursor) |
| `tidemark-deepinfra-symbolic.svg` | DeepInfra |
| `tidemark-deepgram-symbolic.svg` | Deepgram, Inc. |
| `tidemark-deepseek-symbolic.svg` | Hangzhou DeepSeek Artificial Intelligence Co., Ltd. |
| `tidemark-elevenlabs-symbolic.svg` | ElevenLabs |
| `tidemark-factory-symbolic.svg` | Factory AI, Inc. (Factory) |
| `tidemark-fireworks-symbolic.svg` | Fireworks AI |
| `tidemark-groq-symbolic.svg` | Groq, Inc. |
| `tidemark-ibmbob-symbolic.svg` | IBM (Bob) |
| `tidemark-kilo-symbolic.svg` | Kilo Code, Inc. |
| `tidemark-kimi-symbolic.svg` | Moonshot AI (Kimi) |
| `tidemark-litellm-symbolic.svg` | The LiteLLM project |
| `tidemark-llmproxy-symbolic.svg` | The LLM Proxy project |
| `tidemark-manus-symbolic.svg` | Manus AI |
| `tidemark-minimax-symbolic.svg` | MiniMax |
| `tidemark-moonshot-symbolic.svg` | Moonshot AI |
| `tidemark-mimo-symbolic.svg` | Xiaomi Corporation |
| `tidemark-notion-symbolic.svg` | Notion Labs, Inc. |
| `tidemark-nanogpt-symbolic.svg` | NanoGPT (nano-gpt.com) |
| `tidemark-neuralwatt-symbolic.svg` | Neuralwatt |
| `tidemark-openai-api-symbolic.svg` | OpenAI |
| `tidemark-opencodego-symbolic.svg` | The OpenCode project (OpenCode Go) |
| `tidemark-openrouter-symbolic.svg` | OpenRouter |
| `tidemark-perplexity-symbolic.svg` | Perplexity AI, Inc. |
| `tidemark-poe-symbolic.svg` | Quora, Inc. (Poe) |
| `tidemark-qoder-symbolic.svg` | Qoder (Alibaba Group) |
| `tidemark-sub2api-symbolic.svg` | The sub2api project |
| `tidemark-synthetic-symbolic.svg` | Synthetic |
| `tidemark-venice-symbolic.svg` | Venice AI |
| `tidemark-warp-symbolic.svg` | Warp (warp.dev) |
| `tidemark-wayfinder-symbolic.svg` | The Wayfinder project (wayfinder-router) |
| `tidemark-xai-symbolic.svg` | xAI Corp. |
| `tidemark-zai-symbolic.svg` | Z.ai (Zhipu AI) |
| `tidemark-zenmux-symbolic.svg` | ZenMux |

They are used nominatively — to identify which service a card is reporting on — and for no
other purpose. Tidemark is not affiliated with, endorsed by, or sponsored by any of them.
No licence to use these marks is granted by this project's `LICENSE`, and nothing in this
repository should be read as granting one.

## Where they came from and what was done to them

Traced from the SVGs in [lobe-icons](https://github.com/lobehub/lobe-icons) (MIT code,
marks not the project's to license), except the Z.ai Z, which comes from Z.ai's own brand
asset, and except the thirty marks that CodexBar records as the provider icons of
ai&, Amp, Chutes, ClawRouter, ClinePass, Codebuff, Crof, Deepgram, DeepInfra, DeepSeek,
ElevenLabs, Factory,
Fireworks, Groq, IBM Bob, Kilo, LiteLLM, LLM Proxy, MiniMax, Neuralwatt, OpenAI, OpenCode Go,
OpenRouter, Poe, Qoder, sub2api, Synthetic, Venice, Warp, Wayfinder and ZenMux — and except the xAI mark, which is
xAI's own, taken from
[File:XAI Logo.svg](https://commons.wikimedia.org/wiki/File:XAI_Logo.svg) on Wikimedia
Commons: the icon CodexBar files under `xai` is three strokes in the shape of an X and a
bar, an approximation of the mark rather than the mark, and this shipped as that
approximation until 2026-08-22. The Moonshot mark is the Kimi one: they are the same company's, and CodexBar draws
Moonshot with its Kimi icon for that reason. The
ClinePass one, its file notes, is
Cline's own bot icon from [cline.bot/brand](https://cline.bot/brand), and the OpenAI one
is the blossom CodexBar files under its codex name. The NanoGPT mark is a single-colour
trace of NanoGPT's official
[`Nano-gptFilled.png`](https://nano-gpt.com/logo/Nano-gptFilled.png) asset. Each was reduced to a
single-colour path set and stripped of the fills, gradients, masks and filters the
originals carry — a symbolic icon is recoloured by the theme and cannot keep
them; the ai&, ClawRouter, Fireworks, LiteLLM, OpenRouter, Synthetic and Wayfinder marks were first
drawn as strokes, a stroke being as single-colour as a fill, and their strokes have since
been outlined into the filled paths that ship — GTK's symbolic renderer paints `fill` and
does not draw a `stroke` at all, so the stroked files rendered as blank or half-blank
cards. The outlining is a change of representation, not of shape. Each was then
re-framed: its own bounding box measured, its longest side scaled to the
same fraction of a square grid so that the set reads at one optical size, and the box shifted
so the mark stands on the grid's floor rather than floating in its middle — which is
what puts every mark's foot on the baseline of the name beside it. The shapes themselves are
unchanged; the outline of each mark is the owner's, path for path.

## Redistributing

A distribution whose artwork policy will not accept third-party trademarks — Debian's DFSG
and Fedora's trademark rules both can refuse them — should **drop `data/icons` from the
package**. Nothing else has to change: a card with no mark is a state the interface already
has, and the build stays supported without them.

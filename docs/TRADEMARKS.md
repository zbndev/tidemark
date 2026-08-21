# Trademarks

Tidemark's own code is MIT-licensed. **The provider marks it ships are not.**

The files under `data/icons/hicolor/symbolic/apps/` are the trademarks of the companies
whose services the cards are about:

| File | Mark of |
| --- | --- |
| `tidemark-antigravity-symbolic.svg` | Google LLC (Antigravity) |
| `tidemark-claude-symbolic.svg` | Anthropic PBC (Claude) |
| `tidemark-clawrouter-symbolic.svg` | The OpenClaw project (ClawRouter) |
| `tidemark-clinepass-symbolic.svg` | Cline (ClinePass) |
| `tidemark-codex-symbolic.svg` | OpenAI (Codex) |
| `tidemark-crof-symbolic.svg` | Crof |
| `tidemark-kimi-symbolic.svg` | Moonshot AI (Kimi) |
| `tidemark-sub2api-symbolic.svg` | The sub2api project |
| `tidemark-synthetic-symbolic.svg` | Synthetic |
| `tidemark-venice-symbolic.svg` | Venice AI |
| `tidemark-zai-symbolic.svg` | Z.ai (Zhipu AI) |

They are used nominatively — to identify which service a card is reporting on — and for no
other purpose. Tidemark is not affiliated with, endorsed by, or sponsored by any of them.
No licence to use these marks is granted by this project's `LICENSE`, and nothing in this
repository should be read as granting one.

## Where they came from and what was done to them

Traced from the SVGs in [lobe-icons](https://github.com/lobehub/lobe-icons) (MIT code,
marks not the project's to license), except the Z.ai Z, which comes from Z.ai's own brand
asset, and except the six marks that CodexBar records as the provider icons of ClawRouter,
ClinePass, Crof, sub2api, Synthetic and Venice — the ClinePass one, its file notes, is
Cline's own bot icon from [cline.bot/brand](https://cline.bot/brand). Each was reduced to a
single-colour path set and stripped of the fills, gradients, masks
and filters the originals carry — a symbolic icon is recoloured by the theme and cannot keep
them. Each was then re-framed: its own bounding box measured, its longest side scaled to the
same fraction of a square grid so that the set reads at one optical size, and the box shifted
so the mark stands on the grid's floor rather than floating in its middle — which is
what puts every mark's foot on the baseline of the name beside it. The shapes themselves are
unchanged; the outline of each mark is the owner's, path for path.

## Redistributing

A distribution whose artwork policy will not accept third-party trademarks — Debian's DFSG
and Fedora's trademark rules both can refuse them — should **drop `data/icons` from the
package**. Nothing else has to change: a card with no mark is a state the interface already
has, and the build stays supported without them.

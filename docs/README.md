# Documentation

| Document | What it is |
|---|---|
| [overview.md](overview.md) | What PassportSim emulates, how, how fidelity is checked, and its known limitations. |
| [quickstart.md](quickstart.md) | Building a package and running it: first launch per system, the daemon, the web page, Windows notes. Shipped in every package. |
| [deploy-cloudflare.md](deploy-cloudflare.md) | Deploying the web bundle as a Cloudflare Workers static site. Shipped in every package. |
| [commands/](commands/) and `errors.md` | The command reference and error codes, generated from the command registry by `cargo xtask docs`. Never edited by hand. |
| [ARCHITECTURE.md](ARCHITECTURE.md) | The design: crates, data flow, time and determinism, host surfaces, the web page, packaging and verification. |
| [secrets.md](secrets.md) | The secrets policy: what counts as device data, the secret set, repository hygiene and the `secrets-check` rules. |

Contributor guidance, including the clean-room rules, is in [CONTRIBUTING.md](../CONTRIBUTING.md);
the notes of each release are on its GitHub Release.

Translations of the README, the contributor documents and the guides above are in `i18n/`
([简体中文](i18n/zh-CN/README.md), [日本語](i18n/ja/README.md), [Français](i18n/fr/README.md)). The
generated references, `THIRD_PARTY.md` and `LICENSE` are English only.

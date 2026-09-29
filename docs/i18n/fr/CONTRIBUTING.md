# Contribuer à PassportSim

[English](../../../CONTRIBUTING.md) | [简体中文](../zh-CN/CONTRIBUTING.md) | [日本語](../ja/CONTRIBUTING.md) | **Français**

Merci de votre aide. PassportSim vise à se comporter exactement comme la vraie carte : une
modification est donc jugée sur ses preuves. Chaque comportement doit remonter à une documentation,
à un code source sous licence compatible ou à une mesure sur le silicium.

Merci de respecter le [code de conduite](CODE_OF_CONDUCT.md). Signalez les problèmes de sécurité
comme l'indique [SECURITY.md](SECURITY.md), jamais dans une issue publique.

Le rapport le plus utile est un **écart de fidélité** : un firmware qui ne se comporte pas de la
même façon dans l'émulateur et sur la carte. Joignez l'image (ou la façon de la compiler), les
commandes lancées et ce qu'ont fait l'émulateur et la carte. Ne joignez jamais de données d'un
appareil réel (voir [Données secrètes](#données-secrètes)).

## Mise en place

Il faut [Rust](https://rustup.rs) (la chaîne d'outils de `rust-toolchain.toml` s'installe
d'elle-même), [Bun](https://bun.sh) 1.4.1 ou plus récent (la CI utilise la version de
`.bun-version`) et [just](https://github.com/casey/just). Les hôtes de développement sont macOS sur
puce Apple et Windows 10 ou plus récent sur x64.

```sh
just setup                        # cible wasm et paquets de la page web
cargo xtask secrets-check --init  # une fois par clone
cargo xtask hooks install         # contrôles des secrets en pre-commit et pre-push
```

Relancez `just setup` dans chaque nouveau worktree : `web/node_modules/` n'est pas partagé, et
sans lui les tests web échouent sur un paquet manquant.

`just` liste toutes les commandes. Celles du quotidien sont `just run` (compiler et servir la page
web), `just test`, `just check` et `just ci`.

## Avant d'ouvrir une pull request

```sh
just check              # cargo fmt, clippy, vérification des types TypeScript
just test               # tests unitaires Rust et web
cargo xtask codegen     # régénère tables et docs ; l'arbre doit rester propre
just ci                 # le niveau T0 : lints, tous les tests et contrôles du dépôt
```

`just ci` lance `cargo xtask ci t0`, qui ne demande ni corpus de firmwares ni données d'appareil, se
comporte de la même façon sous macOS et Windows et teste la machine sur laquelle il tourne.
`cargo xtask ci t1` et `t2` ajoutent le corpus, les goldens, les exécutions dans le navigateur, les
comparaisons avec l'oracle et les benchmarks ; ils tournent sous macOS. GitHub Actions couvre les
deux hôtes : `pr-check.yml` vérifie chaque pull request sous macOS et Windows, `release.yml`
publie une version, et `deploy-web.yml` déploie la page web. Pour publier une version, lancez le
workflow Release depuis l'onglet Actions, éventuellement avec une version ou un incrément (patch par
défaut) : il crée le tag, construit les deux paquets, publie la GitHub Release avec des notes
générées et déploie la page web.

Liste de contrôle :

- [ ] `just ci` passe, ainsi que `t1` si vous avez modifié l'émulation ou la page web et disposez
  du corpus ;
- [ ] un changement de comportement s'accompagne d'un test qui échouait avant lui ;
- [ ] aucun fichier généré n'a été modifié à la main ;
- [ ] aucune donnée d'appareil réel ne figure dans le diff.

## Commits

- Les titres sont courts, en anglais, avec un préfixe conventionnel : `feat(scope):`,
  `fix(scope):`, `perf(scope):`, `test(scope):`, `docs(scope):`, `ci:`, `build:`.
- Le corps explique pourquoi et cite la preuve d'un changement de comportement : une ligne de spec,
  une capture de sonde, une section de document.
- Committez avec un simple `git commit`. Ne contournez ni ne redirigez jamais les hooks
  (`--no-verify`, `core.hooksPath`).

## Salle blanche

PassportSim est sous licence MIT et écrit à partir d'informations publiques. Pour qu'il le reste :

- **Ne lisez ni ne copiez jamais le code source de** QEMU (y compris le fork d'Espressif), esp32sim,
  ESP-EMU, NimBLE, Bumble, Zephyr, BlueZ, esptool ou serialport-rs, ni d'aucun autre émulateur ou
  pile sous GPL, LGPL, AGPL ou sans licence. N'en paraphrasez pas non plus le code, la structure ou
  les commentaires.
- **Les autres émulateurs ne s'utilisent que comme oracles en boîte noire :** vous pouvez comparer
  leurs sorties (texte de console, flux d'écritures de registres, traces) aux nôtres, comme le font
  `tools/oracle/` et `xtask oracle`, mais jamais regarder à l'intérieur.
- **Les informations sur les registres et le timing peuvent venir de** la documentation publique
  d'Espressif (le manuel de référence technique et les fiches techniques de l'ESP32-C3), des sources
  d'ESP-IDF (Apache-2.0), des ELF de ROM fournis (Apache-2.0) et de nos propres firmwares de sonde
  exécutés sur du vrai silicium (`probes/`).
- **Citez la source.** Chaque ligne de `specs/` a un champ `provenance`, et l'en-tête de chaque
  fichier de modèle nomme les lignes de spec ou les documents qu'il implémente. Marquez une
  hypothèse non encore vérifiée comme `UNVERIFIED`. `cargo xtask provenance` le vérifie en T0.

## Où trouver quoi

| Quoi | Où |
|---|---|
| Conception | [ARCHITECTURE.md](ARCHITECTURE.md) |
| Tests unitaires | À côté du code (`#[cfg(test)]`) et dans le `tests/` de chaque crate |
| Tests d'intégration qui démarrent de vraies images | `tests/milestones/` (les tests nommés `t1_*` ou `t2_*` tournent dans ce niveau de CI) |
| Goldens, scénarios, transcriptions | `tests/golden/`, `tests/scenarios/`, `tests/transcripts/` |
| Tests unitaires web | `web/src/**/*.test.ts` (`bun test`) |
| Tests dans le navigateur | `web/tests/*.spec.ts` (`just e2e`) |
| Données de comportement | `specs/` (voir [specs/README.md](../../../specs/README.md)) |

## Conventions

- **Les fichiers générés ne se modifient jamais à la main.** Modifiez la source et lancez
  `cargo xtask codegen` (tables de registres, docs de fidélité) ou `cargo xtask docs` (référence des
  commandes).
- **Les registres sont locaux.** Un périphérique modifie son propre fichier, pas `periph/mod.rs` ;
  une commande s'enregistre elle-même avec `#[command]`.
- **Les interfaces figées changent à part.** Les traits du cœur et l'ABI wasm listés dans
  [ARCHITECTURE.md](ARCHITECTURE.md#interfaces-figées) changent dans une pull request distincte,
  avant le code qui s'en sert, avec la raison dans sa description.
- **Les dépendances sont relues.** Une nouvelle crate arrive dans une petite modification à elle
  seule, avec `cargo deny check` au vert ; seules les licences de `deny.toml` sont admises.
- **Les crates du cœur restent indépendantes de l'hôte.** Elles compilent pour
  `wasm32-unknown-unknown` et n'utilisent aucune API d'horloge, de thread, de fichier,
  d'environnement, de réseau ou de processus. Le code propre à l'hôte vit dans
  `pemu_host::platform`, les rôles de répertoires dans `pemu_host::paths`.
- **Rust édition 2024 et `cargo fmt`.** Le code, les commentaires et les messages de commit sont en
  anglais. Les commentaires sont courts et disent pourquoi ; le code dit quoi.
- **L'arbre est uniquement en LF** et chaque chemin doit être valide sous Windows ;
  `cargo xtask portable` vérifie les deux.

## Données secrètes

Les données d'un appareil réel n'entrent jamais dans le dépôt : ni adresses MAC, ni identifiants
uniques, ni valeurs de calibration, ni dumps de flash ou d'eFuse, ni noms de fichiers de sauvegarde,
ni contenu de carte. Utilisez des MAC fictives avec le préfixe `02:00:00`. Les hooks installés plus
haut refusent les commits qui contiennent de telles données. La politique complète est dans
[secrets.md](secrets.md).

## Licence

En contribuant, vous acceptez que votre contribution soit placée sous la licence MIT de ce dépôt
([LICENSE](../../../LICENSE)).

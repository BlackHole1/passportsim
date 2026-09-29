# Démarrage rapide

[English](../../quickstart.md) | [简体中文](../zh-CN/quickstart.md) | [日本語](../ja/quickstart.md) | **Français**

Un paquet, c'est une archive contenant un seul exécutable. Il ne demande ni ESP-IDF, ni outils
`~/.espressif`, ni corpus de firmwares, ni appareil, ni Bun, Node, Python ou QEMU, ni
téléchargement de ROM.

## Hôtes pris en charge

macOS 27 ou plus récent sur puce Apple (`aarch64-apple-darwin`), et Windows 10 1903 ou plus récent
sur x64 (`x86_64-pc-windows-msvc`, section 8). Linux n'est pas pris en charge.

## 1. Obtenir le paquet

Pour installer une version publiée, lancez `curl -fsSL https://passportsim.bugs.cc/install.sh | sh` sous macOS ou
`irm https://passportsim.bugs.cc/install.ps1 | iex` dans PowerShell sous Windows ; l'installation se
fait dans votre dossier utilisateur et indique comment lancer `passportsim`. Pour construire un
paquet depuis une copie du dépôt, sans installeur ni `PATH` à modifier :

```sh
cargo run -q -p xtask -- package --target aarch64-apple-darwin
```

La commande écrit, dans `target/package/` :

| Chemin | Contenu |
|---|---|
| `passportsim-0.1.0-macos-arm64/` | le paquet décompressé |
| `passportsim-0.1.0-macos-arm64.tar.gz` | la même arborescence en une archive |
| `passportsim-0.1.0-web/` | le bundle web statique (section 5) |
| `passportsim-0.1.0-web.tar.gz` | le même bundle en une archive |

Aucun fichier du paquet ne nomme le compte de build : les chemins des sources sont remplacés par
des jetons fixes, et la construction du paquet échoue s'il reste un nom d'utilisateur ou un
répertoire personnel.

Décompressez l'archive où vous voulez et placez-vous dans le dossier. Toutes les commandes
suivantes s'exécutent dans ce dossier :

```sh
cd passportsim-0.1.0-macos-arm64
```

Si la construction du paquet échoue en cours de route, vérifiez d'abord l'espace disque : elle
écrit deux arborescences et deux archives. Supprimez `target/package/` et relancez ; tout est
reconstruit à partir de zéro.

### L'exécutable seul

Pour lancer l'émulateur depuis une copie du dépôt sans construire de paquet :

```sh
cargo build -p pemu-cli
./target/debug/passportsim status
```

Lisez alors chaque `./passportsim` ci-dessous comme `./target/debug/passportsim`. La section 2 ne
s'applique pas, et un tel build n'a pas de payload (pas de démo intégrée), ce qu'indique
`--version` (section 6).

## 2. Premier lancement sous macOS

L'exécutable n'est **ni signé ni notarié**. Si l'archive est arrivée par un navigateur ou depuis
une autre machine, retirez une fois l'attribut de quarantaine :

```sh
xattr -dr com.apple.quarantine .
```

Cette commande n'installe rien ; sur un paquet qui n'a jamais quitté cette machine, elle ne fait
rien.

## 3. Vérifier que tout fonctionne

```sh
./passportsim status
```

La commande affiche les instances (aucune au premier lancement), le dossier des artefacts et le
reçu d'exécution :

```text
no instance is running
artifacts: ~/Library/Application Support/passportsim/artifacts
profile fast | deterministic
```

Les dossiers ne sont créés qu'au moment où quelque chose y est écrit.

Chaque commande répond à `--help` ; le même texte se trouve dans
[commands/](../../commands/index.md) :

```sh
./passportsim --help
```

Toute commande produit du JSON avec `--output json`. (`--json` sert à fournir le document
d'*entrée* d'une commande.)

La sortie texte est bornée : les lignes identiques sont regroupées, les lignes longues se terminent
par `...(+N chars)`, et seules les 10 premières et les 30 dernières entrées sont gardées, séparées
par un marqueur `... N lines elided ...`. Pour lire une longue sortie console, parcourez-la avec
`serial read --max-bytes` et le `next_cursor` renvoyé à chaque lecture.

```sh
./passportsim status --output json
```

### Le démon, et MCP

Les instances vivent dans un démon. `start` en lance un en arrière-plan
(`passportsim serve --headless`) s'il n'y en a pas, et les commandes suivantes (`run`, `serial`,
`status`, `stop`, ...) lui sont envoyées : une instance survit donc à la commande qui l'a
démarrée. Sans démon, une commande tourne dans son propre processus ; `--ephemeral` force ce mode.
Un chemin de firmware relatif est rendu absolu avant l'envoi (via MCP et HTTP, les chemins doivent
être absolus). `pk` ci-dessous est un identifiant de corpus (section 4).

```sh
./passportsim start pk --boot none
./passportsim run 'serial:/bsp_i2c/'
./passportsim serial read --cursor 0
./passportsim stop
./passportsim serve --stop
```

Le démon n'écoute que sur `127.0.0.1:8765` (ou sur un port libre si celui-ci est pris). Chaque
requête exige le jeton qu'il écrit, réservé au propriétaire, à côté de `serve.json` dans
`~/.passportsim/`. Un démon sans interface journalise dans `~/.passportsim/logs/serve.log` et
s'arrête seul après dix minutes sans instance.

`passportsim serve` sans `--headless` tourne au premier plan et affiche son URL, le fichier du
jeton et un lien `ui:` vers la page web (section 5). Le lien porte un code de lancement à usage
unique après `#lc=`, valable 60 secondes. `passportsim serve --stop` arrête le démon après avoir
arrêté ses instances et écrit leurs artefacts.

`passportsim mcp` est le serveur MCP pour les agents : MCP sur l'entrée et la sortie standard,
relayé vers le démon (démarré si besoin). `--caps audio,nfc` ajoute des groupes d'outils à
l'ensemble de base. Dans la configuration d'un client, indiquez l'exécutable et cet unique
argument :

```sh
./passportsim mcp
```

## 4. Ce qui est intégré

| Besoin | Intégré | Remplacement optionnel |
|---|---|---|
| ROM | les deux ELF de ROM masquée ESP32-C3 d'Espressif, choisis selon la révision de puce de l'eFuse | aucun que `start` utilise (section 6) |
| eFuse | une image synthétisée : révision de puce v1.1, MAC fictive `02:00:00:xx:xx:xx`, mots de calibration à zéro | `--efuse-dump <dir>`, qui marque la machine comme contaminée ([secrets.md](secrets.md)) |
| Firmware | la démo BSP officielle, si l'hôte de build l'avait (section 6) | un identifiant de corpus, ou le chemin d'un dossier de build `idf.py`, d'un bin fusionné ou d'un `.pebundle` |
| Outils | cet exécutable, ou le bundle web | ESP-IDF seulement pour *compiler* un firmware ; esptool seulement pour les points d'accès USB Serial/JTAG |

`passportsim doctor` indique ce que cette machine a résolu : les empreintes des ROM intégrées, un
éventuel remplacement de ROM, la démo intégrée, et chaque entrée de `corpus.toml`, trouvée,
manquante ou non conforme. Il n'affiche jamais le contenu des fichiers.

```sh
./passportsim doctor
```

On peut aussi lui fournir un rapport, comme le fait un agent via MCP
([commands/doctor.md](../../commands/doctor.md)) :

```sh
printf '%s' '{"report":{"bundled_roms":[],"corpus":[]}}' | ./passportsim doctor --json -
```

Aucun fichier de configuration ni corpus n'est nécessaire.

### Le corpus de firmwares

Un **identifiant de corpus** est un nom court, propre à la machine, qui désigne une image de
firmware : `start official` remplace ainsi un chemin. Les identifiants ne sont pas intégrés ;
chaque machine définit les siens dans `corpus.toml`, dans le dossier de configuration :

| Rôle | macOS | Windows |
|---|---|---|
| dossier de configuration (`corpus.toml`, `config.toml`) | `~/.config/passportsim/` | `%APPDATA%\passportsim\` |
| racine des données (`corpus/`, `artifacts/`, `audio/`) | `~/Library/Application Support/passportsim/` | `%LOCALAPPDATA%\passportsim\data\` |

Une table par identifiant. Les clés de fichiers sont `bin` (image flash fusionnée), `elf` (ELF de
l'application), `boot_elf` (ELF du bootloader) et `pt` (table des partitions) ; seule `bin` est
obligatoire. `sha256` fige chaque fichier par son empreinte complète de 64 caractères (abrégée
ici) :

```text
[official]
bin = "corpus/official/FoloToy-AI-Passport-8MB.bin"
elf = "corpus/official/FoloToy-AI-Passport.elf"
boot_elf = "corpus/official/bootloader.elf"
sha256 = { bin = "5802...e163", elf = "dd63...a2de", boot_elf = "5fcf...17a8" }
```

Les chemins, ici comme dans `config.toml` :

- un chemin **absolu** est utilisé tel quel (un `/` initial compte aussi comme absolu sous
  Windows) ;
- **`~/...` ou `~\...`** désigne le répertoire personnel ;
- **tout autre chemin est relatif à la racine des données**, jamais au dossier courant, ce qui
  rend un même `corpus.toml` portable. Un chemin qui en sort avec `..` est refusé.

Un fichier absent donne `E_ASSET_MISSING`, une empreinte différente `E_ASSET_HASH` ; `doctor`
nomme les chemins refusés.

Variables d'environnement :

- `PASSPORTSIM_CORPUS_<ID>` remplace le chemin d'une entrée (`<ID>` en majuscules, `-` devenant
  `_` : `PASSPORTSIM_CORPUS_PROBE_LONG` pour `probe-long`). Deux identifiants qui donnent le même
  nom sont refusés.
- `PASSPORTSIM_DATA_ROOT` déplace la racine des données.
- `PASSPORTSIM_HOME` déplace tous les rôles de dossiers vers `<dir>/<role>/`.

Les tests du projet utilisent les identifiants `official` (démo BSP officielle), `pk` (Passport
Keys), `goldminer`, `demo`, ainsi que les images de ROM et de sonde `rom0`, `probe-long`,
`qemu-oracle`, `probe2`, `scan3` et `pkgatt`. **Une installation neuve n'a aucun identifiant de
corpus**, et ce n'est pas un problème : `start` sans argument démarre la démo intégrée, et un
chemin démarre votre propre image.

```sh
./passportsim start
./passportsim start ~/esp/my-project/build
./passportsim stop
./passportsim serve --stop
```

Un dossier de build est lu via le `flasher_args.json` qu'écrit `idf.py build` : chaque partie est
donc placée à l'offset enregistré. Un exécutable issu de `cargo build` n'a pas de démo : `start`
sans argument échoue alors avec `E_ASSET_MISSING`, en indiquant `cargo xtask package`.

## 5. Le bundle web

`passportsim-0.1.0-web/` est un site statique : `index.html`, la feuille de style, les scripts de
la page, du worker et de l'audio worklet, le cœur wasm avec les deux ROM, et le `.pebundle` de la
démo si l'hôte de build l'avait. Servez-le avec n'importe quel serveur statique, à la racine du
site ou sous un sous-chemin (par exemple `/emu/`). Ouvrir la page depuis le disque (`file://`) ne
fonctionne pas.

La page démarre la démo. Déposez un dossier de build `idf.py`, un bin fusionné ou un `.pebundle`
pour l'exécuter à la place ; un ELF déposé seul ne fait que fournir ses symboles à `inspect`.

Le mode **simple** (par défaut) affiche l'appareil, une carte firmware et un journal en direct qui
inclut les étapes de chargement de la page. Le mode **avancé** ajoute le contrôle de l'exécution,
les onglets Console, UI tree, Events, Inspect, Fidelity et Perf, et les cartes Battery, USB, Audio,
NFC, Wi-Fi, BLE et Snapshots. La page existe en anglais, chinois simplifié, japonais et français,
suit la langue du navigateur et le thème du système, et retient les choix faits dans son en-tête.
`?mode=advanced` (ou `simple`) et `?lang=ja` (ou `en`, `zh-CN`, `fr`) s'appliquent à un seul
chargement ; avec le lien de `serve`, placez-les avant le `#` :
`http://127.0.0.1:8765/?mode=advanced&lang=ja#lc=<code>`.

Le même bundle se trouve dans le paquet sous `payload/web/` et dans l'exécutable lui-même : c'est
lui que sert `passportsim serve`.

Un autre serveur doit envoyer `Cross-Origin-Opener-Policy: same-origin` et
`Cross-Origin-Embedder-Policy: require-corp` (sans eux, pas de `SharedArrayBuffer`), et servir
`.wasm` en `application/wasm`. Le bundle est aussi un projet Cloudflare Workers prêt à l'emploi ;
voir [deploy-cloudflare.md](deploy-cloudflare.md) pour les commandes et la limite de 25 MiB par
fichier.

## 6. Limites connues

| Quoi | État |
|---|---|
| ELF d'application d'un firmware donné par chemin | `until_ui_settled`, `inspect`, `ui` et `--boot-cache` ont besoin du DWARF de l'ELF de l'application. La ligne de commande n'en trouve un que pour la démo et pour un identifiant de corpus avec une entrée `elf`. Pour une image fusionnée, un dossier de build ou un `.pebundle` donné par chemin, `boot: until_ui_settled` épuise tout son budget et renvoie `unobservable`, les parcours signalent l'ELF manquant, et `--boot-cache` échoue avec `E_STATE`. La page web, elle, lit l'ELF d'un dossier de build ou d'un `.pebundle` déposé |
| remplacement de ROM | `doctor` vérifie `PASSPORTSIM_ROM` et les clés `rom.rev101` / `rom.rev3` de `config.toml`, mais `start` démarre toujours la ROM intégrée ; il n'y a pas d'option `--rom` |
| démo intégrée | présente seulement si l'hôte de build a l'entrée de corpus `official` (l'image n'est jamais versionnée) ; le reçu indique si elle est là. Sans elle, `start` sans argument échoue avec `E_ASSET_MISSING` |

### Le payload, et `--version`

L'exécutable embarque tout le payload : ressources web, cœur wasm, schémas, documents, compétence
pour agents et démo. Le déplacer seul ailleurs ne fait rien perdre. `--version` (ou `-V`) affiche
l'empreinte du payload, égale au `payload.sha256` de `receipt.json`, et la revérifie :

```sh
./passportsim --version
```

```text
passportsim 0.1.0
payload: embedded, sha256 <the payload.sha256 of receipt.json>
```

| Ligne `payload:` | Signification |
|---|---|
| `embedded, sha256 <digest>` | un exécutable de paquet, intact |
| `package directory beside the binary, sha256 <digest>` | rien d'intégré ; le dossier `payload/` vérifié situé à côté est utilisé |
| `none: development build ...` | un simple `cargo build` ; la suite de la ligne explique comment obtenir un payload |
| `embedded, damaged: ...` | l'exécutable a été modifié ; remplacez-le |

## 7. Le reçu

`receipt.json` enregistre la version, le commit, la cible, le SHA-256 de chaque fichier du payload
et l'empreinte du payload, si la démo a été intégrée (et sinon pourquoi), si chaque ELF de ROM est
bien présent dans chaque artefact, et quelles règles du contrôle des secrets ont été appliquées. Il
ne contient ni chemin d'hôte, ni nom d'utilisateur, ni identité d'appareil.

La construction du paquet passe chaque fichier au contrôle des secrets ([secrets.md](secrets.md))
et échoue à la moindre détection. Les règles hachées ont besoin du fichier
`~/.config/passportsim/secrets-check.toml` propre à l'hôte ; sans lui, le reçu indique
`secrets.hashed_rules: false`.

Les paquets ne sont pas signés et il n'existe ni paquet Homebrew, ni winget, ni Scoop ; les étapes
de premier lancement des sections 2 et 8 les remplacent.

## 8. Windows

Le paquet Windows est `passportsim-0.1.0-windows-x64.zip`, avec `passportsim.exe`. Il se construit
sur un hôte Windows doté des outils MSVC, dans PowerShell :

```powershell
cargo run -q -p xtask -- package --target x86_64-pc-windows-msvc --payload-from <macOS package directory>
```

La commande écrit les quatre mêmes éléments qu'à la section 1, avec des archives `.zip`.
L'exécutable lie le runtime C de façon statique : rien n'est à installer sur la machine qui le
lance ; il déclare la prise en charge des chemins longs et la page de code UTF-8. La construction du
paquet échoue si l'un de ces contrôles échoue, et consigne les deux dans le bloc `windows` de
`receipt.json`.

**Un seul payload pour les deux hôtes.** La démo n'existe que sur l'hôte de build macOS, et les
octets du cœur wasm diffèrent d'un hôte à l'autre. `--payload-from` prend les deux dans un paquet
macOS du même commit, vérifie la démo, construit le reste et refuse de continuer si le payload
complet ne correspond pas, fichier par fichier, à celui du paquet macOS ; les deux reçus portent
alors le même `payload.sha256`. Sans cette option, le paquet Windows a son propre cœur wasm et pas
de démo. Il n'existe pas de paquet pour Windows on Arm.

**Premier lancement sous Windows.** Le `.exe` n'est pas signé : SmartScreen affiche « Windows a
protégé votre ordinateur ». Choisissez **Informations complémentaires**, puis **Exécuter quand
même**. Vous pouvez aussi retirer la marque de téléchargement dans PowerShell :

```powershell
Get-ChildItem -Recurse | Unblock-File
```

Les vérifications des sections 3 et 4 fonctionnent ensuite de la même façon :

```powershell
.\passportsim.exe --version
```

```powershell
.\passportsim.exe status
```

```powershell
.\passportsim.exe doctor
```

Tout le reste fonctionne comme décrit plus haut, avec `.\passportsim.exe` à la place de
`./passportsim`. Le démon en arrière-plan survit à la console qui l'a lancé ;
`passportsim serve --stop` l'arrête.

Les rôles de dossiers viennent des dossiers connus de Windows (known folders), et non de
`%USERPROFILE%` ou `%LOCALAPPDATA%`. Pour tous les déplacer, définissez `PASSPORTSIM_HOME=<dir>`.

## 9. Pour aller plus loin

- [commands/index.md](../../commands/index.md) : chaque commande avec son outil MCP, sa route HTTP
  et son étape de scénario (généré, en anglais).
- [errors.md](../../errors.md) : chaque code d'erreur (généré, en anglais).
- [SKILL.md](../../../skills/passportsim/SKILL.md) : la compétence pour agents, avec la liste de
  refus pour la sécurité des appareils.
- [secrets.md](secrets.md) : la politique des secrets.
- `docs/ARCHITECTURE.md` dans le dépôt : la conception (non livrée dans un paquet).

# Politique des secrets

[English](../../secrets.md) | [简体中文](../zh-CN/secrets.md) | [日本語](../ja/secrets.md) | **Français**

Les données d'appareil n'entrent jamais dans le dépôt, les journaux, les exports par défaut ou les
sorties visibles par un agent. Ce document dit ce qui compte comme secret, comment l'émulateur le
traite à l'exécution, et comment `cargo xtask secrets-check` et les hooks git le tiennent hors du
dépôt.

## 1. La règle d'identité

Aucun document, test, golden, journal, exemple, message de commit, issue ou pull request ne
reproduit, pour un appareil :

- une adresse MAC (de base ou dérivée) ;
- l'identifiant unique ;
- un mot de calibration eFuse ;
- un nom de fichier de sauvegarde ;
- le contenu de cardid.

Les exemples et les fixtures utilisent le préfixe de MAC fictive `02:00:00`. Les numéros de série
des appareils et les jetons du démon ne sont jamais copiés non plus. Pour parler d'une valeur
d'appareil, donnez son type et son emplacement (`calib_word` à `file:0x1c`), jamais la valeur.

Les fichiers issus d'un appareil restent sous la racine des données
(`~/Library/Application Support/passportsim/` sous macOS, `%LOCALAPPDATA%\passportsim\data\` sous
Windows), la partie appareil dans un dossier réservé au propriétaire.

## 2. Ce qui compte comme secret

| Donnée | Traitement par défaut | Activation explicite |
|---|---|---|
| Sauvegardes de la flash d'un appareil | jamais chargées, sauf par `--flash <path>`, qui contamine la machine | `--flash` plus `--allow-tainted` |
| Dumps eFuse bruts (MAC, identifiant unique, calibration) | jamais lus ; `--efuse synth` par défaut | `--efuse-dump <dir>` contamine |
| Identifiants NVS (Wi-Fi, clés d'appairage BLE, jetons d'application) | `inspect nvs` montre les espaces de noms, les clés et les types, jamais les valeurs ; les exports effacent les pages NVS | `inspect nvs --reveal` avec confirmation humaine |
| Partition cardid `[0x356000, 0x35A000)` | motif synthétique dans les images créées par l'émulateur ; jamais affichée, journalisée ni exportée ; le planificateur ne l'écrit jamais | aucune |
| MAC, identifiant unique, mots de calibration dans les sorties | valeurs fictives quand elles sont synthétisées ; masquées quand la machine est contaminée | `--reveal identity` avec confirmation humaine |
| Images de vraies cartes NFC | contamination au `nfc.load` ; `nfc.dump` masque l'UID et PWD/PACK | `--include-secrets` |
| Micro en direct, charges réseau pontées, HCI externe | journalisés localement pour la relecture, retirés des exports | `--include-secrets` |
| Jeton du démon et codes de lancement | dans le dossier d'exécution, réservés au propriétaire, jamais dans les sorties, journaux ou artefacts | aucune |
| Numéros de série des appareils, noms de fichiers de sauvegarde | jamais copiés dans le dépôt ou la documentation | aucune |

**Les ELF de ROM fournis ne sont pas secrets.** `assets/rom/esp32c3_rev101_rom.elf` et
`assets/rom/esp32c3_rev3_rom.elf` sont la publication esp-rom-elfs d'Espressif, publique et sous
Apache-2.0, versionnée avec sa [LICENSE](../../../assets/rom/LICENSE), sa
[NOTICE](../../../assets/rom/NOTICE) et les empreintes SHA-256 de
[pins.toml](../../../assets/rom/pins.toml). Les paquets contiennent ces trois fichiers.

**L'image de la démo officielle n'est jamais dans le dépôt.** `xtask package` l'intègre depuis le
corpus de firmwares de l'hôte de build uniquement si son SHA-256 correspond à l'empreinte figée et
si le texte de sa licence MIT est présent.

**Un chargement contaminant est réservé aux humains.** Toute entrée qui contamine une machine
(`--flash` d'une image dont les octets cardid ne sont pas tous à 0xFF ou qui contient des
identifiants NVS, `--efuse-dump`, `nfc.load` d'une vraie carte) est une opération de la ligne de
commande native soumise à confirmation humaine ; MCP et HTTP ne l'exposent jamais.

## 3. L'ensemble secret

Une seule fonction pure, `pemu_api::secret_set`, décide de ce qui compte comme identité. Le scanner
du dépôt et le masquage à l'exécution l'utilisent tous les deux : ils ne peuvent donc pas diverger.

Membres, avec le type que `secrets-check` signale sous la forme `hashed:<kind>` :

| Type | Membre | Formes |
|---|---|---|
| `mac` | MAC de base et base+1 à base+3 (station Wi-Fi, soft-AP, BT, Ethernet) | hexadécimal avec deux-points, tirets ou nu, dans les deux casses, et octets inversés ; MAC dérivées sous la forme ESP-IDF à rebouclage du dernier octet et sous la forme à retenue sur 48 bits |
| `mac_suffix` | suffixe NIC de 3 octets de chaque MAC | formes texte uniquement |
| `unique_id` | identifiant unique eFuse BLK2 (128 bits) | octets bruts et hexadécimal nu, dans les deux ordres d'octets |
| `calib_word` | chaque mot de calibration BLK2 non nul | 4 octets bruts dans les deux ordres, et texte hexadécimal |
| `backup_stem` | radicaux des noms de fichiers de sauvegarde | les octets du radical |
| `cardid` | contenu de la fenêtre cardid différent de 0xFF | haché par blocs de 32 octets |
| `nvs_credential` | valeurs d'identifiants NVS de 6 octets ou plus | octets bruts, hexadécimal et base64 ; à l'exécution uniquement |
| `nfc_uid`, `nfc_pwd`, `nfc_pack` | UID, mot de passe et accusé de mot de passe de l'étiquette NFC | à l'exécution uniquement |
| `canary` | le canari aléatoire de la section 5.6 | fichier de hachage uniquement |

### 3.1 Garde-fous contre les faux positifs

Pour que le contrôle reste assez discret pour qu'on lui fasse confiance, le constructeur écarte les
membres de moins de 4 octets (le PACK NFC de 2 octets ne garde que son texte hexadécimal), les
membres dont tous les octets sont identiques, les mots de calibration ayant moins de 2 octets non
nuls, et les radicaux de sauvegarde de moins de 6 octets. Il n'ajoute comme MAC dérivées que base+1
à base+3. Un fichier est du texte si ses 8000 premiers octets ne contiennent aucun octet NUL (la
règle de git) ; les membres de type texte uniquement ne correspondent que dans les fichiers texte.

## 4. Contamination et masquage

Comportement de l'émulateur à l'exécution :

- **Contamination.** Une machine construite à partir d'une entrée porteuse de secrets (la liste de
  la section 2, ou un pont en direct) est contaminée, tout comme ses forks, ses instantanés et ses
  entrées de cache de démarrage. Une machine contaminée :
  - refuse l'export d'instantané, de flash et d'artefacts avec `E_SECRET_REFUSED`, sauf si
    `--include-secrets` s'accompagne d'un code de confirmation humaine ;
  - affiche `tainted: true` dans son reçu ;
  - ne garde son cache de démarrage qu'en mémoire ;
  - masque la fenêtre cardid, les pages NVS et toute correspondance avec l'ensemble secret dans les
    outils de mémoire brute (`mem_read`, `watch`, `trace`, `inspect heap`).
- **Le masquage** s'applique à chaque sortie texte et JSON visible par un agent et à chaque fichier
  écrit dans le dossier des artefacts. Il travaille par valeur : une correspondance avec l'ensemble
  secret de la machine devient `<MAC>` ou `<SECRET>`, tandis que les adresses fournies par l'agent
  lui-même sont laissées telles quelles. Les artefacts binaires (btsnoop, pcap, dumps) sont
  réécrits de la même façon.
- **Les exports** remplissent la fenêtre cardid de 0xFF, effacent les partitions NVS, omettent les
  octets eFuse et retirent les charges en direct du journal. Un instantané masqué démarre avec une
  NVS d'usine et un cardid synthétique, et le signale dans son reçu.
- **Journaux.** Aucune télémétrie. Les journaux du démon sont masqués. Les rapports de plantage ne
  contiennent ni RAM ni flash, sauf demande de l'utilisateur.
- **Réseau.** Le relais et le pont refusent par défaut les plages de boucle locale, privées,
  link-local et ULA, et journalisent chaque destination.

## 5. Hygiène du dépôt

Les goldens texte normalisés et dont l'identité est masquée peuvent être versionnés ; le contrôle
les analyse quand même.

### 5.1 `.gitignore`

[`.gitignore`](../../../.gitignore) ignore les firmwares et les données d'appareil (`*.bin`,
`*.elf`, `efuse_blk*`, `*flash*.bin`, `cardid*`, `boot_log*`, `GROUND_TRUTH*`), les artefacts
d'exécution (`*.snap`, `*.pebundle`, `*.pcap`, `*.btsnoop`, `*.wav`, `/artifacts/`,
`.passportsim/`) et la configuration locale (`*.local.toml`, `secrets-check.toml`). Des entrées `!`
ne réintègrent que les deux ELF de ROM fournis et quelques ELF de test relus. `git add -f`
contourne `.gitignore` : le vrai garde-fou est donc `xtask secrets-check`.

### 5.2 Règles de motifs

Les règles de motifs n'ont besoin d'aucune donnée locale (`xtask/src/secrets/`) :

| Règle | Refuse |
|---|---|
| `mac-shape` | un texte en forme de MAC (six paires hexadécimales séparées par `:` ou `-`) autre que le préfixe `02:00:00`, l'adresse nulle et les adresses de groupe |
| `efuse-dump` | les fichiers binaires dont la taille et la structure correspondent à des dumps bruts de blocs eFuse (section 6.1) |
| `nvs-credential` | les partitions NVS contenant des clés d'identifiants |
| `cardid-window` | les octets différents de 0xFF dans la fenêtre cardid `[0x356000, 0x35A000)` de tout binaire assez long pour la contenir |
| `backup-name` | les noms de fichiers qui suivent les schémas de nommage des sauvegardes d'appareil (section 6.2) |
| `rom-pin` | tout binaire sous `assets/rom/` qui n'est pas un ELF figé dans `assets/rom/pins.toml`, ou tout binaire de ce dossier si `LICENSE` ou `NOTICE` manque |

Un ELF de ROM figé est exempté des règles de contenu. À la construction d'un paquet,
`cardid-window` ignore aussi l'exécutable `passportsim` et `pemu_wasm.wasm` tout juste compilés,
et seulement s'ils sont des exécutables structurellement valides (PE, ELF, Mach-O ou wasm) : tous
deux dépassent 0x35A000 octets, et cet offset contient donc du code. Un en-tête falsifié placé
devant une image flash reste refusé.

### 5.3 Règles hachées

Les règles hachées attrapent les valeurs d'appareil sous des formes qui échappent aux motifs
(hexadécimal nu, tableaux d'octets, octets inversés, mots de calibration, blocs de cardid).
`xtask secrets-check --init` hache chaque membre de l'ensemble secret avec un sel aléatoire dans
`~/.config/passportsim/secrets-check.toml` (`%APPDATA%\passportsim\secrets-check.toml` sous
Windows), réservé au propriétaire (mode 0600 dans un dossier 0700 sous macOS, une DACL protégée
sous Windows, revérifiée à chaque chargement). Ce fichier :

- n'entre jamais dans le dépôt et ne quitte jamais l'hôte qui détient les données d'appareil ;
- ne contient que des hachages SHA-256 salés, plus le canari aléatoire ;
- ne doit être ni lu, ni affiché, ni copié par un agent.

**Règle de sortie.** `secrets-check` n'affiche jamais le contenu trouvé, les valeurs des membres ni
les hachages : seulement les noms de règles, `file:offset` et des comptes. Un fichier qui déclenche
`backup-name` est signalé sans son nom.

### 5.4 Où s'applique chaque famille de règles

Les règles de motifs s'appliquent sur tous les hôtes. Les règles hachées s'appliquent partout où
existe un dossier d'appareil ; chacun de ces hôtes construit son propre fichier de hachage avec
`--init`, et doit le faire avant qu'un vrai flashage y ait lieu.

| Où | Règles de motifs | Règles hachées |
|---|---|---|
| `cargo xtask secrets-check` (arbre ou `--paths`) | oui | si le fichier de hachage existe ; sinon ignorées avec une note |
| hook pre-commit (`--staged`) et hook pre-push (`--hook pre-push`) | oui | oui ; refus en cas de doute |
| T0 sur un hôte avec un dossier d'appareil | oui | oui |
| T0 sur un hôte sans dossier d'appareil | oui | non ; le reçu indique "pattern rules only" |
| T1 | oui | oui |

Le contrôle ne trouve ses fichiers qu'à partir des dossiers connus (Windows) ou de `HOME` (macOS),
et refuse de s'exécuter tant que `PASSPORTSIM_HOME`, `PASSPORTSIM_CONFIG_DIR` ou
`PASSPORTSIM_DATA_ROOT` est défini, sauf si `--root` est donné : une redirection ne peut donc pas
masquer le dossier d'appareil.

En cas de doute, il refuse :

- un hook refuse sur une machine qui a un dossier d'appareil mais pas de fichier de hachage, et
  indique de lancer `cargo xtask secrets-check --init` ;
- tous les modes refusent un fichier de hachage illisible, mal formé ou non réservé au propriétaire ;
- les hooks font `exec cargo xtask ...` : un cargo absent ou un build en échec entraîne un refus,
  pas un contrôle sauté.

### 5.5 Commandes

```text
cargo xtask secrets-check [--root <dir>]                  scan git ls-files -co --exclude-standard
cargo xtask secrets-check [--root <dir>] --paths <files>  scan the given files
cargo xtask secrets-check [--root <dir>] --staged         what the pre-commit hook runs
cargo xtask secrets-check [--root <dir>] --hook pre-push  what the pre-push hook runs
cargo xtask hooks install [--root <dir>] [--force]        install both hooks (maintainer only)
cargo xtask secrets-check --init                          write the hash file (maintainer only)
cargo xtask secrets-check --self-test                     check the hash file (maintainer only)
```

Les agents ne lancent que les quatre premières. Avec `--paths`, les chemins relatifs se résolvent
par rapport à `--root` s'il est donné, sinon par rapport au dossier courant.

### 5.6 Armer le contrôle

Sur un nouveau clone, avant le premier commit, le mainteneur :

1. lance `cargo xtask secrets-check --init`, qui construit l'ensemble secret à partir du dossier
   d'appareil et écrit le fichier de hachage avec un nouveau sel et un canari aléatoire (il n'écrit
   rien si une lecture échoue, pour ne jamais hacher un ensemble partiel) ;
2. lance `cargo xtask hooks install` ;
3. lance `cargo xtask secrets-check --self-test`, qui vérifie que le scanner détecte chaque forme
   de membre et n'affiche que des comptes ;
4. indexe un fichier temporaire contenant le canari, vérifie que `git commit` est refusé, puis le
   retire de l'index et le supprime.

Les worktrees partagent les hooks. Relancer `--init` remplace le sel et le canari : refaites alors
l'étape 4.

### 5.7 Quand un commit est refusé

Le hook affiche une ligne par détection (règle, fichier, offset). Ensuite :

1. **Ne contournez pas le contrôle.** N'utilisez jamais `git commit --no-verify`, ne modifiez
   jamais `core.hooksPath`, ne modifiez ni ne supprimez les hooks, ne touchez pas au fichier de
   hachage.
2. **Corrigez le contenu, pas le contrôle.**
   - `mac-shape` dans la documentation ou les tests : utilisez une adresse fictive `02:00:00`.
   - `efuse-dump` ou `backup-name` sur une fixture : suivez la section 6.
   - `cardid-window` ou `nvs-credential` : le binaire n'a pas sa place dans le dépôt ;
     générez-le pendant le test ou gardez-le sous la racine des données.
   - `rom-pin` : seuls les ELF de ROM figés vivent dans `assets/rom/`.
   - `hashed:<kind>` : une vraie valeur d'appareil a atteint le fichier. Retirez-la et trouvez
     d'où elle vient.
3. **Signalez sans la valeur.** Donnez seulement la règle et `file:offset` ; ne citez jamais les
   octets trouvés.
4. **Un refus lié au fichier de hachage** (absent, illisible, mauvais mode) relève du mainteneur.
   Ne lancez pas `--init` vous-même.
5. **Un faux positif présumé** va au mainteneur avec la règle et `file:offset`. Il n'existe pas
   de liste d'exceptions, par conception ; la correction passe par le nom du fichier ou le format
   de la fixture.

## 6. Règles pour les fixtures

Aucune des deux règles n'a de liste d'exceptions : une liste d'exceptions est une brèche par
laquelle un vrai dump pourrait passer.

### 6.1 Règle de taille pour les petits binaires

`efuse-dump` refuse :

- tout fichier binaire de 24 ou 32 octets (un bloc eFuse brut) qui n'est pas un remplissage
  uniforme et n'a pas de signature de conteneur (ELF, PNG, GIF, JPEG, gzip, zip, zstd, wasm, PDF),
  y compris une empreinte SHA-256 brute de 32 octets stockée en `.bin` ;
- un fichier binaire de 336 octets (les onze blocs) sans signature de conteneur dont le champ MAC
  de BLK1 est une adresse unicast non nulle.

Générez les fixtures de ces tailles pendant le test ou utilisez un format conteneur ; stockez les
empreintes en texte hexadécimal. Les remplissages uniformes de 0x00 ou 0xFF sont acceptés.

### 6.2 Règle de nommage pour les images synthétiques

`backup-name` signale un chemin (sans tenir compte de la casse) quand :

- un composant de dossier est `passport-backups` ;
- le nom de fichier commence par `efuse_blk`, `cardid`, `boot_log` ou `ground_truth` ;
- le fichier est un dump brut (`.bin`, `.img`, `.dump`, `.dmp` ou `.raw`, éventuellement suivi de
  `.gz`, `.xz`, `.zst`, `.bz2`, `.zip` ou `.7z`) dont le radical contient `backup`, `dump`,
  `flash`, `full`, `efuse`, `nvs`, `cardid`, `readback`, `passport`, `4m`, `8m` ou `16m` ;
- un composant du chemin porte une MAC : une forme de MAC avec séparateurs, ou une suite de 12
  chiffres hexadécimaux comprenant au moins un chiffre et une lettre (adresses fictives, nulle et
  de groupe exceptées).

Nommez les images synthétiques sans ces mots, par exemple `synthetic_image.bin` ou
`seeded_card_erased.img`. Les mêmes mots figurent dans `.gitignore` : une fixture binaire a donc
aussi besoin d'une entrée `!` relue. Les radicaux exacts des sauvegardes sont détectés par valeur
par les règles hachées.

## 7. Portée des hooks

- **pre-commit** analyse les blobs indexés, pas l'arbre de travail. Réindexez le fichier après une
  correction.
- **pre-push** analyse chaque blob de `remote..local`. Pour une pointe distante nouvelle ou
  inconnue, il analyse l'arbre poussé plus les blobs des commits qui ne sont sur aucune référence de
  suivi distante, afin que l'historique ne puisse pas fuir par une nouvelle branche. Un
  `--hook pre-push` lancé à la main analyse `@{upstream}..HEAD`, ou l'arbre de `HEAD`.
- **Installation.** Les hooks vont dans `git rev-parse --git-path hooks` : les worktrees les
  partagent. Un hook existant sans le marqueur xtask est conservé, sauf avec `--force`. Les scripts
  sont des scripts `sh`, exécutés par le shell que Git fournit sur les deux hôtes.
- **Entrées de `--init`.** Le dossier d'appareil est `[paths] data_root` de la configuration
  locale, sinon la racine des données par défaut. Les sauvegardes sont les fichiers `.bin` placés
  directement dans le dossier des sauvegardes.
- **Tests.** Les builds de test de `xtask` refusent les modes appareil et hook et analysent avec un
  `HOME` vide : aucun test ne peut lire de données d'appareil ni le vrai fichier de hachage.

## 8. Sécurité des appareils

**Qui peut ouvrir l'appareil.**

- L'émulateur, le démon, ses points d'accès, la CI, les tests et les agents n'ouvrent jamais
  `/dev/cu.*`, `/dev/tty.*` ni un port `COM`, et ne lancent jamais `esptool`, `espefuse`,
  `idf.py flash` ni `idf.py monitor`.
- Seul `pemu-planner` avec la fonctionnalité `device` ouvre le port, et seulement après
  confirmation humaine (`docs/ARCHITECTURE.md`, « Flashing a real device »). La découverte énumère
  par VID/PID sans ouvrir de port, car la réinitialisation par défaut d'esptool ferait passer
  l'application en mode téléchargement. Flasher un appareil réel se fait uniquement par la ligne de
  commande native, jamais depuis le navigateur.

**Captures de développement** sur un appareil réel (goldens, exécutions de sondes, calibration) :

1. sauvegardez d'abord ;
2. n'écrivez que les segments du bootloader, de la table des partitions et de l'application ;
3. n'écrivez jamais la plage cardid 0x356000 à 0x359FFF ;
4. n'effacez jamais toute la flash et ne gravez jamais d'eFuses ;
5. restaurez ensuite Passport Keys depuis la sauvegarde et comparez le MD5 de cardid.

Tout autre usage de données d'appareil (comme `--efuse-dump`) demande l'accord explicite du
propriétaire de l'appareil.

**La confirmation humaine** arrête les appels d'outils erronés d'un agent coopératif. Elle
n'arrête pas un agent hostile disposant d'un shell sous le même utilisateur, qui pourrait lancer
esptool lui-même ; les garde-fous ci-dessous traitent ce cas. Voies de confirmation, dans l'ordre :

1. l'élicitation MCP, à laquelle l'utilisateur répond dans le client ;
2. une boîte de dialogue native ouverte par le démon (dans une session de bureau) ;
3. un code à usage unique sur le terminal de contrôle, uniquement pour une commande au premier plan
   dans une console interactive. Le code n'est jamais affiché dans l'interface web, renvoyé dans un
   résultat d'outil ni écrit dans un fichier.

Les mêmes voies protègent `inspect nvs --reveal`, `--reveal identity`, `--include-secrets` et les
chargements contaminants.

**Garde-fous livrés.** La [compétence pour agents](../../../skills/passportsim/SKILL.md) et
[`device-deny.json`](../../../skills/passportsim/device-deny.json) (pour la liste
`permissions.deny` d'un `.claude/settings.json` de Claude Code) refusent les écritures courantes qui
atteignent un appareil réel : `esptool`, `esptool.py`, `python -m esptool`, `py -m esptool`, un
chemin vers le `python.exe` d'un environnement virtuel, `esptool.exe`, `espefuse`, `espefuse.exe`,
`idf.py flash`, `idf.py -p /dev/cu.*`, `idf.py -p COM*` et les chemins `\\.\COM*`, et font passer
le flashage par le planificateur. Cette liste fait au mieux, car une liste de motifs ne peut pas
couvrir tous les shells et toutes les façons de citer. **La liste d'autorisation réellement
appliquée se trouve là où l'argument est analysé** : le planificateur et les outils de la
compétence n'acceptent comme port que `socket://127.0.0.1:*` et `rfc2217://127.0.0.1:*`, avant de
lancer le moindre processus.

Ce document est livré dans chaque paquet, à côté de la compétence. Son lien vers `.gitignore` n'y
fonctionne pas, car un paquet ne contient aucun fichier du dépôt.

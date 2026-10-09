# Déployer le bundle web sur Cloudflare Workers

[English](../../deploy-cloudflare.md) | [简体中文](../zh-CN/deploy-cloudflare.md) | [日本語](../ja/deploy-cloudflare.md) | **Français**

Le bundle web (`passportsim-0.1.0-web/`, section 5 du [démarrage rapide](quickstart.md)) est un
site statique que `cargo xtask package` écrit sous la forme d'un projet Cloudflare Workers prêt à
l'emploi. Déployez-le depuis son dossier sans rien modifier, ou lancez `just deploy` depuis une
copie du dépôt.

Le déploiement du projet lui-même est https://passportsim.bugs.cc : le Worker `passportsim` avec
le firmware de démo, et le domaine personnalisé rattaché dans le tableau de bord.

## Ce que le paquet écrit

Quatre fichiers à côté de la page (`xtask/src/package/cloudflare.rs`) :

| Fichier | Rôle |
|---|---|
| `wrangler.jsonc` | Un Worker nommé `passportsim` qui sert le dossier du bundle comme ressources statiques, avec un seul script, `play-relay.js`. Les ressources sont servies d'abord : le script ne s'exécute que pour un chemin auquel aucun fichier ne correspond. `workers_dev: true` conserve l'URL `workers.dev` quand un domaine personnalisé est aussi déployé. |
| `_headers` | Les en-têtes qu'envoie `passportsim serve` : `Cross-Origin-Opener-Policy: same-origin` et `Cross-Origin-Embedder-Policy: require-corp` (nécessaires à `SharedArrayBuffer`), `Cross-Origin-Resource-Policy: same-origin` et `X-Content-Type-Options: nosniff`. Il fixe aussi le type de contenu du `.pebundle` de la démo et des textes de licence. |
| `.assetsignore` | Tient `wrangler.jsonc`, `play-relay.js` et le dossier d'état `.wrangler/` hors du site. |
| `play-relay.js` | Le script du Worker : le relais du champ des jeux vers `ai-passport.folotoy.cn` (voir plus bas). C'est `web/src/edge/playRelay.js` tel quel. |

Workers répond avec un `ETag` et `Cache-Control: public, max-age=0, must-revalidate` : un
navigateur ne retélécharge donc la démo de 23,5 MiB que si elle a changé. `pemu_wasm.wasm` est
servi en `application/wasm`.

## Le déploiement ne stocke rien

Chaque fichier de la page est une ressource statique, servie sans exécuter de code. Tout ce que
fait un visiteur se passe dans son navigateur. Un firmware déposé est lu localement et transmis au
Web Worker de la page (le « Worker » dont parle la page est ce thread du navigateur, pas un
Cloudflare Worker), les instantanés restent dans la mémoire de la page, et l'historique des
firmwares est dans l'IndexedDB du navigateur pour ce site.
`web/tests/local.spec.ts` échoue sur toute requête qui n'est pas un GET vers l'origine de la page.

Le seul code côté serveur est le relais du champ des jeux. Le site des jeux de FoloToy ne répond
qu'à ses propres pages : un navigateur sur une autre origine ne peut pas le lire. Quand un visiteur
saisit le lien ou le numéro d'un jeu, la page demande à sa propre origine
`/play-site/api/plays/id/<numéro>` puis `/play-site/api/download/...`, et le script du Worker
transmet ces deux GET à `ai-passport.folotoy.cn` (`web/src/edge/playRelay.js`). C'est une liste
d'autorisation, pas un proxy : l'hôte amont est fixe, tout autre chemin est un 404, ni la chaîne de
requête, ni les cookies, ni les en-têtes du visiteur ne sont transmis, une redirection n'est pas
suivie, et une requête faite par la page d'un autre site est refusée. Il ne stocke ni ne journalise
rien ; comme tout serveur, il voit l'adresse du visiteur et le jeu demandé, et FoloToy voit une
requête venant de Cloudflare. La page vérifie le firmware avec la taille et le SHA-256 annoncés par
le site des jeux avant de le charger.

Une requête relayée exécute le script du Worker et compte donc dans la limite de requêtes de
Workers (voir Limites) ; une requête pour un fichier de la page ne compte pas. `passportsim serve`
et un simple serveur statique n'ont pas de relais : le champ des jeux le dit alors et donne le lien
de la page du jeu pour un téléchargement à la main. `just run` sert la page avec le même relais,
pour le développement.

## Déployer

Il faut [Bun](https://bun.sh) (ou Node avec `npx`) et un compte Cloudflare. Depuis le dossier du
bundle :

```sh
cd target/package/passportsim-0.1.0-web
bunx wrangler@4 login     # une fois par machine ; ouvre le navigateur
bunx wrangler@4 deploy    # envoie le bundle et affiche l'URL https
```

`--name <name>` déploie sous un autre nom de Worker, qui donne aussi l'hôte
`<name>.<account subdomain>.workers.dev`. Pour votre propre domaine, ajoutez une entrée `routes` à
`wrangler.jsonc` ou rattachez un domaine personnalisé dans le tableau de bord.

Pour l'essayer localement dans le runtime Workers, sans compte :

```sh
bunx wrangler@4 dev --ip 127.0.0.1 --port 8787 --persist-to "$TMPDIR/passportsim-wrangler"
```

(dans PowerShell, `--persist-to "$env:TEMP\passportsim-wrangler"`), puis ouvrez
`http://127.0.0.1:8787/`. Gardez `--persist-to` hors du bundle : le `.wrangler/state` par défaut
se trouve dans le dossier des ressources, et Wrangler se recharge alors à chacune de ses propres
écritures de cache et coupe les réponses, si bien que la page ne démarre jamais.

`web/tests/cloudflare.spec.ts` vérifie un `wrangler dev` en cours d'exécution dans Chromium
(en-têtes d'isolation, types de contenu, démarrage de la démo, dépôt d'un firmware). Il ne tourne
que s'il est nommé :

```sh
cd web
PEMU_E2E_CLOUDFLARE_URL=http://127.0.0.1:8787/ PEMU_E2E_CLOUDFLARE_IMAGE=<a merged .bin> \
  bun run e2e --project=chromium tests/cloudflare.spec.ts
```

## Intégration continue et publications

Workflows de `.github/workflows/` :

| Workflow | Déclenché par | Rôle |
|---|---|---|
| `pr-check.yml` | pull requests, pushs sur `main` | `cargo xtask ci t0` sous macOS et Windows, réparti par `--group` en jobs parallèles, et la vérification des types et les tests Playwright de la page (Chromium, Firefox, WebKit) sous macOS. Les runners n'ont pas de corpus de firmwares : les tests de corpus indiquent SKIPPED-CORPUS. |
| `deploy-web.yml` | à la main, ou depuis `release.yml` | Construit le bundle web sous macOS, le déploie avec `wrangler deploy` et vérifie le site en ligne. |
| `release.yml` | à la main, depuis l'onglet Actions | Calcule la version suivante à partir des tags `v*` (ou prend celle indiquée), construit les paquets macOS arm64 et Windows x64 à cette version, crée le tag, publie une GitHub Release avec des notes générées, les archives et `SHA256SUMS.txt`, puis déploie le bundle web. |

Réglages du dépôt (Settings, Secrets and variables, Actions ; les secrets peuvent aussi vivre dans
l'environnement `production`) :

| Nom | Type | Rôle |
|---|---|---|
| `CLOUDFLARE_API_TOKEN` | secret | Un jeton créé avec le modèle « Edit Cloudflare Workers », limité au compte, et à la zone si un domaine personnalisé est défini. |
| `CLOUDFLARE_ACCOUNT_ID` | secret | Le compte qui héberge le Worker. |
| `CF_CUSTOM_DOMAIN` | variable, optionnelle | Un domaine personnalisé déclaré à chaque déploiement. |
| `CF_WORKER_NAME` | variable, optionnelle | Un nom de Worker autre que `passportsim`. |
| `DEMO_SITE_URL` | variable, optionnelle | Un site déployé d'où télécharger le firmware de démo (voir plus bas). |
| `DEMO_BUNDLE_SHA256` | variable, optionnelle | Le SHA-256 du `official.pebundle` de ce site, décompressé : le site le sert compressé en gzip. |

**Domaine personnalisé.** Sans cette variable, un déploiement laisse les domaines du Worker tels que
le tableau de bord les a définis. Avec elle, chaque déploiement passe `--domain <name>`, qui en fait
le seul domaine personnalisé du Worker. Un fork la laisse vide et obtient son URL `workers.dev`.
Après l'envoi, le job récupère la page, le cœur wasm et la démo depuis les URL en ligne et échoue
si les en-têtes d'isolation manquent ou si le cœur n'est pas celui qui vient d'être envoyé.

**Firmware de démo.** La démo n'est jamais versionnée et les runners n'ont pas de corpus : un
workflow la télécharge donc depuis un site qui la sert déjà (`official.pebundle` et les deux
fichiers `licenses/official-demo.*`). Faites le premier déploiement avec la démo à la main
(`just deploy`) depuis une machine qui a le corpus, puis définissez `DEMO_SITE_URL` et
`DEMO_BUNDLE_SHA256` (`gzip -dc official.pebundle | shasum -a 256` sur la copie compressée en gzip du bundle web). Le job vérifie l'empreinte, et
`cargo xtask package --demo <dir>` n'accepte les fichiers que s'ils sont exactement ce que ce
commit produirait à partir de l'image figée. Un échec du téléchargement ou de la vérification fait
échouer le job avant tout déploiement. Si les deux variables sont vides, le site est déployé sans
la démo et le résumé du job le signale.

## Limites

D'après la documentation de Cloudflare, consultée le 2026-09-27 :

| Limite | Gratuit | Payant | Source |
|---|---|---|---|
| Un fichier de ressource statique | 25 MiB (26 214 400 octets) | 25 MiB | [Workers limits, static assets](https://developers.cloudflare.com/workers/platform/limits/#static-assets) |
| Fichiers de ressources par version de Worker | 20 000 | 100 000 (Wrangler 4.34.0 ou plus récent) | idem |
| Taille totale des ressources | aucune limite indiquée | aucune limite indiquée | idem |
| Requêtes vers les ressources statiques | gratuites et illimitées | gratuites et illimitées | [Billing and limitations](https://developers.cloudflare.com/workers/static-assets/billing-and-limitations/) |
| Requêtes qui exécutent le script du Worker (le relais des jeux, deux par jeu chargé) | 100 000 par jour, puis erreur 1027 pour ces requêtes | sans limite, facturées | [Workers limits](https://developers.cloudflare.com/workers/platform/limits/), consultée le 2026-10-08 |
| Règles `_headers` / longueur de ligne | 100 règles, 2 000 caractères par ligne | idem | [Headers](https://developers.cloudflare.com/workers/static-assets/headers/) |

Wrangler refuse tout le déploiement si un seul fichier dépasse 25 MiB. Un bundle avec la démo
compte 18 fichiers ; le plus gros est `pemu_wasm.wasm`, environ 7,0 Mo, suivi de `official.pebundle`,
environ 6,1 Mo : c'est la démo de 24,7 Mo compressée en gzip, car Cloudflare envoie un fichier
`application/octet-stream` tel quel, et la page le décompresse. `cargo xtask package` échoue en nommant le
fichier si l'un dépasse la limite ou s'il y en a plus de 20 000, et affiche la marge à chaque
exécution.

## Ce qu'il faut décider

- **Le compte**, et qui en détient l'accès.
- **L'adresse** : le nom du Worker et son hôte `workers.dev`, ou une route ou un domaine
  personnalisé.
- **Publier ou non la démo.** `official.pebundle` est une image précompilée de la démo BSP de
  l'AI Passport de FoloToy, publiée par FoloToy sous licence MIT ; la licence et la notice
  l'accompagnent dans `licenses/`. Pour ne pas la publier, supprimez `official.pebundle` avant de
  déployer ; la page s'ouvre alors sans machine et demande un firmware.
- **Le cache.** Les réglages par défaut revalident à chaque chargement. Un cache plus long demande
  d'abord des noms de fichiers versionnés, car la page demande des noms fixes (`main.js`,
  `pemu_wasm.wasm`).

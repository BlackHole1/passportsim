# Politique de sécurité

[English](../../../SECURITY.md) | [简体中文](../zh-CN/SECURITY.md) | [日本語](../ja/SECURITY.md) | **Français**

## Versions prises en charge

PassportSim n'a pas encore atteint la version 1.0. Les correctifs de sécurité arrivent sur `main`
et dans le paquet suivant ; les anciens paquets ne sont pas corrigés.

## Signaler une vulnérabilité

Ne signalez pas de problème de sécurité dans une issue ou une pull request publique. Écrivez à
**bh@bugs.cc** en indiquant :

- en quoi consiste le problème et ce qu'un attaquant pourrait en faire ;
- les étapes, l'entrée ou l'image de firmware qui le reproduisent (sans données d'un appareil
  réel, voir plus bas) ;
- la version (`passportsim --version`) et le système d'exploitation de votre hôte.

Vous recevrez un accusé de réception sous 3 jours ouvrés et une évaluation sous 10. Nous vous
tiendrons informé, conviendrons avec vous d'une date de divulgation et vous citerons, sauf si vous
préférez l'anonymat.

## Dans le périmètre

L'émulateur exécute des firmwares non fiables et, sur demande, communique avec un appareil réel.
Ces frontières relèvent de la sécurité :

- **La frontière de l'invité.** Un firmware ne doit ni lire ni écrire les fichiers de l'hôte, ni
  atteindre le réseau au-delà de ce que le LAN virtuel relie explicitement, ni faire planter le
  processus hôte d'une façon qui affecte d'autres instances.
- **Le démon.** `passportsim serve` n'écoute que sur la boucle locale, et chaque requête HTTP,
  WebSocket ou MCP exige son jeton réservé au propriétaire ou le code de lancement à usage unique de
  l'interface web. Tout moyen de contourner l'un ou l'autre est une vulnérabilité.
- **Le flasheur d'appareil réel.** `flash_device` sauvegarde d'abord la flash, n'écrit ni n'efface
  jamais les zones qui identifient l'appareil, n'efface jamais la puce entière et ne grave jamais
  d'eFuses. Toute entrée qui l'y amène est une vulnérabilité.
- **Les données secrètes.** Les données d'appareil (adresses MAC, identifiants uniques, contenu de
  carte, dumps de flash ou d'eFuse, noms de sauvegardes) ne doivent jamais apparaître dans la
  sortie des outils, les journaux, les artefacts, le dépôt ou un paquet.
- **Les paquets.** Un paquet ne doit contenir ni les chemins du compte de la machine de build, ni
  rien de ce que le contrôle des secrets refuse.

## Hors périmètre

- Les bogues du firmware émulé lui-même, ou les différences entre l'émulateur et la carte. Ce sont
  des problèmes de fidélité : ouvrez une issue normale.
- Les attaques qui supposent du code déjà exécuté sous votre compte sur l'hôte.
- Un firmware qui boucle indéfiniment ou épuise sa propre mémoire ; `run` accepte un budget de
  temps.

## Données d'appareil dans les rapports

Ne joignez jamais de dumps de flash ou d'eFuse, de sauvegardes ou de journaux issus d'un appareil
réel. Si un rapport en a besoin, dites-le et nous conviendrons d'un canal privé. Voir
[secrets.md](secrets.md).

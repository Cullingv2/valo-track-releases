# Valo Overlay : fonctionnement et développement

Ce document décrit ce que fait l'overlay en détail et comment le compiler. Pour la présentation et le téléchargement, voir le [README](README.md).

Overlay pour Valorant : **Alt+Z** ouvre par-dessus le jeu une fenêtre avec les rangs, le pic, le winrate, le niveau et les groupes des joueurs de ta partie. Un clic sur un joueur ouvre sa **carrière**, en trois parties clairement séparées : **l’acte en cours** (données officielles Riot : rang, RR, parties, winrate, pic de l’acte, placements), **l’historique classé** (rang de fin de chaque acte, pic global, total de parties) et les **statistiques de tout l’acte** (remises à zéro à chaque nouvel acte ; un clic sur un acte de l’historique affiche les siennes). Chaque match n’est téléchargé qu’une fois (cache disque), l’analyse se fait en parallèle et les stats se remplissent au fur et à mesure : pic de l'acte et pic global, victoires / défaites, dégâts par manche, K/D, headshot %, KAST, DDΔ, ACS, KAD, first bloods, manches flawless, aces, graphiques (score de combat par match, victoires / défaites avec l’écart de manches, évolution des RR, avec infobulles), statistiques par agent et par carte. Un clic sur un match ouvre son **détail** : score, manches une à une (élimination, spike, temps), et les 10 joueurs avec rang, K/D/A, K/D, ACS, ADR, HS %, KAST, first bloods / deaths, multi-kills, groupes et MVP. Un clic sur un joueur du match ouvre sa carrière.

Après chaque partie, un **écran de fin de partie** s'ouvre au prochain Alt+Z : victoire ou défaite, RR gagnés ou perdus, la **médaille** donnée par Riot (Distinction, Mérite, Réussite) avec ton score de performance et le détail attaque / soutien, tes stats du match, ton score de combat manche par manche et le tableau des scores. En partie, les **groupes** sont affichés pour les deux équipes : Riot ne donne que ceux de tes amis, les autres sont déduits du dernier match de chaque joueur.

Au démarrage de l'application : une courte intro (le V se dessine, VALO//TRACK, message d'accueil), qu'un clic ou Échap passe. Les animations (intro, transitions, compteurs, jauges) n'utilisent que `transform` / `opacity`, sont jouées une seule fois par action, et peuvent être coupées dans la config.

- **Backend Rust** (Tauri 2) : lit le client Riot local (`lockfile`) et interroge directement les serveurs Valorant avec les jetons de ta session (jeu lancé ou non). Données publiques en complément via HenrikDev (optionnel : clé ou serveur relais).
- **Interface** HTML/CSS dans la WebView2 de Windows, inspirée de l'interface de Valorant. Assets officiels (icônes de rang, agents, cartes, cartes de joueur) via `media.valorant-api.com`.
- Fenêtre sans bordure aux coins arrondis (Windows 11), toujours au premier plan, déplaçable par l'en-tête. À la fermeture (Alt+Z, Échap ou ×), le focus revient au jeu.

## Installer

Télécharge la dernière version sur la [page des releases](../../releases/latest) (`ValoOverlay-x.y.z-setup.exe`, Windows 10/11 64 bits) et lance-la. L'installation se fait pour l'utilisateur courant, sans droits administrateur, avec un raccourci dans le menu Démarrer. Tu peux aussi construire l'installeur toi-même (voir « Développer »).

L'application vit dans la zone de notification (icône V rouge) :
- clic gauche : afficher / masquer
- clic droit : Mode démo, Quitter

Relancer l'application quand elle tourne déjà ouvre simplement l'overlay.

## Utilisation

| Action | Effet |
| --- | --- |
| Alt+Z | afficher / masquer |
| clic sur un joueur | carrière du joueur |
| clic sur un match (carrière) | détail du match |
| Échap | retour (vue précédente), puis fermer |
| glisser l'en-tête | déplacer la fenêtre |

- Les pseudos des joueurs d’un match (que Riot ne fournit plus) sont complétés par HenrikDev (clé ou serveur relais).
- Mets Valorant en **plein écran fenêtré**, sinon aucune fenêtre ne peut s'afficher par-dessus.
- **Alt+Z est aussi le raccourci de l'overlay NVIDIA.** Si les deux s'ouvrent, change l'un des deux.
- Les stats ne comptent que les **modes à manches** (compétition, non classé, vélocité, Spike Rush, Premier…) : les matchs à mort, Escalade et parties personnalisées sont exclus, car leurs scores n’ont pas de sens en moyenne. Si Riot limite les requêtes, l’analyse fait une pause puis reprend seule.
- **Riot ne fournit que les ~100 derniers matchs de chaque joueur**, tous modes confondus (pour un gros joueur, à peine une ou deux semaines). Pour un acte plus ancien, seul le bilan officiel est disponible (parties, victoires, rang, victoires par rang). L’overlay garde une **archive locale** de chaque match analysé (`%LOCALAPPDATA%\fr.valooverlay.app\matches` et `players`) et **archive tes propres matchs automatiquement** (au démarrage puis toutes les 20 min, jamais pendant une partie) : tes prochains actes resteront consultables en détail.
- Les joueurs en **mode incognito** gardent leur pseudo masqué (sauf toi et ton groupe).
- Les endpoints utilisés ne sont pas documentés officiellement par Riot et peuvent changer après une mise à jour. L'outil ne touche pas au processus du jeu, mais son usage reste à tes risques.

## Configuration

`%APPDATA%\fr.valooverlay.app\config.json` (créé au premier lancement) :

```json
{
  "hotkey": "Alt+Z",
  "mode": "toggle",
  "language": "fr-FR",
  "intro": "launch",
  "animations": true
}
```

- `hotkey` : `"Alt+Z"`, `"Ctrl+Shift+V"`, `"F10"`…
- `mode` : `"toggle"` (un appui ouvre / ferme) ou `"hold"` (visible tant que la touche est enfoncée)
- `language` : langue des noms d'agents, de cartes et de rangs
- `intro` : `"launch"` (la fenêtre s'ouvre sur l'intro au démarrage de l'application), `"daily"` (première ouverture du jour), `"always"` (à chaque ouverture) ou `"never"`
- `animations` : `false` pour tout couper (PC très modestes)
- `apiServer` / `apiToken` : serveur relais Valo Overlay (dossier `server/`, à héberger sur un VPS). Il garde la clé HenrikDev côté serveur et partage un cache entre tous les joueurs : stats complètes de l’acte et pseudos des matchs, sans clé personnelle.
- `henrikApiKey` : clé gratuite de l’API HenrikDev (optionnelle). Riot ne liste que les ~100 derniers matchs d’un joueur : avec cette clé, les stats principales de tout l’acte s’affichent immédiatement à partir des résumés HenrikDev (25 matchs par requête, identiques aux données Riot), puis le détail de chaque match est téléchargé chez Riot en arrière-plan pour les stats avancées (KAST, first bloods, clutchs…). Les joueurs de la partie sont préchargés pendant la sélection d’agents. Seul l’identifiant du joueur est envoyé à HenrikDev.

## Développer

```bash
cd src-tauri
cargo run
```

Construire l'installeur : `npx @tauri-apps/cli@2 build` à la racine du projet (ou `cargo tauri build` dans `src-tauri`) ; il sort dans `src-tauri/target/release/bundle/nsis/`. Tests : `cargo test` dans `src-tauri`.

Serveur relais intégré à l'application (optionnel) : copie `src-tauri/.cargo/config.toml.example` en `src-tauri/.cargo/config.toml` et renseigne l'adresse du serveur et son jeton (`VALO_API_SERVER`, `VALO_API_TOKEN`). Ce fichier n'est pas versionné : le jeton n'apparaît jamais dans le code source. Sans lui, l'application fonctionne avec les données Riot (et une éventuelle clé HenrikDev personnelle dans `config.json`).

```
src-tauri/src/
  main.rs      démarrage Tauri
  riot.rs      lockfile, jetons, région/version, requêtes glz / pd / shared
  tracker.rs   boucle salon → sélection → en jeu, pseudos, rangs, groupes
  career.rs    historique et statistiques d'un joueur
  assets.rs    métadonnées valorant-api.com (cache disque 3 jours)
  overlay.rs   fenêtre, focus, raccourci global, icône de notification
  config.rs    config.json
  henrik.rs    API HenrikDev (serveur relais ou clé), quota
  directory.rs joueurs connus et leur région
server/        relais HenrikDev pour VPS (Docker + HTTPS), voir server/README.md
ui/
  index.html, style.css, app.js   l'interface
  demo.js                         données fictives du mode démo
```

Aperçu du design dans un navigateur : `node tools/gen-dev-assets.mjs ui/dev-assets.json`, sers le dossier `ui/` avec un serveur statique, puis ouvre `index.html?phase=ingame` (ou `pregame`, `menus`, `waiting`).

## Contribuer

Les issues et les pull requests sont les bienvenues : bug, idée, stat qui te semble fausse (avec l'identifiant du match si possible). Le projet vise Windows uniquement (c'est la seule plateforme de Valorant).

## Licence

Code sous licence [MIT](LICENSE) : tu peux l'utiliser, le modifier et le redistribuer librement, en gardant la mention de copyright.

## Mentions légales

Valo Overlay n'est ni approuvé ni sponsorisé par Riot Games et ne reflète pas l'opinion de Riot Games ni de quiconque ayant participé à la production ou à la gestion de ses propriétés. Riot Games et toutes les propriétés associées sont des marques commerciales ou déposées de Riot Games, Inc. Les visuels du jeu (rangs, agents, cartes) proviennent de [valorant-api.com](https://valorant-api.com).

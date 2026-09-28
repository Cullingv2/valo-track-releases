# Serveur relais Valo Overlay

Petit serveur (un seul fichier, Node.js sans dépendance) qui donne à **tous** les joueurs de l'overlay les stats complètes de l'acte et les pseudos des matchs, sans avoir chacun une clé HenrikDev :

- **la clé HenrikDev reste sur le serveur** : l'application n'envoie qu'un jeton d'application ;
- **cache partagé** : un match, un compte ou un rang demandé par un joueur est servi instantanément aux autres (les matchs, qui ne changent plus, sont gardés sur disque, compressés) ;
- **quota respecté** : le serveur lit le quota HenrikDev restant et étale les requêtes (au besoin l'application patiente toute seule, sans message) ;
- seules quelques routes HenrikDev en lecture sont relayées (rang, résumés et détail des matchs), avec une limite de requêtes par client.

## Installation sur un hébergeur Node.js avec panel (Pterodactyl…), à côté d’un bot Discord

1. Envoie à la racine (SFTP ou gestionnaire de fichiers du panel) les 2 fichiers du dossier `a-envoyer/` : `server.mjs` et `valo-relay.env` (clé HenrikDev et jeton déjà remplis, à ne jamais partager). Ils ne remplacent aucun fichier du bot.
2. Dans le fichier principal du bot (celui indiqué par `"main"` dans `package.json`), ajoute tout en haut :
   ```js
   import("./server.mjs"); // relais Valo Overlay (même processus que le bot)
   ```
   (chemin relatif à ce fichier : `import("../server.mjs")` s’il est dans `src/`).
3. Redémarre. La console affiche `[valo-relay] relais sur http://0.0.0.0:PORT` : c’est le port attribué par le panel (onglet Réseau). Une erreur du relais n’arrête jamais le bot.
4. Vérifie : `http://adresse-du-serveur:PORT/v1/health`.

Seul (sans bot) : `node server.mjs`.

## Installation sur un VPS (Docker, HTTPS automatique)

Il faut un nom de domaine (ou sous-domaine) qui pointe vers l'IP du VPS, et les ports 80 / 443 ouverts.

```bash
git clone … valo-overlay && cd valo-overlay/server   # ou copie juste ce dossier sur le VPS
cp .env.example .env
nano .env            # DOMAIN, HENRIK_KEY, APP_TOKEN (openssl rand -hex 24)
docker compose up -d
curl https://valo.exemple.fr/v1/health
```

Sans Docker : Node.js 18+, copie `server.mjs` et `.env` dans `/opt/valo-relay`, puis `valo-relay.service` (systemd) et un Caddy / nginx devant pour le HTTPS.

## Brancher l'application

Dans `%APPDATA%\fr.valooverlay.app\config.json` :

```json
{
  "apiServer": "https://valo.exemple.fr",
  "apiToken": "le même jeton que APP_TOKEN"
}
```

(Pour une version distribuée à d'autres joueurs, ces deux valeurs peuvent être intégrées à l'application : la clé HenrikDev, elle, ne quitte jamais le serveur.)

## Quota HenrikDev

Une clé « Basic » donne 30 requêtes par minute pour **tout le serveur**. Le cache absorbe l'essentiel (matchs partagés entre les 10 joueurs d'une partie, pages déjà vues), mais si beaucoup de monde utilise l'overlay, demande une clé de niveau supérieur sur le Discord de HenrikDev. `GET /v1/health` affiche le nombre de requêtes, le taux de cache et le quota restant.

## Routes

| Route | Cache |
| --- | --- |
| `/v1/henrik/valorant/v3/by-puuid/mmr/{région}/pc/{puuid}` | 3 min |
| `/v1/henrik/valorant/v1/by-puuid/stored-matches/{région}/{puuid}?page=&size=&mode=` | 5 min (page 1), 30 min |
| `/v1/henrik/valorant/v4/match/{région}/{id}` | permanent (disque) |
| `/v1/health` | — |

Toutes les routes sauf `/v1/health` exigent l'en-tête `X-App-Token` quand `APP_TOKEN` est défini.

// Valo Overlay — serveur relais HenrikDev.
//
// Garde la clé HenrikDev (jamais envoyée aux joueurs), met les réponses en cache pour tout le
// monde (un match déjà demandé par un joueur est servi instantanément aux autres), respecte le
// quota HenrikDev.
// Aucune dépendance : Node.js 18 ou plus récent.

import http from "node:http";
import fs from "node:fs";
import path from "node:path";
import zlib from "node:zlib";
import crypto from "node:crypto";
import { fileURLToPath } from "node:url";

// Réglages : variables d'environnement, sinon fichier valo-relay.env (ou .env) à côté du script
// (hébergeurs de type Pterodactyl, où l'on ne peut pas définir ses propres variables).
// Peut tourner seul (node server.mjs) ou dans le même processus qu'un bot Discord
// (import("./server.mjs") dans son fichier principal) : il n'arrête jamais le processus.
const HERE = path.dirname(fileURLToPath(import.meta.url));
const DOTENV = {};
for (const name of [".env", "valo-relay.env"]) {
  try {
    for (const line of fs.readFileSync(path.join(HERE, name), "utf8").split(/\r?\n/)) {
      const m = line.match(/^\s*([A-Z_][A-Z0-9_]*)\s*=\s*(.*?)\s*$/);
      if (m) DOTENV[m[1]] = m[2].replace(/^(["'])(.*)\1$/, "$2");
    }
  } catch {}
}
const env = (k, d) => (process.env[k] ?? DOTENV[k] ?? "").trim() || d;
// RELAY_PORT en priorité, sinon SERVER_PORT (port attribué par le panel de l'hébergeur)
const PORT = Number(env("RELAY_PORT", env("SERVER_PORT", env("PORT", "8787"))));
const HOST = env("HOST", "0.0.0.0");
const KEY = env("HENRIK_KEY", "");
const TOKEN = env("APP_TOKEN", "");
const DATA = path.resolve(HERE, env("DATA_DIR", "./valo-relay-data"));
const RATE_PER_MIN = Number(env("RATE_PER_MIN", "240"));
const MEMORY_MB = Number(env("CACHE_MEMORY_MB", "256"));
const DISK_MB = Number(env("CACHE_DISK_MB", "4000"));
const MAX_WAIT_MS = Number(env("MAX_WAIT_S", "25")) * 1000;
const UPSTREAM = "https://api.henrikdev.xyz";

const log = (...a) => console.log(new Date().toISOString(), "[valo-relay]", ...a);
try {
  fs.mkdirSync(path.join(DATA, "matches"), { recursive: true });
} catch (e) {
  log("dossier de données impossible à créer :", e.message);
}

/* ───────────── Routes autorisées ───────────── */

const REGION = "(eu|na|ap|kr|latam|br)";
const UUID = "[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}";
const MIN = 60_000;
const ROUTES = [
  // Résumés des matchs d'un joueur (25 par page)
  { re: new RegExp(`^valorant/v1/by-puuid/stored-matches/${REGION}/${UUID}$`), query: { page: /^\d{1,3}$/, size: /^\d{1,2}$/, mode: /^[a-z]{3,20}$/ }, ttl: (q) => (Number(q.get("page") || 1) <= 1 ? 5 * MIN : 30 * MIN) },
  // Rang et historique des actes
  { re: new RegExp(`^valorant/v3/by-puuid/mmr/${REGION}/pc/${UUID}$`), ttl: () => 3 * MIN },
  // Match complet : ne change plus jamais, gardé sur disque
  { re: new RegExp(`^valorant/v4/match/${REGION}/${UUID}$`), ttl: () => Infinity, disk: true },
];

function route(p, q) {
  const r = ROUTES.find((x) => x.re.test(p));
  if (!r) return null;
  for (const [k, v] of q) if (!r.query?.[k] || !r.query[k].test(v)) return null;
  return r;
}

/* ───────────── Cache mémoire (LRU) + disque pour les matchs ───────────── */

const mem = new Map(); // clé → { status, body: Buffer, exp }
let memBytes = 0;

function memGet(key) {
  const e = mem.get(key);
  if (!e) return null;
  if (e.exp < Date.now()) {
    mem.delete(key);
    memBytes -= e.body.length;
    return null;
  }
  mem.delete(key); // LRU : repasse en fin de liste
  mem.set(key, e);
  return e;
}

function memSet(key, status, body, ttl) {
  const old = mem.get(key);
  if (old) {
    mem.delete(key);
    memBytes -= old.body.length;
  }
  mem.set(key, { status, body, exp: Date.now() + Math.min(ttl, 24 * 60 * MIN) });
  memBytes += body.length;
  for (const [k, e] of mem) {
    if (memBytes <= MEMORY_MB * 1024 * 1024) break;
    mem.delete(k);
    memBytes -= e.body.length;
  }
}

const matchFile = (p) => path.join(DATA, "matches", p.split("/").pop() + ".json.gz");

function diskGet(p) {
  try {
    return zlib.gunzipSync(fs.readFileSync(matchFile(p)));
  } catch {
    return null;
  }
}

function diskSet(p, body) {
  fs.writeFile(matchFile(p), zlib.gzipSync(body), () => {});
}

/** Garde le dossier des matchs sous CACHE_DISK_MB (les plus anciens partent en premier). */
function pruneDisk() {
  const dir = path.join(DATA, "matches");
  let files;
  try {
    files = fs.readdirSync(dir).map((f) => {
      const st = fs.statSync(path.join(dir, f));
      return { f, size: st.size, t: st.atimeMs || st.mtimeMs };
    });
  } catch {
    return;
  }
  let total = files.reduce((a, x) => a + x.size, 0);
  files.sort((a, b) => a.t - b.t);
  for (const x of files) {
    if (total <= DISK_MB * 1024 * 1024) break;
    try {
      fs.unlinkSync(path.join(dir, x.f));
      total -= x.size;
    } catch {}
  }
}

/* ───────────── Quota HenrikDev ───────────── */

const quota = { remaining: Infinity, resetAt: 0 };
let inflightUpstream = 0;
const UPSTREAM_PARALLEL = 4;

/** Attend un créneau (quota de la minute et requêtes simultanées) ; false si l'attente serait trop longue. */
async function slot() {
  const deadline = Date.now() + MAX_WAIT_MS;
  for (;;) {
    const now = Date.now();
    if (quota.resetAt && now >= quota.resetAt) {
      quota.remaining = Infinity;
      quota.resetAt = 0;
    }
    const blocked = quota.remaining <= 1 ? quota.resetAt - now : 0;
    if (blocked <= 0 && inflightUpstream < UPSTREAM_PARALLEL) {
      inflightUpstream++;
      if (Number.isFinite(quota.remaining)) quota.remaining--;
      return true;
    }
    const wait = blocked > 0 ? blocked + 250 : 60;
    if (now + wait > deadline) return false;
    await new Promise((r) => setTimeout(r, Math.min(wait, 1000)));
  }
}

async function upstream(p, q) {
  const url = `${UPSTREAM}/${p}${q.size ? "?" + q : ""}`;
  for (let attempt = 0; attempt < 3; attempt++) {
    if (!(await slot())) return { status: 429, body: Buffer.from('{"error":"busy"}'), retryAfter: Math.ceil((quota.resetAt - Date.now()) / 1000) || 30 };
    let res;
    try {
      res = await fetch(url, { headers: { Authorization: KEY, "User-Agent": "valo-overlay-relay/1.0" }, signal: AbortSignal.timeout(20_000) });
    } catch (e) {
      inflightUpstream--;
      log("amont injoignable", p, e.message);
      return { status: 502, body: Buffer.from('{"error":"upstream"}') };
    }
    inflightUpstream--;
    const h = (k) => Number(res.headers.get(k));
    if (Number.isFinite(h("x-ratelimit-remaining")) && res.headers.has("x-ratelimit-remaining")) {
      quota.remaining = h("x-ratelimit-remaining");
      quota.resetAt = Date.now() + Math.max(1, h("x-ratelimit-reset") || 60) * 1000;
    }
    const body = Buffer.from(await res.arrayBuffer());
    if (res.status === 429) {
      quota.remaining = 0;
      quota.resetAt = Date.now() + Math.max(1, h("retry-after") || h("x-ratelimit-reset") || 60) * 1000;
      continue;
    }
    return { status: res.status, body };
  }
  return { status: 429, body: Buffer.from('{"error":"busy"}'), retryAfter: 30 };
}

/* ───────────── Limite par client ───────────── */

const hits = new Map(); // ip → { n, reset }
function allowed(ip) {
  const now = Date.now();
  let h = hits.get(ip);
  if (!h || h.reset < now) hits.set(ip, (h = { n: 0, reset: now + MIN }));
  return ++h.n <= RATE_PER_MIN;
}

/* ───────────── Serveur HTTP ───────────── */

const pending = new Map(); // requêtes amont en cours (une seule par ressource)
const stats = { requests: 0, hits: 0, upstream: 0, started: Date.now() };

function send(res, status, body, extra = {}) {
  res.writeHead(status, { "content-type": "application/json; charset=utf-8", "cache-control": "no-store", ...extra });
  res.end(body);
}

function tokenOk(req) {
  if (!TOKEN) return true;
  const got = Buffer.from(String(req.headers["x-app-token"] || ""));
  const want = Buffer.from(TOKEN);
  return got.length === want.length && crypto.timingSafeEqual(got, want);
}

async function henrik(p, q, r) {
  const key = p + (q.size ? "?" + q : "");
  const cached = memGet(key);
  if (cached) return { ...cached, cache: "hit" };
  if (r.disk) {
    const body = diskGet(p);
    if (body) {
      memSet(key, 200, body, Infinity);
      return { status: 200, body, cache: "disk" };
    }
  }
  if (!pending.has(key)) {
    pending.set(
      key,
      (async () => {
        stats.upstream++;
        const u = await upstream(p, q);
        if (u.status === 200) {
          memSet(key, 200, u.body, r.ttl(q));
          if (r.disk) diskSet(p, u.body);
        } else if (u.status === 404 || u.status === 400) {
          memSet(key, u.status, u.body, 10 * MIN); // introuvable : pas redemandé tout de suite
        }
        return u;
      })().finally(() => pending.delete(key)),
    );
  }
  return { ...(await pending.get(key)), cache: "miss" };
}

/**
 * Traite une requête du relais. `prefix` : chemin sous lequel il est monté dans un autre serveur
 * web (ex. "/valo" dans le tableau de bord d'un bot Discord). Renvoie false si la requête ne le
 * concerne pas (l'autre serveur la traite alors normalement).
 */
export async function handleRelay(req, res, prefix = "") {
  let url;
  try {
    url = new URL(req.url, "http://relay");
  } catch {
    return false;
  }
  if (prefix && url.pathname !== prefix && !url.pathname.startsWith(prefix + "/")) return false;
  const pathname = url.pathname.slice(prefix.length) || "/";
  if (!pathname.startsWith("/v1/")) return false;
  stats.requests++;
  const ip = String(req.headers["x-forwarded-for"] || req.socket.remoteAddress || "").split(",")[0].trim();
  if (req.method !== "GET" || req.url.length > 400) return send(res, 405, '{"error":"method"}'), true;

  if (pathname === "/v1/health") {
    send(res, 200, JSON.stringify({ ok: !!KEY, uptime: Math.round((Date.now() - stats.started) / 1000), ...stats, cached: mem.size, quota: Number.isFinite(quota.remaining) ? quota.remaining : null }));
    return true;
  }
  if (!KEY) return send(res, 503, '{"error":"no_key"}'), true;
  if (!tokenOk(req)) return send(res, 401, '{"error":"token"}'), true;
  if (!allowed(ip)) return send(res, 429, '{"error":"rate"}', { "retry-after": "30" }), true;

  if (pathname.startsWith("/v1/henrik/")) {
    const p = pathname.slice("/v1/henrik/".length);
    const r = route(p, url.searchParams);
    if (!r) return send(res, 404, '{"error":"route"}'), true;
    try {
      const out = await henrik(p, url.searchParams, r);
      if (out.cache !== "miss") stats.hits++;
      const extra = { "x-cache": out.cache };
      if (out.retryAfter) extra["retry-after"] = String(Math.max(1, out.retryAfter));
      send(res, out.status, out.body, extra);
    } catch (e) {
      log("erreur", p, e);
      send(res, 500, '{"error":"internal"}');
    }
    return true;
  }
  send(res, 404, '{"error":"route"}');
  return true;
}

setInterval(pruneDisk, 60 * MIN).unref();
setInterval(() => {
  const now = Date.now();
  for (const [ip, h] of hits) if (h.reset < now) hits.delete(ip);
}, 5 * MIN).unref();
pruneDisk();

// Lancé seul (node server.mjs) ou avec RELAY_PORT : son propre serveur web. Importé par un autre
// programme : rien n'écoute, l'hôte appelle handleRelay() depuis son propre serveur.
const standalone = !!process.env.RELAY_PORT || !!DOTENV.RELAY_PORT || (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url));
if (standalone) {
  const server = http.createServer(async (req, res) => {
    if (!(await handleRelay(req, res))) send(res, 404, '{"error":"route"}');
  });
  // Une erreur (port déjà pris…) est seulement journalisée.
  server.on("error", (e) => log("arrêté :", e.message));
  server.listen(PORT, HOST, () => log(`relais sur http://${HOST}:${PORT} (données : ${DATA}${TOKEN ? ", jeton requis" : ""})`));
}
if (!KEY) log("HENRIK_KEY manquant (valo-relay.env) : relais inactif");

"use strict";

const TAURI = window.__TAURI__;
const $ = (s) => document.querySelector(s);

const QUEUES = {
  competitive: "Compétition",
  unrated: "Non classé",
  swiftplay: "Vélocité",
  spikerush: "Spike Rush",
  deathmatch: "Combat à mort",
  hurm: "Combat à mort par équipe",
  ggteam: "Escalade",
  onefa: "Réplication",
  premier: "Premier",
  newmap: "Nouvelle carte",
  snowball: "Boules de neige",
};

const PHASES = {
  offline: ["Hors ligne", ""],
  waiting: ["En attente", ""],
  menus: ["Salon", "is-menus"],
  pregame: ["Sélection", "is-pregame"],
  ingame: ["En jeu", "is-ingame"],
};

const PARTY_COLORS = ["#f2cf5b", "#b48cff", "#5ec8ff", "#ff9a4c", "#7cf29a"];
const DEFAULT_BG_AGENT = "add6443a-41bd-e414-f6ad-e58d267f4e95"; // Jett

const app = {
  assets: null,
  snap: null,
  demo: false,
  demoPhase: "ingame",
  timer: 0,
  /** Vue ouverte par-dessus la partie : { kind: "career" | "match", … } ; null = partie en cours */
  view: null,
  /** Vues précédentes (bouton Retour / Échap) */
  stack: [],
  /** Carrières déjà chargées (joueur | acte | mode) */
  careerCache: new Map(),
  careerReq: 0,
  /** Fenêtre affichée (masquée : aucun rendu, juste la mémorisation de l'état) */
  visible: !TAURI,
  /** Partie reçue pendant que la fenêtre était masquée : à redessiner à l'ouverture */
  dirty: false,
  /** Chargements de carrière en cours : requête → fonction qui reçoit sa progression */
  careerApplies: new Map(),
  /** Points des graphiques affichés, pour les infobulles */
  charts: {},
  rankPending: new Set(),
  rankArrived: new Map(),
  enterAt: 0,
};

/* ───────────── Utilitaires ───────────── */

const esc = (s) =>
  String(s ?? "").replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]);

const hex = (rgba) => (rgba && rgba.length >= 6 ? "#" + rgba.slice(0, 6) : null);
const cardUrl = (id, kind = "wideart") => (id ? `https://media.valorant-api.com/playercards/${id}/${kind}.png` : "");

function tier(n) {
  const t = app.assets?.tiers.get(n ?? 0) || app.assets?.tiers.get(0);
  return t || { name: "Non classé", color: "#ece8e1", icon: "" };
}

function agent(id) {
  return id ? app.assets?.agents.get(id.toLowerCase()) : null;
}

function mapById(id) {
  return id ? app.assets?.maps.get(id.toLowerCase()) : null;
}

function queueLabel(s) {
  if (s.provisioningFlow === "CustomGame") return "Partie personnalisée";
  return QUEUES[s.queueId] || s.queueId || "";
}

function timeAgo(ms) {
  if (!ms) return "";
  const min = Math.round((Date.now() - ms) / 60000);
  if (min < 60) return `il y a ${Math.max(1, min)} min`;
  const h = Math.round(min / 60);
  if (h < 24) return `il y a ${h} h`;
  const d = Math.round(h / 24);
  return d === 1 ? "hier" : `il y a ${d} j`;
}

function currentSnap() {
  return app.demo ? window.DEMO?.snapshot(app.assets, app.demoPhase) : app.snap;
}

/* ───────────── Assets ───────────── */

async function loadAssets() {
  let raw;
  try {
    raw = TAURI ? await TAURI.core.invoke("get_assets") : await fetch("dev-assets.json").then((r) => r.json());
  } catch (e) {
    console.warn("assets indisponibles", e);
    raw = { agents: [], tiers: [], maps: [], cards: [] };
  }
  const A = { agents: new Map(), tiers: new Map(), maps: new Map(), cards: raw.cards || [] };
  A.borders = (raw.levelBorders || []).slice().sort((x, y) => x.startingLevel - y.startingLevel);
  for (const a of raw.agents || []) {
    const g = a.backgroundGradientColors || [];
    A.agents.set(a.uuid.toLowerCase(), {
      id: a.uuid,
      name: a.displayName,
      icon: a.displayIcon,
      portrait: a.fullPortrait,
      role: a.role?.displayName || "",
      roleIcon: a.role?.displayIcon || "",
      c1: hex(g[0]),
      c2: hex(g[2] || g[1]),
    });
  }
  for (const t of raw.tiers || []) {
    A.tiers.set(t.tier, { name: t.tierName, color: hex(t.color) || "#ece8e1", icon: t.largeIcon || t.smallIcon });
  }
  for (const m of raw.maps || []) {
    const map = { name: m.displayName, splash: m.splash, strip: m.listViewIcon };
    if (m.mapUrl) A.maps.set(m.mapUrl.toLowerCase(), map);
    if (m.uuid) A.maps.set(m.uuid.toLowerCase(), map);
  }
  return A;
}

/* ───────────── Blocs ───────────── */

function rankBlock(r) {
  if (!r) {
    return `<div class="rank"><div class="sk sk-ico"></div><div class="t"><div class="sk sk-line"></div><div class="sk sk-line"></div></div></div>`;
  }
  if (r.error) {
    const t = tier(0);
    return `<div class="rank"><img src="${esc(t.icon)}" alt=""><div class="t"><div class="tn" style="--rc:var(--ink-3)">Inconnu</div><div class="note">Rang indisponible</div></div></div>`;
  }
  const t = tier(r.tier);
  let detail;
  if (r.tier >= 24 || (r.tier > 0 && r.leaderboard)) {
    detail = `<div class="rr"><span>${r.rr} RR${r.leaderboard ? ` · #${r.leaderboard}` : ""}</span></div>`;
  } else if (r.tier > 0) {
    detail = `<div class="rr"><div class="rr-bar"><i style="width:${Math.min(100, r.rr)}%"></i></div><span>${r.rr} RR</span></div>`;
  } else if (r.prevTier > 0) {
    detail = `<div class="note">Dernier : ${esc(tier(r.prevTier).name)}</div>`;
  } else {
    detail = `<div class="note">${r.games ? `${r.games} placement${r.games > 1 ? "s" : ""}` : "Aucune partie"}</div>`;
  }
  return `<div class="rank" style="--rc:${t.color}"><img src="${esc(t.icon)}" alt=""><div class="t"><div class="tn">${esc(t.name)}</div>${detail}</div></div>`;
}

function peakBlock(r) {
  if (!r) return `<div class="peak"><div class="sk sk-ico sm"></div><div style="flex:1"><div class="sk sk-line"></div></div></div>`;
  if (!r.peakTier) return `<div class="peak"><div><span class="lbl">Pic</span><b style="color:var(--ink-3)">—</b></div></div>`;
  const t = tier(r.peakTier);
  return `<div class="peak"><img src="${esc(t.icon)}" alt=""><div><span class="lbl">Pic</span><b style="color:${t.color}">${esc(t.name)}</b>${r.peakAct ? `<em>${esc(r.peakAct)}</em>` : ""}</div></div>`;
}

function wrBlock(r) {
  if (!r) return `<div class="wr"><div class="sk sk-line" style="margin-left:auto;width:80%"></div></div>`;
  if (!r.games) return `<div class="wr"><span class="lbl">Winrate</span><b style="color:var(--ink-3)">—</b></div>`;
  const pct = Math.round((r.wins / r.games) * 100);
  const cls = pct >= 55 ? "good" : pct < 45 ? "bad" : "";
  return `<div class="wr"><span class="lbl">Winrate</span><b class="${cls}">${pct}%</b><em>${r.games} partie${r.games > 1 ? "s" : ""}</em></div>`;
}

function nameBlock(p) {
  const a = agent(p.agentId);
  const dot = p.party != null ? `<i class="pdot" style="--pc:${PARTY_COLORS[p.party % PARTY_COLORS.length]}" title="${p.partyGuess ? "Groupe probable (ensemble à leur dernier match)" : "En groupe"}"></i>` : "";
  const name = p.name
    ? `<span class="n">${esc(p.name)}</span><span class="tag">#${esc(p.tag)}</span>`
    : `<span class="n hidden">${esc(a?.name || "Joueur")}</span><span class="tag">${p.incognito || hiddenLive(p.puuid) ? "masqué" : "anonyme"}</span>`;
  return `<div class="name">${dot}${name}</div>`;
}

/** Niveau de compte dans son cadre officiel (un cadre tous les 20 niveaux). */
function levelBadge(level) {
  const border = app.assets?.borders?.filter((b) => b.startingLevel <= level).pop();
  if (!border) return `<span class="lvl">NV ${level}</span>`;
  return `<span class="lvlb" style="background-image:url('${esc(border.levelNumberAppearance)}')" title="Niveau ${level}">${level}</span>`;
}

function subBlock(p) {
  const a = agent(p.agentId);
  const bits = [];
  if (p.level != null) bits.push(levelBadge(p.level));
  if (a && p.name) bits.push(`<span>${esc(a.name)}</span>`);
  return `<div class="sub">${bits.join("")}</div>`;
}

function agentCell(p) {
  const a = agent(p.agentId);
  const style = a ? `--a1:${a.c1 || "#26394a"};--a2:${a.c2 || "#0f1923"}` : "";
  const inner = a ? `<img src="${esc(a.icon)}" alt="">` : `<span class="q">?</span>`;
  return `<div class="agent" style="${style}">${inner}</div>`;
}

/** Âge (ms) de l'arrivée du rang d'un joueur, s'il vient de passer de « chargement » à « chargé ». */
function rankFresh(p) {
  const now = performance.now();
  if (!p.rank) {
    app.rankPending.add(p.puuid);
    return null;
  }
  if (app.rankPending.delete(p.puuid)) app.rankArrived.set(p.puuid, now);
  const at = app.rankArrived.get(p.puuid);
  return at != null && now - at < 1000 ? Math.round(now - at) : null;
}

function playerRow(p, i, { pregame = false } = {}) {
  const cls = ["p"];
  if (p.isMe) cls.push("me");
  const fresh = rankFresh(p);
  if (fresh != null) cls.push("fresh");
  let state = "";
  if (pregame) {
    if (p.agentState === "locked") state = `<span class="state lock">Verrouillé</span>`;
    else if (p.agentState === "selected") { cls.push("hovering"); state = `<span class="state lock soft">En sélection</span>`; }
    else { cls.push("picking"); state = `<span class="state" style="color:var(--ink-3)">Choix…</span>`; }
  }
  const art = p.cardId ? `<div class="art" style="background-image:url('${cardUrl(p.cardId)}')"></div>` : "";
  const cells = [art, agentCell(p), `<div class="who">${nameBlock(p)}${subBlock(p)}</div>`];
  if (pregame) cells.push(`<div>${state}</div>`);
  cells.push(peakBlock(p.rank), rankBlock(p.rank), wrBlock(p.rank));
  const ra = fresh != null ? `;--ra:-${fresh}ms` : "";
  return `<div class="${cls.join(" ")}" data-puuid="${esc(p.puuid)}" style="--i:${i}${ra}">${cells.join("")}</div>`;
}

function playerCard(p, i, clickable = true) {
  const r = p.rank;
  return `
    <div class="pcard ${p.isMe ? "me" : ""}" ${clickable ? `data-puuid="${esc(p.puuid)}"` : ""} style="--i:${i};background-image:url('${cardUrl(p.cardId, "largeart")}')">
      <div class="inner">
        ${nameBlock({ ...p, party: null })}
        ${subBlock(p)}
        ${rankBlock(r)}
        <div class="row2">${peakBlock(r)}${wrBlock(r)}</div>
      </div>
    </div>`;
}

function teamAvg(players) {
  const ranked = players.filter((p) => p.rank && p.rank.tier > 0);
  if (!ranked.length) return "";
  const avg = Math.round(ranked.reduce((s, p) => s + p.rank.tier, 0) / ranked.length);
  const t = tier(avg);
  return `<div class="team-avg"><div><small>Moyenne</small><b style="color:${t.color}">${esc(t.name)}</b></div><img src="${esc(t.icon)}" alt=""></div>`;
}

/* ───────────── Groupes ───────────── */

/** Place les membres d'un même groupe les uns sous les autres (à la position du premier). */
function groupOrder(list) {
  const first = new Map();
  list.forEach((p, i) => {
    if (p.party != null && !first.has(p.party)) first.set(p.party, i);
  });
  return list
    .map((p, i) => ({ p, i, k: p.party != null ? first.get(p.party) : i }))
    .sort((a, b) => a.k - b.k || a.i - b.i)
    .map((x) => x.p);
}

/** Accolade de couleur reliant les lignes consécutives d'un même groupe. */
function partyBrackets(list, offset) {
  let html = "";
  for (let i = 0; i < list.length; ) {
    const g = list[i].party;
    let j = i;
    while (g != null && j + 1 < list.length && list[j + 1].party === g) j++;
    if (g != null && j > i) {
      const c = PARTY_COLORS[g % PARTY_COLORS.length];
      const guess = list.slice(i, j + 1).some((p) => p.partyGuess);
      const title = guess ? `Groupe probable de ${j - i + 1} : ensemble à leur dernier match` : `Groupe de ${j - i + 1}`;
      html += `<i class="bracket" style="--s:${i};--n:${j - i + 1};--pc:${c};--i:${offset + i}" title="${title}"><b></b></i>`;
    }
    i = j + 1;
  }
  return html;
}

function playerList(list, offset, opts) {
  const ordered = groupOrder(list);
  return `<div class="plist">${partyBrackets(ordered, offset)}${ordered.map((p, i) => playerRow(p, i + offset, opts)).join("")}</div>`;
}

/* ───────────── Vues ───────────── */

function viewEmpty(title, text) {
  const logo = $(".brand .logo").outerHTML;
  return `<div class="empty"><div>${logo}<h2>${esc(title)}</h2><p>${esc(text)}</p></div></div>`;
}

function viewIngame(s) {
  const team = (list, enemy, offset, title) => `
    <div class="team ${enemy ? "enemy" : ""}">
      <div class="team-head">
        <div class="team-title"><small>${enemy ? "Adversaires" : "Alliés"}</small><span>${title}</span></div>
        ${teamAvg(list)}
      </div>
      ${playerList(list, offset)}
    </div>`;
  const allies = s.players.filter((p) => p.isAlly);
  const enemies = s.players.filter((p) => !p.isAlly);
  if (!enemies.length) {
    // Modes sans équipes (combat à mort) : deux colonnes de joueurs.
    const half = Math.ceil(s.players.length / 2);
    return `<div class="teams">${team(s.players.slice(0, half), false, 0, "Joueurs")}${team(s.players.slice(half), false, half, "Joueurs")}</div>`;
  }
  return `<div class="teams">${team(allies, false, 0, "Ton équipe")}<div class="vs"><span>VS</span></div>${team(enemies, true, 5, "Équipe adverse")}</div>`;
}

function viewPregame(s) {
  const m = mapById(s.mapId);
  return `
    <div class="pregame">
      <div class="team">
        <div class="team-head">
          <div class="team-title"><small>Sélection des agents</small><span>Ton équipe</span></div>
          ${teamAvg(s.players)}
        </div>
        ${playerList(s.players, 0, { pregame: true })}
      </div>
      <div class="map-card" style="--i:6;background-image:url('${esc(m?.splash || "")}')">
        <div class="inner">
          <small>${esc(queueLabel(s) || "Partie")}</small>
          <h2>${esc(m?.name || "Carte")}</h2>
          <div class="timer"><span>Temps restant</span><b id="timer">--</b></div>
        </div>
      </div>
    </div>`;
}

function viewMenus(s) {
  const solo = s.players.length <= 1;
  return `
    <div class="lobby-head">
      <div class="team-title"><small>${esc(queueLabel(s) || "Salon")}</small><span>${solo ? "Ton profil" : "Ton groupe"}</span></div>
      ${solo ? `<div class="hint-txt">Les rangs de ta partie s'afficheront dès la sélection des agents.</div>` : ""}
    </div>
    <div class="cards">${s.players.map((p, i) => playerCard(p, i)).join("")}</div>
    ${lastResultCard()}`;
}

/* ───────────── Carrière & détail de match ─────────────
   Navigation en pile : partie en cours → carrière → match → carrière d'un autre joueur…
   « Retour » / Échap dépile. app.view = vue affichée (null = partie en cours). */

const fmt = (n, d = 0) => (Number.isFinite(n) ? n.toFixed(d) : "0");
const signed = (n, d = 0) => (n > 0 ? "+" : "") + fmt(n, d);

/** Nombre animé (compteur) : la valeur finale est déjà écrite, l'animation part de 0. */
function num(value, { d = 0, suffix = "", sign = false, cls = "" } = {}) {
  const text = (sign ? signed(value, d) : fmt(value, d)) + suffix;
  return `<b class="${cls}" data-n="${value}" data-d="${d}" data-s="${esc(suffix)}"${sign ? ' data-sign="1"' : ""}>${text}</b>`;
}

const good = (v, hi, lo) => (v >= hi ? "good" : v < lo ? "bad" : "");
const BACK_BTN = `<button class="back" data-act="back"><svg viewBox="0 0 24 24"><path d="M15 5l-7 7 7 7"/></svg>Retour</button>`;

function duration(ms) {
  const min = Math.round((ms || 0) / 60000);
  return min ? `${min} min` : "";
}

/** Pseudo masqué si le joueur est en incognito dans la partie en cours (sauf toi / ton groupe). */
function hiddenLive(puuid) {
  const live = app.snap?.players.find((p) => p.puuid === puuid);
  return !!(live && live.incognito && !live.name);
}

function viewCareer(c) {
  const bar = `
    <div class="career-bar">
      ${BACK_BTN}
      <div class="seg" title="Matchs pris en compte dans les statistiques (Tous les modes : modes à manches, hors match à mort, Escalade et parties personnalisées)">
        <button data-queue="comp" class="${c.competitive ? "on" : ""}">Compétition</button>
        <button data-queue="all" class="${c.competitive ? "" : "on"}">Tous les modes</button>
      </div>
    </div>`;
  return `${bar}<div class="career">${careerCard(c)}<div class="c-right"><div id="season">${careerSeason(c)}</div><div id="perf">${perfHtml(c)}</div></div></div>`;
}

/** Rang affiché : détail chargé pour la carrière, sinon celui de la partie en cours, sinon le dernier match classé. */
function careerRank(c) {
  if (c.detail) return c.detail;
  const r = c.player.rank;
  if (r && !r.error) return r;
  if (c.data?.currentTier != null) {
    return { tier: c.data.currentTier, rr: c.data.currentRr || 0, wins: 0, games: 0, prevTier: 0, peakTier: 0, actPeakTier: 0 };
  }
  return r || null;
}

function careerCard(c) {
  const p = c.player;
  return `
    <div class="pcard c-card ${p.isMe ? "me" : ""}" style="background-image:url('${cardUrl(p.cardId, "largeart")}')">
      <div class="inner">
        ${nameBlock({ ...p, party: null })}
        ${subBlock(p)}
        <span class="lbl">Rang actuel</span>
        ${rankBlock(careerRank(c))}
      </div>
    </div>`;
}

/** "V26 · ACTE V" → "V26 A5" ; "E9 · ACTE III" → "E9 A3" */
function shortAct(name) {
  if (!name) return "—";
  const roman = { I: 1, II: 2, III: 3, IV: 4, V: 5, VI: 6 };
  const [ep, act] = name.split("·").map((x) => x.trim());
  const n = act?.replace(/^ACTE\s+/i, "");
  return act ? `${ep} A${roman[n] || n}` : name;
}

function tierCell(label, t, extra = "", tip = "") {
  const tt = tier(t);
  return `<div class="t-cell" style="--rc:${tt.color}" ${tip ? `title="${esc(tip)}"` : ""}><img class="pop" src="${esc(tt.icon)}" alt=""><div><span class="lbl">${label}</span><b>${esc(tt.name)}</b>${extra}</div></div>`;
}

/** Acte affiché : null = acte en cours. */
function selectedAct(c) {
  return c.actId ? c.detail?.history.find((a) => a.id === c.actId) : c.detail?.history.find((a) => a.current);
}

/** Données officielles Riot : l'acte affiché (bilan) et l'historique classé cliquable. */
function careerSeason(c) {
  const r = careerRank(c);
  const d = c.detail;
  const sel = c.actId ? selectedAct(c) : null;
  const stat = (label, v, tip) => `<div title="${esc(tip)}"><span class="lbl">${label}</span><b>${v}</b></div>`;

  let title, cur;
  if (sel) {
    // Acte passé : bilan de fin d'acte
    const t = tier(sel.tier);
    const wr = sel.games ? Math.round((sel.wins / sel.games) * 100) : null;
    title = `<h3 class="sec"><span>Acte sélectionné</span><em>${esc(sel.name || "")}</em><button class="link" data-act="cur-act">Revenir à l'acte en cours</button></h3>`;
    cur = `
      <div class="s-main" style="--rc:${t.color}">
        <img class="pop" src="${esc(t.icon)}" alt="">
        <div class="s-rank"><b>${esc(t.name)}</b><em>Rang de fin d'acte</em></div>
      </div>
      <div class="s-stats">
        ${stat("Parties", sel.games, "Parties classées jouées pendant cet acte")}
        ${stat("Victoires", sel.wins, "Victoires pendant cet acte")}
        ${stat("Winrate", wr == null ? "—" : `<span class="${good(wr, 55, 45)}">${wr}%</span>`, "Pourcentage de victoires pendant cet acte")}
        ${tierCell("Pic de l'acte", sel.peak, "", "Meilleur rang atteint pendant cet acte")}
      </div>`;
  } else {
    const actName = d?.currentActName || app.snap?.actName || "";
    title = `<h3 class="sec"><span>Acte en cours</span>${actName ? `<em>${esc(actName)}</em>` : ""}<i class="src">Données officielles Riot</i></h3>`;
    if (!r) {
      cur = `<div class="sk" style="height:12rem"></div>`;
    } else {
      const t = tier(r.tier);
      const wr = r.games ? Math.round((r.wins / r.games) * 100) : null;
      let status;
      if (r.tier > 0) status = r.tier >= 24 ? `${r.rr} RR${r.leaderboard ? ` · #${r.leaderboard} au classement` : ""}` : `${r.rr} / 100 RR`;
      else if (r.games > 0) status = `Placements en cours (${Math.min(r.games, 5)} / 5)`;
      else status = r.prevTier ? `Pas encore joué cet acte · dernier rang ${tier(r.prevTier).name}` : "Pas encore joué cet acte";
      cur = `
        <div class="s-main" style="--rc:${t.color}">
          <img class="pop" src="${esc(t.icon)}" alt="">
          <div class="s-rank">
            <b>${esc(t.name)}</b>
            ${r.tier > 0 && r.tier < 24 ? `<i class="s-rr"><i style="--f:${Math.min(1, r.rr / 100)}"></i></i>` : ""}
            <em>${esc(status)}</em>
          </div>
        </div>
        <div class="s-stats">
          ${stat("Parties", r.games || 0, "Parties classées jouées cet acte")}
          ${stat("Victoires", r.wins || 0, "Victoires cet acte")}
          ${stat("Winrate", wr == null ? "—" : `<span class="${good(wr, 55, 45)}">${wr}%</span>`, "Pourcentage de victoires cet acte")}
          ${r.actPeakTier || r.tier
            ? tierCell("Pic de l'acte", r.actPeakTier || r.tier, "", "Meilleur rang atteint pendant cet acte")
            : stat("Pic de l'acte", "<span class='dim'>—</span>", "Aucun rang obtenu cet acte")}
        </div>`;
    }
  }

  // Historique classé : chaque acte est cliquable
  let hist;
  if (!d) {
    hist = c.detailError ? `<div class="s-empty">Historique indisponible</div>` : `<div class="sk" style="height:9rem"></div>`;
  } else if (!d.history.length) {
    hist = `<div class="s-empty">Aucune partie classée</div>`;
  } else {
    const acts = d.history.slice(-8);
    const bestTier = Math.max(...d.history.map((a) => a.peak));
    const shown = c.actId || d.history.find((a) => a.current)?.id;
    // Frise : chaque acte posé sur une ligne de progression, flèche de tendance d'un acte à l'autre
    const items = acts
      .map((a, i) => {
        const t = tier(a.tier);
        const wr = a.games ? Math.round((a.wins / a.games) * 100) : 0;
        const prev = acts[i - 1];
        const trend = prev && a.tier && prev.tier ? Math.sign(a.tier - prev.tier) : 0;
        const arrow = trend > 0 ? `<i class="a-trend up" title="En hausse">▲</i>` : trend < 0 ? `<i class="a-trend down" title="En baisse">▼</i>` : "";
        const cls = [a.current ? "cur" : "", a.peak === bestTier ? "best" : "", a.id === shown ? "sel" : ""].join(" ");
        const tip = `${a.name || "Acte"} · fin : ${t.name} · pic : ${tier(a.peak).name} · ${a.wins} V / ${a.games} parties (${wr} %) — cliquer pour voir les stats de l'acte`;
        return `<button class="act ${cls}" data-act-id="${esc(a.id)}" style="--i:${i};--rc:${t.color}" title="${esc(tip)}">
          <span class="a-icon"><i class="a-halo"></i><img src="${esc(t.icon)}" alt="">${a.peak === bestTier ? `<i class="a-peak">PIC</i>` : ""}</span>
          <i class="a-dot"></i>
          <b>${esc(shortAct(a.name))}${arrow}</b>
          <em>${a.current ? "En cours" : `${a.games} P · ${wr}%`}</em>
        </button>`;
      })
      .join("");
    const totalWr = d.totalGames ? Math.round((d.totalWins / d.totalGames) * 100) : 0;
    hist = `
      <div class="acts" style="--n:${acts.length}"><i class="a-track"></i>${items}</div>
      <div class="s-foot">
        ${tierCell("Pic global", d.peakTier || 0, d.peakAct ? `<em>${esc(d.peakAct)}</em>` : "", "Meilleur rang jamais atteint")}
        <div title="Toutes les parties classées, tous actes confondus"><span class="lbl">Total classé</span><b>${d.totalGames} parties · <span class="${good(totalWr, 55, 45)}">${totalWr}%</span></b></div>
      </div>`;
  }

  return `
    <div class="season">
      <section class="s-cur">${title}${cur}</section>
      <section class="s-hist">
        <h3 class="sec"><span>Historique classé</span><em>clique sur un acte pour voir ses stats</em></h3>
        ${hist}
      </section>
    </div>`;
}

/** Zone des statistiques (mise à jour seule pendant le chargement, sans recharger la page). */
function perfHtml(c) {
  const d = c.data;
  if (c.error) return `<div class="c-msg">Matchs indisponibles<br><small style="color:var(--ink-3)">${esc(c.error)}</small></div>`;
  if (!d) return perfSkeleton(c);
  // Stats principales dès que la liste des matchs est complète (elles ne bougent plus ensuite) ;
  // les stats détaillées apparaissent d'un coup à la fin de l'analyse.
  if (!d.done && !d.mainReady) return careerPerfHead(c, d) + perfSkeleton(c, false);
  if (!d.matches.length) {
    if (d.historyLimited) return officialOnly(c, d);
    return careerPerfHead(c, d) + `<div class="c-msg">Aucun match${c.competitive ? " classé" : ""} sur cette période</div>`;
  }
  return careerPerfHead(c, d) + careerBigs(d) + careerGrid(d) + careerCharts(d) + careerTabs(c, d) +
    `<div class="tab-body" data-tabbody>${careerTabBody(c)}</div>`;
}

const longDate = (ms) => new Date(ms).toLocaleDateString("fr-FR", { day: "numeric", month: "long", year: "numeric" });

/** Acte dont Riot ne garde plus les matchs : explication + bilan officiel (parties, victoires par rang). */
function officialOnly(c, d) {
  const act = selectedAct(c);
  const period = d.actName ? `${d.actCurrent ? "Acte en cours" : "Acte"} · ${d.actName}` : "";
  const who = c.player.isMe ? "de ton compte" : "de ce joueur";
  const since = d.historyOldest ? ` remonte au ${longDate(d.historyOldest)}` : " ne remonte pas jusqu'à cet acte";
  const notice = `
    <div class="notice">
      <svg viewBox="0 0 24 24"><circle cx="12" cy="12" r="10"/><path d="M12 11v6M12 7.5v.5"/></svg>
      <div>
        <b>Détail des matchs indisponible pour cet acte</b>
        <p>Riot ne fournit que les ~100 derniers matchs de chaque joueur (tous modes confondus) : l'historique ${who}${since}. Pour cet acte, seul le bilan officiel est disponible.</p>
        ${c.player.isMe
          ? `<p class="ok">L'overlay archive tes matchs automatiquement (toutes les 20 min) : tes prochains actes seront complets, tant qu'il tourne au moins une fois tous les ~100 matchs.</p>`
          : `<p class="ok">Chaque consultation archive ses derniers matchs sur ton PC : en revenant régulièrement sur ce joueur, ses actes se complètent.</p>`}
      </div>
    </div>`;
  let body = "";
  if (act) {
    const losses = Math.max(0, act.games - act.wins);
    const wr = act.games ? (act.wins / act.games) * 100 : 0;
    const big = (i, label, html, fill, tip) =>
      `<div class="big" style="--i:${i + 2};--f:${Math.max(0.02, Math.min(1, fill))}" title="${esc(tip)}"><span class="lbl">${label}</span>${html}<i class="meter"><i></i></i></div>`;
    const maxGames = Math.max(act.games, 1);
    const wbt = [...(act.winsByTier || [])].reverse();
    const maxW = Math.max(1, ...wbt.map(([, w]) => w));
    const rows = wbt
      .map(([t, w], i) => {
        const tt = tier(t);
        return `<div class="wbt-row" style="--rc:${tt.color};--i:${i + 8}"><img src="${esc(tt.icon)}" alt=""><span>${esc(tt.name)}</span><i class="wbt-bar"><i style="--f:${(w / maxW).toFixed(3)}"></i></i><b>${w}</b></div>`;
      })
      .join("");
    body = `
      <div class="bigs">
        ${big(0, "Parties", num(act.games), 1, "Parties classées jouées pendant l'acte")}
        ${big(1, "Victoires", num(act.wins, { cls: "good" }), act.wins / maxGames, "Victoires pendant l'acte")}
        ${big(2, "Défaites", num(losses, { cls: "bad" }), losses / maxGames, "Défaites et égalités pendant l'acte")}
        ${big(3, "Winrate", num(wr, { d: 1, suffix: "%", cls: good(wr, 55, 45) }), wr / 100, "Pourcentage de victoires pendant l'acte")}
      </div>
      ${rows ? `<div class="chart-card wide wbt"><div class="cc-head"><span class="lbl">Victoires par rang pendant l'acte</span><b>${act.wins} <small>victoires</small></b></div>${rows}</div>` : ""}`;
  }
  return `
    <div class="perf-head">
      <div class="perf-title"><h3 class="sec"><span>Statistiques</span><em>${esc(period)}</em><i class="src">Bilan officiel Riot</i></h3></div>
    </div>
    ${notice}${body}`;
}

function perfSkeleton(c, head = true) {
  const sk = (n, cls) => Array.from({ length: n }, () => `<div class="sk ${cls}"></div>`).join("");
  return `${head ? `<div class="sk sk-perfhead"></div>` : ""}<div class="bigs">${sk(4, "sk-big")}</div><div class="sk sk-grid"></div><div class="charts">${sk(2, "sk-chart")}</div>`;
}

/** Titre de la section Performance : période exacte, mode, progression de l'analyse. */
function careerPerfHead(c, d) {
  const s = d.summary;
  const period = d.actName ? `${d.actCurrent ? "Acte en cours" : "Acte"} · ${d.actName}` : "Derniers matchs";
  const mode = c.competitive ? "Compétition" : "Tous les modes";
  const act = selectedAct(c);
  let info;
  if (!d.done && !d.mainReady) {
    return `
    <div class="perf-head">
      <div class="perf-title">
        <h3 class="sec"><span>Statistiques</span><em>${esc(period)}</em><i class="src">${esc(mode)}</i></h3>
        ${progressHtml(d)}
      </div>
    </div>`;
  } else {
    const expected = c.competitive && act ? act.games : null;
    const plural = (n, w) => `${n} ${w}${n > 1 ? "s" : ""}`;
    let count = expected && s.matches < expected
      ? `${s.matches} partie${s.matches > 1 ? "s" : ""} analysée${s.matches > 1 ? "s" : ""} sur ${expected} dans l'acte (données officielles), ${s.rounds} manches`
      : `${plural(s.matches, "match")} analysé${s.matches > 1 ? "s" : ""}, ${s.rounds} manches`;
    // Détail pas encore téléchargé pour tous les matchs (il continue en arrière-plan)
    const adv = s.advancedMatches ?? s.matches;
    const note = d.done && adv > 0 && adv < s.matches ? ` · stats détaillées sur les ${adv} plus récents` : "";
    if (expected && s.matches >= expected) count += " · acte complet";
    const modes = c.competitive ? "" : " · hors match à mort, Escalade et parties personnalisées";
    info = `<p>${esc(count + modes + note)}</p>`;
  }
  const C = 2 * Math.PI * 26;
  const winLen = s.matches ? (s.wins / s.matches) * C : 0;
  return `
    <div class="perf-head">
      <div class="perf-title">
        <h3 class="sec"><span>Statistiques</span><em>${esc(period)}</em><i class="src">${esc(mode)}</i></h3>
        ${info}
      </div>
      <div class="h-wl" title="Victoires / défaites">
        <svg viewBox="0 0 64 64" class="ring"><circle cx="32" cy="32" r="26" class="ring-bg"/><circle cx="32" cy="32" r="26" class="ring-fg" style="--len:${winLen}px" stroke-dasharray="${winLen} ${C}"/></svg>
        <div class="wl"><b class="good">${s.wins} V</b><b class="bad">${s.losses} D</b></div>
      </div>
      <div class="h-stat" title="Pourcentage de manches gagnées"><span class="lbl">Manches gagnées</span>${num(s.roundWin, { d: 1, suffix: "%", cls: good(s.roundWin, 52, 48) })}</div>
      <div class="h-stat" title="RR gagnés ou perdus sur la période"><span class="lbl">RR gagnés</span>${s.rrNet ? num(s.rrNet, { sign: true, cls: s.rrNet > 0 ? "good" : "bad" }) : "<b class='dim'>—</b>"}</div>
    </div>`;
}

/** Barre d'analyse : matchs prêts / matchs de la période. */
function progressHtml(d) {
  const f = d.found ? d.analyzed / d.found : 0;
  const label = d.found ? `Analyse des matchs · ${d.analyzed} / ${d.found}` : "Recherche des matchs";
  return `<div class="progress"><span id="progLabel">${label}</span><i><i id="progBar" style="--f:${Math.max(0.04, f).toFixed(3)}"></i></i></div>`;
}

const SK_VAL = `<b class="sk sk-val"></b>`;

function careerBigs(d) {
  const s = d.summary;
  const wr = s.matches ? (s.wins / s.matches) * 100 : 0;
  const big = (i, label, html, fill, tip) =>
    `<div class="big" style="--i:${i + 2};--f:${Math.max(0.02, Math.min(1, fill))}" title="${esc(tip)}"><span class="lbl">${label}</span>${html}<i class="meter"><i></i></i></div>`;
  return `<div class="bigs">
    ${big(0, "Dégâts / manche", d.done ? num(s.adr, { d: 1 }) : SK_VAL, d.done ? s.adr / 250 : 0, "ADR : dégâts infligés en moyenne par manche")}
    ${big(1, "K/D", num(s.kd, { d: 2, cls: good(s.kd, 1.1, 0.9) }), s.kd / 2, "Kills divisés par morts")}
    ${big(2, "Headshot", d.done ? num(s.hs, { d: 1, suffix: "%" }) : SK_VAL, d.done ? s.hs / 40 : 0, "Part des balles touchées à la tête")}
    ${big(3, "Winrate", num(wr, { d: 1, suffix: "%", cls: good(wr, 55, 45) }), wr / 100, "Pourcentage de matchs gagnés")}
  </div>`;
}

function careerGrid(d) {
  const s = d.summary;
  const cells = [
    ["KAST", num(s.kast, { d: 1, suffix: "%" }), "Manches avec un kill, une assist, une survie ou une mort vengée"],
    ["DDΔ / manche", num(s.ddelta, { sign: true, cls: s.ddelta >= 0 ? "good" : "bad" }), "Dégâts infligés moins dégâts reçus, par manche"],
    ["Kills", num(s.kills), "Total des kills"],
    ["Morts", num(s.deaths), "Total des morts"],
    ["Assists", num(s.assists), "Total des assists"],
    ["ACS", num(s.acs, { d: 1 }), "Score de combat moyen par manche"],
    ["KAD", num(s.kad, { d: 2 }), "(Kills + assists) divisés par morts"],
    ["Kills / manche", num(s.kpr, { d: 2 }), "Kills en moyenne par manche"],
    ["First bloods", num(s.firstBloods, { cls: "good" }), "Premier kill de la manche"],
    ["First deaths", num(s.firstDeaths, { cls: "bad" }), "Premier mort de la manche"],
    ["Multi-kills", num(s.multikills), "Manches avec au moins 3 ennemis différents tués"],
    ["Aces", num(s.aces), "Manches où le joueur a tué les 5 ennemis (reconnues par Riot)"],
    ["Clutchs", num(s.clutches || 0), "Manches gagnées en clutch (reconnues par Riot)"],
    ["Econ rating", num(s.econ || 0), "Dégâts infligés pour 1 000 crédits dépensés"],
    ["Manches flawless", num(s.flawless), "Manches gagnées sans perdre un coéquipier"],
    ["MVP", num(s.mvps, { cls: s.mvps ? "gold" : "" }), "Meilleur score de combat du match"],
  ];
  // KAST et suivantes viennent du détail des matchs : en attente tant qu'il manque des matchs
  const ADVANCED = new Set(["KAST", "DDΔ / manche", "First bloods", "First deaths", "Multi-kills", "Aces", "Clutchs", "Econ rating", "Manches flawless", "MVP"]);
  const adv = s.advancedMatches ?? s.matches;
  const pending = !d.done && adv < s.matches;
  const none = d.done && adv === 0 && s.matches > 0;
  const note = d.done && adv > 0 && adv < s.matches ? ` (sur les ${adv} matchs les plus récents)` : "";
  return `<div class="grid">${cells
    .map(([l, v, tip], i) => {
      const advanced = ADVANCED.has(l);
      const val = pending && advanced ? `<b class="sk sk-val"></b>` : none && advanced ? `<b class="num dim">—</b>` : v;
      const title = advanced ? (none ? `${tip} : détail des matchs indisponible` : tip + note) : tip;
      return `<div class="cell" style="--i:${i + 6}" title="${esc(title)}"><span class="lbl">${l}</span>${val}</div>`;
    })
    .join("")}</div>`;
}

/* ── Graphiques (SVG + points HTML, infobulle au survol) ── */

const shortDate = (ms) => (ms ? new Date(ms).toLocaleDateString("fr-FR", { day: "numeric", month: "short" }) : "");

function matchTip(m) {
  const map = mapById(m.mapId);
  const res = m.won === true ? `<b class="good">Victoire</b>` : m.won === false ? `<b class="bad">Défaite</b>` : "<b>Nul</b>";
  const rr = m.rrChange != null ? ` · <b class="${m.rrChange >= 0 ? "good" : "bad"}">${signed(m.rrChange)} RR</b>` : "";
  return `<em>${esc(shortDate(m.startMs))} · ${esc(map?.name || "")}</em>${res} ${m.scoreUs}–${m.scoreThem}${rr}<br>${m.acs} ACS · ${m.kills} / ${m.deaths} / ${m.assists} · ${m.hs}% HS`;
}

/** Axe Y « propre » : bornes arrondies et 3 à 5 graduations. */
function niceTicks(min, max, count = 4) {
  const span = Math.max(1, max - min);
  const raw = span / count;
  const pow = 10 ** Math.floor(Math.log10(raw));
  const step = [1, 2, 2.5, 5, 10].map((k) => k * pow).find((s) => s >= raw) || raw;
  const lo = Math.floor(min / step) * step;
  const hi = Math.ceil(max / step) * step;
  const ticks = [];
  for (let v = lo; v <= hi + 1e-9; v += step) ticks.push(Math.round(v));
  return { lo, hi, ticks };
}

function lineChart(key, pts, { lo, hi, ticks, ref, refLabel, cls = "" }) {
  const n = pts.length;
  const X = (i) => (n === 1 ? 50 : (i / (n - 1)) * 100);
  const Y = (v) => 100 - ((v - lo) / (hi - lo || 1)) * 100;
  const line = pts.map((p, i) => `${i ? "L" : "M"}${X(i).toFixed(2)},${Y(p.v).toFixed(2)}`).join("");
  const grid = ticks.map((t) => `<line x1="0" x2="100" y1="${Y(t).toFixed(2)}" y2="${Y(t).toFixed(2)}" class="gl"/>`).join("");
  const refLine = ref != null ? `<line x1="0" x2="100" y1="${Y(ref).toFixed(2)}" y2="${Y(ref).toFixed(2)}" class="ref"/>` : "";
  const dots = pts
    .map((p, i) => `<i class="dot ${p.cls || ""}" style="left:${X(i).toFixed(2)}%;top:${Y(p.v).toFixed(2)}%;--i:${Math.min(i, 40)}"></i>`)
    .join("");
  const labels = ticks.map((t) => `<span style="top:${Y(t).toFixed(2)}%">${t}</span>`).join("");
  app.charts[key] = pts.map((p, i) => ({ x: X(i), y: Y(p.v), tip: p.tip }));
  return `
    <div class="chart ${cls}${n > 40 ? " dense" : ""}" data-chart="${key}">
      <div class="yl">${labels}</div>
      <div class="plot">
        <svg viewBox="0 0 100 100" preserveAspectRatio="none">
          <defs><linearGradient id="g-${key}" x1="0" x2="0" y1="0" y2="1"><stop offset="0" class="g0"/><stop offset="1" class="g1"/></linearGradient></defs>
          ${grid}${refLine}
          <path d="${line}L${X(n - 1).toFixed(2)},100L0,100Z" class="area" fill="url(#g-${key})"/>
          <path d="${line}" class="ln" pathLength="1"/>
        </svg>
        ${ref != null && refLabel ? `<span class="ref-lbl" style="top:${Y(ref).toFixed(2)}%">${esc(refLabel)}</span>` : ""}
        ${dots}
        <i class="guide"></i><div class="tip"></div>
      </div>
    </div>`;
}

/** Écart de manches match par match : barre vers le haut = victoire, vers le bas = défaite. */
function resultChart(key, list) {
  const n = list.length;
  const max = Math.max(...list.map((m) => Math.abs(m.scoreUs - m.scoreThem)), 1);
  const w = 100 / n;
  const bars = list
    .map((m, i) => {
      const diff = m.scoreUs - m.scoreThem;
      const h = Math.max(1.5, (Math.abs(diff) / max) * 46);
      const cls = m.won === true ? "w" : m.won === false ? "l" : "n";
      const y = diff >= 0 ? 50 - h : 50;
      return `<rect class="${cls}" style="--i:${Math.min(i, 40)}" x="${(i * w + w * 0.14).toFixed(2)}" y="${y.toFixed(2)}" width="${(w * 0.72).toFixed(2)}" height="${h.toFixed(2)}"/>`;
    })
    .join("");
  app.charts[key] = list.map((m, i) => ({ x: i * w + w / 2, y: m.scoreUs >= m.scoreThem ? 20 : 80, tip: matchTip(m) }));
  return `
    <div class="chart bars" data-chart="${key}">
      <div class="yl"><span style="top:4%">+${max}</span><span style="top:50%">0</span><span style="top:96%">−${max}</span></div>
      <div class="plot">
        <svg viewBox="0 0 100 100" preserveAspectRatio="none"><line x1="0" x2="100" y1="50" y2="50" class="gl"/>${bars}</svg>
        <i class="guide"></i><div class="tip"></div>
      </div>
    </div>`;
}

function careerCharts(d) {
  const list = [...d.matches].reverse(); // du plus ancien au plus récent
  if (list.length < 2) return "";
  const xAxis = `<div class="axis"><span>${esc(shortDate(list[0].startMs))}</span><span>${list.length} matchs</span><span>${esc(shortDate(list.at(-1).startMs))}</span></div>`;

  // Score de combat
  const acsVals = list.map((m) => m.acs);
  const avg = acsVals.reduce((a, b) => a + b, 0) / list.length;
  const acsAxis = niceTicks(Math.min(...acsVals) * 0.9, Math.max(...acsVals) * 1.05);
  const acs = lineChart(
    "acs",
    list.map((m) => ({ v: m.acs, cls: m.won === true ? "w" : m.won === false ? "l" : "n", tip: matchTip(m) })),
    { ...acsAxis, ref: avg, refLabel: `moy. ${fmt(avg)}` }
  );

  // Résultats + série en cours
  let streak = 0;
  const last = d.matches[0].won;
  for (const m of d.matches) {
    if (m.won === last && last != null) streak++;
    else break;
  }
  const streakTxt = last == null
    ? ""
    : `Série en cours : <b class="${last ? "good" : "bad"}">${streak} ${last ? "victoire" : "défaite"}${streak > 1 ? "s" : ""}</b>`;
  const wr = d.summary.matches ? Math.round((d.summary.wins / d.summary.matches) * 100) : 0;

  // RR (compétition)
  const rrList = list.filter((m) => m.rrChange != null);
  let rrCard;
  if (rrList.length >= 2 && rrList.some((m) => m.rrChange !== 0)) {
    let acc = 0;
    const pts = rrList.map((m) => ({ v: (acc += m.rrChange), cls: m.rrChange >= 0 ? "w" : "l", tip: matchTip(m) }));
    const axis = niceTicks(Math.min(0, ...pts.map((p) => p.v)), Math.max(0, ...pts.map((p) => p.v)));
    rrCard = `
      <div class="chart-card wide">
        <div class="cc-head"><span class="lbl">Évolution des RR sur la période</span><b class="${acc >= 0 ? "good" : "bad"}">${signed(acc)} RR</b></div>
        ${lineChart("rr", pts, { ...axis, ref: 0, cls: acc >= 0 ? "up" : "down" })}
        <div class="axis"><span>${esc(shortDate(rrList[0].startMs))}</span><span>${rrList.length} matchs classés</span><span>${esc(shortDate(rrList.at(-1).startMs))}</span></div>
      </div>`;
  } else {
    rrCard = `<div class="chart-card wide empty"><div class="cc-head"><span class="lbl">Évolution des RR</span></div><div class="t-empty">Pas de RR sur ces matchs · placements ou modes non classés</div></div>`;
  }

  return `
    <div class="charts">
      <div class="chart-card">
        <div class="cc-head"><span class="lbl">Score de combat par match</span><b>${fmt(avg)} <small>de moyenne</small></b></div>
        ${acs}
        ${xAxis}
      </div>
      <div class="chart-card">
        <div class="cc-head"><span class="lbl">Victoires et défaites</span><b><span class="good">${d.summary.wins} V</span> · <span class="bad">${d.summary.losses} D</span> <small>${wr}%</small></b></div>
        ${resultChart("res", list)}
        <div class="axis"><span>écart de manches</span><span>${streakTxt}</span></div>
      </div>
      ${rrCard}
    </div>`;
}

function careerTabs(c, d) {
  const tab = (id, label, n) => `<button data-tab="${id}" class="${c.tab === id ? "on" : ""}">${label}<small>${n}</small></button>`;
  return `
    <div class="tabs">
      ${tab("matches", "Matchs", d.matches.length)}
      ${tab("agents", "Agents", d.agents.length)}
      ${tab("maps", "Cartes", d.maps.length)}
    </div>`;
}

function careerTabBody(c) {
  const d = c.data;
  if (c.tab === "agents") return careerAgents(d);
  if (c.tab === "maps") return careerMaps(d);
  const shown = c.shown || 20;
  const rest = d.matches.length - shown;
  const more = rest > 0 ? `<button class="more" data-act="more">Afficher ${Math.min(20, rest)} matchs de plus (${rest} restants)</button>` : "";
  return careerMatches({ ...d, matches: d.matches.slice(0, shown) }) + more;
}

function careerMatches(d) {
  return d.matches
    .map((m, i) => {
      const a = agent(m.agentId);
      const map = mapById(m.mapId);
      const cls = m.won === true ? "win" : m.won === false ? "loss" : "";
      const res = m.won === true ? "Victoire" : m.won === false ? "Défaite" : "Nul";
      const kd = m.deaths ? m.kills / m.deaths : m.kills;
      let rr = `<div class="rrc"><b class="flat">—</b></div>`;
      if (m.rrChange != null) {
        const t = tier(m.tierAfter);
        const c = m.rrChange > 0 ? "good" : m.rrChange < 0 ? "bad" : "flat";
        rr = `<div class="rrc"><b class="${c}">${signed(m.rrChange)}</b><img src="${esc(t.icon)}" alt=""></div>`;
      }
      const badge = m.mvp ? `<span class="badge mvp">MVP</span>` : m.teamMvp ? `<span class="badge tmvp">Top équipe</span>` : "";
      const n = (label, v, c = "") => `<div class="num"><span class="lbl">${label}</span><b class="${c}">${v}</b></div>`;
      return `
      <div class="m ${cls}" data-match="${esc(m.matchId)}" style="--i:${i + 10}">
        <div class="mbg" style="background-image:url('${esc(map?.strip || "")}')"></div>
        ${a ? `<img class="ag" src="${esc(a.icon)}" alt="">` : `<div></div>`}
        <div class="mp"><b>${esc(map?.name || "Carte")}${badge}</b><em>${esc(QUEUES[m.queueId] || m.queueId || "")} · ${esc(timeAgo(m.startMs))}</em></div>
        <div class="sc">${m.scoreUs}–${m.scoreThem}<small>${res}</small></div>
        ${n("K / D / A", `${m.kills} / ${m.deaths} / ${m.assists}`)}
        ${n("K/D", fmt(kd, 2), good(kd, 1.1, 0.9))}
        ${n("ACS", m.acs)}
        ${n("KAST", m.light ? "—" : `${m.kast}%`)}
        ${n("HS", `${m.hs}%`)}
        ${n("ADR", m.adr)}
        ${rr}
        <svg class="chev" viewBox="0 0 24 24"><path d="M9 5l7 7-7 7"/></svg>
      </div>`;
    })
    .join("");
}

function careerAgents(d) {
  const head = `<div class="ag-row ag-head"><span>Agent</span><span>Matchs</span><span>Win %</span><span>K/D</span><span>ADR</span><span>ACS</span><span>DDΔ</span><span>Meilleure carte</span></div>`;
  const rows = d.agents
    .map((s, i) => {
      const a = agent(s.agentId);
      const wr = s.matches ? (s.wins / s.matches) * 100 : 0;
      const map = mapById(s.bestMap);
      return `
      <div class="ag-row" style="--i:${i + 10};--a1:${a?.c1 || "#26394a"}">
        <div class="ag-who">${a ? `<img src="${esc(a.icon)}" alt="">` : ""}<div><b>${esc(a?.name || "Agent")}</b><em>${a?.roleIcon ? `<img class="role" src="${esc(a.roleIcon)}" alt="">` : ""}${esc(a?.role || "")}</em></div></div>
        <b>${s.matches}</b>
        <b class="${good(wr, 55, 45)}">${fmt(wr, 1)}%</b>
        <b class="${good(s.kd, 1.1, 0.9)}">${fmt(s.kd, 2)}</b>
        <b>${d.done ? fmt(s.adr, 1) : "…"}</b>
        <b>${fmt(s.acs, 1)}</b>
        <b class="${d.done ? (s.ddelta >= 0 ? "good" : "bad") : ""}">${d.done ? signed(s.ddelta) : "…"}</b>
        <div class="ag-map" style="background-image:url('${esc(map?.strip || "")}')"><b>${esc(map?.name || "—")}</b><span class="${good(s.bestMapWr, 55, 45)}">${s.bestMapWr}% WR</span></div>
      </div>`;
    })
    .join("");
  return head + rows;
}

function careerMaps(d) {
  const head = `<div class="mp-row mp-head"><span>Carte</span><span>Matchs</span><span>Win %</span><span>Manches</span><span>K/D</span><span>ACS</span><span>ADR</span></div>`;
  const rows = d.maps
    .map((s, i) => {
      const map = mapById(s.mapId);
      const wr = s.matches ? (s.wins / s.matches) * 100 : 0;
      return `
      <div class="mp-row" style="--i:${i + 10}">
        <div class="mp-name" style="background-image:url('${esc(map?.strip || "")}')"><b>${esc(map?.name || "Carte")}</b><em>${s.wins}V · ${s.matches - s.wins}D</em></div>
        <b>${s.matches}</b>
        <div class="wr-cell"><b class="${good(wr, 55, 45)}">${fmt(wr, 0)}%</b><i class="wr-bar"><i style="--f:${(wr / 100).toFixed(3)}"></i></i></div>
        <b class="${good(s.roundWin, 52, 48)}">${fmt(s.roundWin, 0)}%</b>
        <b class="${good(s.kd, 1.1, 0.9)}">${fmt(s.kd, 2)}</b>
        <b>${fmt(s.acs, 0)}</b>
        <b>${d.done ? fmt(s.adr, 0) : "…"}</b>
      </div>`;
    })
    .join("");
  return head + rows;
}

/* ── Détail d'un match ── */

const ROUND_ICONS = {
  Elimination: `<path d="M5 5l14 14M19 5L5 19"/>`,
  Detonate: `<path d="M12 2v6M12 16v6M2 12h6M16 12h6M5 5l4 4M15 15l4 4M19 5l-4 4M9 15l-4 4"/>`,
  Defuse: `<path d="M4 12l5 5L20 6"/>`,
  Timer: `<circle cx="12" cy="13" r="8"/><path d="M12 9v4l3 2M9 2h6"/>`,
  Surrendered: `<path d="M6 21V4M6 4h11l-2 4 2 4H6"/>`,
};
const ROUND_LABELS = { Elimination: "Élimination", Detonate: "Spike explosé", Defuse: "Spike désamorcé", Timer: "Temps écoulé", Surrendered: "Abandon" };

function viewMatch(v) {
  const bar = `<div class="career-bar">${BACK_BTN}<span class="tabs-note">Détail du match</span></div>`;
  return `${bar}<div id="mdbody">${matchBody(v)}</div>`;
}

function matchBody(v) {
  if (v.error) return `<div class="c-msg">Match indisponible<br><small style="color:var(--ink-3)">${esc(v.error)}</small></div>`;
  if (!v.data) {
    return `<div class="sk md-sk-head"></div><div class="sk md-sk-rounds"></div>${Array.from({ length: 10 }, () => `<div class="sk md-sk-row"></div>`).join("")}`;
  }
  const d = v.data;
  const mine = d.teams.find((t) => t.players.some((p) => p.puuid === v.perspective)) || d.teams[0];
  const others = d.teams.filter((t) => t !== mine);
  const them = others[0];
  const map = mapById(d.mapId);
  const won = mine?.won && !(them && them.roundsWon === mine.roundsWon);
  const draw = them && them.roundsWon === mine?.roundsWon;
  const res = draw ? "Égalité" : won ? "Victoire" : "Défaite";
  const resCls = draw ? "" : won ? "good" : "bad";
  const date = d.startMs ? new Date(d.startMs).toLocaleDateString("fr-FR", { weekday: "long", day: "numeric", month: "long" }) : "";

  const head = `
    <div class="md-head" style="--i:0">
      <div class="md-bg" style="background-image:url('${esc(map?.splash || "")}')"></div>
      <div class="md-left">
        <small>${esc(QUEUES[d.queueId] || d.queueId || "Partie")}${d.actName ? ` · ${esc(d.actName)}` : ""}</small>
        <h2>${esc(map?.name || "Carte")}</h2>
        <em>${esc(date)} · ${esc(timeAgo(d.startMs))}${d.lengthMs ? ` · ${duration(d.lengthMs)}` : ""}</em>
      </div>
      <div class="md-score">
        <b class="good">${mine?.roundsWon ?? 0}</b><span>–</span><b class="bad">${them?.roundsWon ?? 0}</b>
        <div class="md-res ${resCls}">${res}</div>
      </div>
    </div>`;

  const rounds = d.rounds.length
    ? `<div class="md-rounds" style="--i:1">${d.rounds
        .map((r, i) => {
          const ours = r.winner === mine?.teamId;
          const sep = i === 12 ? `<i class="half">Mi-temps</i>` : i === 24 ? `<i class="half">Prolong.</i>` : "";
          return `${sep}<div class="rd ${ours ? "w" : "l"}" style="--i:${i}" title="Manche ${i + 1} · ${esc(ROUND_LABELS[r.result] || r.result)}"><svg viewBox="0 0 24 24">${ROUND_ICONS[r.result] || ROUND_ICONS.Elimination}</svg><span>${i + 1}</span></div>`;
        })
        .join("")}</div>`
    : "";

  return `${head}${rounds}${matchTeams(v, d)}`;
}

/** Tableau des scores des deux équipes, vu depuis `v.perspective`. */
function matchTeams(v, d) {
  const mine = d.teams.find((t) => t.players.some((p) => p.puuid === v.perspective)) || d.teams[0];
  const others = d.teams.filter((t) => t !== mine);
  const draw = others[0] && others[0].roundsWon === mine?.roundsWon;
  const team = (t, ally, offset) => {
    if (!t) return "";
    const isMe = currentSnap()?.players.some((p) => p.isMe && p.puuid === v.perspective);
    const title = ally ? (isMe ? "Ton équipe" : "Son équipe") : "Adversaires";
    const rows = t.players.map((p, i) => matchRow(p, v, offset + i)).join("");
    return `
      <div class="md-team ${ally ? "ally" : "enemy"}">
        <div class="md-thead">
          <div class="team-title"><small>${t.won ? "Victoire" : draw ? "Égalité" : "Défaite"} · ${t.roundsWon} manches</small><span>${title}</span></div>
          ${teamAvg(t.players.map((p) => ({ rank: { tier: p.tier } })))}
        </div>
        <div class="md-row md-hrow"><span></span><span></span><span>Joueur</span><span title="Score de combat moyen">ACS</span><span>K / D / A</span><span>K/D</span><span title="Kills − morts">+/−</span><span title="Dégâts par manche">ADR</span><span>HS</span><span>KAST</span><span title="First bloods">FB</span><span title="First deaths">FD</span><span title="Manches à 3 kills ou plus">MK</span></div>
        ${rows}
      </div>`;
  };
  return `${team(mine, true, 2)}${others.map((t) => team(t, false, 8)).join("")}`;
}

/** Pseudo d'un joueur d'un ancien match. Riot ne fournit plus les pseudos dans le détail
 *  des matchs : on n'affiche que ceux déjà connus (partie en cours, carrières ouvertes). */
function knownName(puuid, line) {
  if (hiddenLive(puuid)) return null;
  if (line?.name) return { name: line.name, tag: line.tag };
  const live = app.snap?.players.find((p) => p.puuid === puuid && p.name);
  if (live) return { name: live.name, tag: live.tag };
  const seen = [...app.stack, app.view].find((v) => v?.kind === "career" && v.player.puuid === puuid && v.player.name);
  return seen ? { name: seen.player.name, tag: seen.player.tag } : null;
}

function matchRow(p, v, i) {
  const a = agent(p.agentId);
  const t = tier(p.tier);
  const known = knownName(p.puuid, p);
  const name = known
    ? `<span class="n">${esc(known.name)}</span><span class="tag">#${esc(known.tag || "")}</span>`
    : v.data?.namesPending && !hiddenLive(p.puuid)
      ? `<span class="n"><i class="sk sk-name"></i></span>`
      : `<span class="n hidden">${esc(a?.name || "Joueur")}</span><span class="tag">${hiddenLive(p.puuid) ? "masqué" : "anonyme"}</span>`;
  const dot = p.party != null ? `<i class="pdot" style="--pc:${PARTY_COLORS[p.party % PARTY_COLORS.length]}" title="En groupe"></i>` : "";
  const badge = medalChip(p) + (p.mvp ? `<span class="badge mvp">MVP</span>` : p.teamMvp ? `<span class="badge tmvp">Top</span>` : "");
  const kd = p.deaths ? p.kills / p.deaths : p.kills;
  const diff = p.kills - p.deaths;
  const focus = p.puuid === v.perspective ? " focus" : "";
  return `
    <div class="md-row${focus}" data-puuid="${esc(p.puuid)}" data-from="match" style="--i:${i};--a1:${a?.c1 || "#26394a"}">
      <img class="md-rank" src="${esc(t.icon)}" alt="" title="${esc(t.name)}">
      ${a ? `<img class="md-ag" src="${esc(a.icon)}" alt="">` : `<div class="md-ag"></div>`}
      <div class="md-who"><div class="name">${dot}${name}${badge}</div><em>${esc(t.name)}${p.level ? ` · NV ${p.level}` : ""}</em></div>
      <b class="acs">${p.acs}</b>
      <b>${p.kills} / ${p.deaths} / ${p.assists}</b>
      <b class="${good(kd, 1.1, 0.9)}">${fmt(kd, 2)}</b>
      <b class="${diff > 0 ? "good" : diff < 0 ? "bad" : ""}">${signed(diff)}</b>
      <b>${p.adr}</b>
      <b>${p.hs}%</b>
      <b>${p.kast}%</b>
      <b class="${p.firstBloods ? "good" : "dim"}">${p.firstBloods}</b>
      <b class="${p.firstDeaths ? "bad" : "dim"}">${p.firstDeaths}</b>
      <b class="${p.multikills ? "" : "dim"}">${p.multikills}${p.aces ? `<small class="ace">ACE</small>` : ""}</b>
    </div>`;
}

/* ── Fin de partie ──
   Écran affiché après chaque partie : verdict, RR, médaille et score de performance (Riot),
   stats du match, score de combat de chaque manche et tableau des scores. */

const MEDALS = {
  distinction: { label: "Distinction", stars: 3 },
  merit: { label: "Mérite", stars: 2 },
  pass: { label: "Réussite", stars: 1 },
};
const TRENDS = {
  double_up: ["Excellent", "up2"],
  up: ["Bon", "up"],
  neutral: ["Moyen", "mid"],
  down: ["Faible", "down"],
  double_down: ["Très faible", "down2"],
};
const FACTORS = {
  killImpact: "Impact des kills",
  damage: "Dégâts infligés",
  trades: "Échanges",
  deathImpact: "Impact des morts",
  assists: "Assistances",
  utilityUsage: "Utilitaires",
  plants: "Poses du spike",
  defuses: "Désamorçages",
};
const FACTOR_ORDER = Object.keys(FACTORS);
const CEREMONIES = { Ace: "ACE", Clutch: "CLUTCH", TeamAce: "TEAM ACE", Thrifty: "ÉCO", Flawless: "FLAWLESS", Closer: "CLOSER" };

function medalChip(p) {
  if (!p.medal) return "";
  const m = MEDALS[p.medal] || MEDALS.pass;
  return `<span class="mchip m-${esc(p.medal)}" title="${m.label} · score de performance ${p.perf ?? "—"}"><i></i>${p.perf ?? ""}</span>`;
}

function trendIcon(t) {
  const paths = {
    double_up: `<path d="M6 12l6-6 6 6"/><path d="M6 18l6-6 6 6"/>`,
    up: `<path d="M6 15l6-6 6 6"/>`,
    down: `<path d="M6 9l6 6 6-6"/>`,
    double_down: `<path d="M6 6l6 6 6-6"/><path d="M6 12l6 6 6-6"/>`,
  };
  const cls = (TRENDS[t] || TRENDS.neutral)[1];
  return `<svg class="ti t-${cls}" viewBox="0 0 24 24" aria-hidden="true">${paths[t] || `<path d="M7 12h10"/>`}</svg>`;
}

/** Emblème de la médaille : hexagone, chevron et étoiles (3 / 2 / 1), rayons pour la distinction. */
function medalEmblem(kind) {
  const n = MEDALS[kind]?.stars || 1;
  const stars = Array.from({ length: n }, (_, i) => {
    const x = 60 + (i - (n - 1) / 2) * 15;
    return `<path class="st" style="--i:${i}" d="M${x} 22l4.5 6-4.5 6-4.5-6z"/>`;
  }).join("");
  const rays = kind === "distinction"
    ? `<g class="rays">${Array.from({ length: 16 }, (_, i) => `<path d="M60 60L57.5 -14h5z" transform="rotate(${i * 22.5} 60 60)"/>`).join("")}</g>`
    : "";
  return `<svg class="emblem" viewBox="0 0 120 120" aria-hidden="true">
    <defs><linearGradient id="medalGrad" x1="0" y1="0" x2="0.35" y2="1"><stop offset="0" style="stop-color:var(--m1)"/><stop offset="1" style="stop-color:var(--m2)"/></linearGradient></defs>
    ${rays}
    <path class="e-out" d="M60 4l50 28v56l-50 28-50-28V32z"/>
    <path class="e-in" d="M60 13l42 24v46l-42 24-42-24V37z"/>
    <path class="e-chev" d="M32 45h15l13 21 13-21h15L60 90z"/>
    ${stars}
  </svg>`;
}

function perfGauge(p, s) {
  const x = (v) => Math.max(0, Math.min(1, v / (s.max || 500))).toFixed(4);
  return `
    <div class="gauge">
      <div class="g-head"><span class="lbl">Barème</span>
        <div class="g-legend"><i class="m-pass">Réussite</i><i class="m-merit">Mérite ${s.merit}+</i><i class="m-distinction">Distinction ${s.distinction}+</i></div>
      </div>
      <div class="g-track" style="--f:${x(p.perf)}">
        <i class="g-zone m-merit" style="--a:${x(s.merit)};--b:${x(s.distinction)}"></i>
        <i class="g-zone m-distinction" style="--a:${x(s.distinction)};--b:1"></i>
        <i class="g-fill"></i>
        <i class="g-avg" style="--x:${x(s.avg)}"></i>
        <i class="g-you" style="--x:${x(p.perf)}"></i>
      </div>
      <div class="g-marks"><span style="--x:0">0</span><span style="--x:${x(s.avg)}">Moy. ${s.avg}</span><span style="--x:${x(s.merit)}">${s.merit}</span><span style="--x:${x(s.distinction)}">${s.distinction}</span><span style="--x:1">${s.max}</span></div>
    </div>`;
}

function perfAxis(label, tip, score, trend, factors, s, i) {
  const [word, cls] = TRENDS[trend] || TRENDS.neutral;
  const list = (factors || []).slice().sort((a, b) => FACTOR_ORDER.indexOf(a[0]) - FACTOR_ORDER.indexOf(b[0]));
  return `
    <div class="pax t-${cls}" style="--i:${i}" title="${esc(tip)}">
      <div class="ax-head"><span class="lbl">${label}</span><span class="trend t-${cls}">${trendIcon(trend)}${word}</span></div>
      ${num(score ?? 0)}
      <div class="ax-bar" style="--f:${Math.min(1, (score || 0) / (s.max || 500)).toFixed(4)};--avg:${(s.avg / (s.max || 500)).toFixed(4)}"><i></i></div>
      <ul>${list.map(([k, t], j) => `<li style="--j:${j}"><span>${esc(FACTORS[k] || k)}</span>${trendIcon(t)}</li>`).join("")}</ul>
    </div>`;
}

function rrBlock(rr) {
  if (!rr) return `<div class="rs-rr none"></div>`;
  const before = tier(rr.tierBefore);
  const after = tier(rr.tierAfter);
  const moved = rr.tierAfter !== rr.tierBefore && rr.tierBefore > 0;
  const up = rr.tierAfter > rr.tierBefore;
  const pct = (v) => (Math.max(0, Math.min(100, v)) / 100).toFixed(3);
  let seg = "";
  if (!moved && rr.earned > 0) seg = `<i class="rr-seg gain" style="--a:${pct(rr.rrBefore)};--b:${pct(rr.rrAfter)}"></i>`;
  if (!moved && rr.earned < 0) seg = `<i class="rr-seg loss" style="--a:${pct(rr.rrAfter)};--b:${pct(rr.rrBefore)}"></i>`;
  const arrow = `<svg class="rr-arrow" viewBox="0 0 24 24"><path d="M5 12h13M13 6l6 6-6 6"/></svg>`;
  return `
    <div class="rs-rr ${rr.earned >= 0 ? "pos" : "neg"}" style="--rc:${after.color}">
      <div class="rr-icons">${moved ? `<img class="rr-old" src="${esc(before.icon)}" alt="" title="${esc(before.name)}">${arrow}` : ""}<img class="rr-new" src="${esc(after.icon)}" alt="" title="${esc(after.name)}"></div>
      <div class="rr-txt">
        ${moved ? `<span class="rr-move ${up ? "up" : "down"}">${up ? "Promotion" : "Rétrogradation"}</span>` : `<span class="lbl">Classement</span>`}
        <div class="rr-earned">${num(rr.earned, { sign: true })}<small>RR</small></div>
        <div class="rr-bar" style="--f:${pct(rr.rrAfter)}"><i class="fill"></i>${seg}</div>
        <em>${esc(after.name)} · ${rr.rrAfter} RR${rr.afkPenalty ? ` · pénalité AFK ${rr.afkPenalty}` : ""}</em>
      </div>
    </div>`;
}

function resultHero(r, d, me, mine, them) {
  const map = mapById(d.mapId);
  const ffa = d.teams.length > 2;
  const all = d.teams.flatMap((t) => t.players);
  const place = 1 + all.filter((p) => p.kills > me.kills).length;
  const draw = !ffa && them && them.roundsWon === mine.roundsWon;
  const won = ffa ? place === 1 : !draw && mine.won;
  const verdict = ffa && !won ? `${place}e place` : draw ? "Égalité" : won ? "Victoire" : "Défaite";
  const queue = QUEUES[d.queueId] || (d.queueId ? d.queueId : "Partie personnalisée");
  const mvp = me.mvp ? `<span class="rs-mvp">MVP du match</span>` : me.teamMvp ? `<span class="rs-mvp team">MVP de l'équipe</span>` : "";
  const score = ffa
    ? `<div class="rs-score"><b class="us">${me.kills}</b><span>kills</span></div>`
    : `<div class="rs-score"><b class="us">${mine.roundsWon}</b><span>:</span><b class="them">${them?.roundsWon ?? 0}</b></div>`;
  const ended = d.startMs ? timeAgo(d.startMs + (d.lengthMs || 0)) : "";
  return `
    <section class="rs-hero ${draw ? "draw" : won ? "win" : "loss"}">
      <div class="rs-bg" style="background-image:url('${esc(map?.splash || "")}')"></div>
      <i class="rs-slash"></i>
      <div class="rs-hl">
        <small>${esc(queue)}${d.actName ? ` · ${esc(d.actName)}` : ""}</small>
        <h1 class="rs-verdict">${esc(verdict)}</h1>
        <em>${esc(map?.name || "Carte")}${d.rounds.length > 1 ? ` · ${d.rounds.length} manches` : ""}${d.lengthMs ? ` · ${duration(d.lengthMs)}` : ""}${ended ? ` · ${esc(ended)}` : ""}</em>
        ${mvp}
      </div>
      ${score}
      ${rrBlock(r.rr)}
    </section>`;
}

function resultMedal(me, s) {
  if (!me.medal || !s) return "";
  const m = MEDALS[me.medal] || MEDALS.pass;
  return `
    <section class="rs-medal m-${esc(me.medal)}">
      <div class="rs-emblem">${medalEmblem(me.medal)}</div>
      <div class="rs-mtxt">
        <span class="lbl">Médaille de fin de partie</span>
        <h2>${m.label}</h2>
        <div class="rs-perf">${num(me.perf ?? 0)}<small>/ ${s.max}</small></div>
        <span class="lbl">Score de performance</span>
      </div>
      ${perfGauge(me, s)}
      <div class="axes">
        ${perfAxis("Attaque", "Impact offensif : kills, dégâts, échanges, morts", me.offense, me.offTrend, me.offFactors, s, 0)}
        ${perfAxis("Soutien", "Jeu d'équipe : assistances, utilitaires, spike", me.support, me.supTrend, me.supFactors, s, 1)}
      </div>
    </section>`;
}

function resultStats(me, d) {
  const all = d.teams.flatMap((t) => t.players);
  const place = 1 + all.filter((p) => p.acs > me.acs).length;
  const rounds = Math.max(1, me.rounds || d.rounds.length);
  const kd = me.deaths ? me.kills / me.deaths : me.kills;
  const diff = me.kills - me.deaths;
  const big = (i, label, html, sub, fill, tip) =>
    `<div class="big" style="--i:${i + 3};--f:${Math.max(0.02, Math.min(1, fill)).toFixed(3)}" title="${esc(tip)}"><span class="lbl">${label}</span>${html}<em>${sub}</em><i class="meter"><i></i></i></div>`;
  const kda = `<div class="kda">${num(me.kills)}<span>/</span>${num(me.deaths)}<span>/</span>${num(me.assists)}</div>`;
  const cells = [
    ["Headshot", num(me.hs, { suffix: "%" }), "Part des balles touchées à la tête"],
    ["DDΔ / manche", num(me.ddelta, { sign: true, cls: me.ddelta >= 0 ? "good" : "bad" }), "Dégâts infligés moins dégâts reçus, par manche"],
    ["Kills / manche", num(me.kills / rounds, { d: 2 }), "Kills en moyenne par manche"],
    ["Place (ACS)", `<b>#${place}<small> / ${all.length}</small></b>`, "Classement au score de combat parmi tous les joueurs"],
    ["First bloods", num(me.firstBloods, { cls: me.firstBloods ? "good" : "" }), "Premier kill de la manche"],
    ["First deaths", num(me.firstDeaths, { cls: me.firstDeaths ? "bad" : "" }), "Premier mort de la manche"],
    ["Multi-kills", num(me.multikills), "Manches avec au moins 3 ennemis différents tués"],
    ["Aces", num(me.aces, { cls: me.aces ? "gold" : "" }), "Manches où tu as tué les 5 ennemis"],
    ["Clutchs", num(me.clutches, { cls: me.clutches ? "gold" : "" }), "Manches gagnées en clutch (reconnues par Riot)"],
    ["Econ rating", num(me.econ), "Dégâts infligés pour 1 000 crédits dépensés"],
    ["Spikes posés", num(me.plants), "Spikes posés"],
    ["Désamorçages", num(me.defuses), "Spikes désamorcés"],
  ];
  return `
    <section class="rs-stats">
      <div class="bigs">
        ${big(0, "Score de combat", num(me.acs), `${me.score.toLocaleString("fr-FR")} points au total`, me.acs / 400, "ACS : score de combat moyen par manche")}
        ${big(1, "K / D / A", kda, `K/D ${fmt(kd, 2)} · ${signed(diff)}`, kd / 2, "Kills / morts / assists")}
        ${big(2, "KAST", num(me.kast, { suffix: "%" }), "Kill, assist, survie ou échange", me.kast / 100, "Manches avec un kill, une assist, une survie ou une mort vengée")}
        ${big(3, "Dégâts / manche", num(me.adr), `${me.damage.toLocaleString("fr-FR")} dégâts`, me.adr / 250, "ADR : dégâts infligés en moyenne par manche")}
      </div>
      <div class="grid rs-grid">${cells
        .map(([l, v, tip], i) => `<div class="cell" style="--i:${i + 7}" title="${esc(tip)}"><span class="lbl">${l}</span>${v}</div>`)
        .join("")}</div>
    </section>`;
}

/** Score de combat de chaque manche : barres (manche gagnée / perdue), kills et cérémonies. */
function resultRounds(me, d, mine) {
  const scores = me.roundScores || [];
  if (d.rounds.length < 2 || !scores.some((s) => s > 0)) return "";
  const kills = me.roundKills || [];
  const top = Math.max(600, ...scores);
  const best = scores.indexOf(Math.max(...scores));
  const bars = d.rounds
    .map((r, i) => {
      const won = r.winner === mine.teamId;
      const s = scores[i] || 0;
      const k = kills[i] || 0;
      const tag = r.player === me.puuid && CEREMONIES[r.ceremony] ? CEREMONIES[r.ceremony] : k >= 3 ? `${k}K` : "";
      const sep = i === 12 ? `<i class="rb-half" title="Mi-temps"></i>` : i === 24 ? `<i class="rb-half" title="Prolongation"></i>` : "";
      const tip = `Manche ${i + 1} · ${won ? "gagnée" : "perdue"} (${ROUND_LABELS[r.result] || r.result})\n${s} points · ${k} kill${k > 1 ? "s" : ""}`;
      return `${sep}<div class="rb ${won ? "w" : "l"}${i === best ? " best" : ""}" style="--h:${(s / top).toFixed(3)};--i:${i}" title="${esc(tip)}">
        <div class="rb-col">${tag ? `<em>${tag}</em>` : ""}<b>${s || ""}</b><i></i></div><span>${i + 1}</span></div>`;
    })
    .join("");
  return `
    <section class="rs-rounds">
      <div class="rs-sec"><span class="lbl">Score de combat par manche</span><em>Moyenne <b>${me.acs}</b> · Meilleure manche <b>${scores[best]}</b> (manche ${best + 1})</em></div>
      <div class="rb-plot" style="--avg:${(me.acs / top).toFixed(3)}"><i class="rb-avg"><span>ACS ${me.acs}</span></i>${bars}</div>
    </section>`;
}

function viewResult(v) {
  const r = v.data;
  const d = r.detail;
  const bar = `<div class="career-bar">${BACK_BTN}<span class="tabs-note">Résultat de la partie</span></div>`;
  const me = d.teams.flatMap((t) => t.players).find((p) => p.puuid === r.puuid);
  if (!me) return `${bar}<div class="c-msg">Résultat indisponible</div>`;
  const mine = d.teams.find((t) => t.players.includes(me));
  const them = d.teams.find((t) => t !== mine);
  const board = d.teams.length > 2
    ? matchTeams(v, { ...d, teams: [{ ...mine, players: d.teams.flatMap((t) => t.players).sort((a, b) => b.kills - a.kills) }] })
    : matchTeams(v, d);
  return `${bar}<div class="rs">
    ${resultHero(r, d, me, mine, them)}
    <div class="rs-main${me.medal && d.perfScale ? "" : " no-medal"}">${resultMedal(me, d.perfScale)}${resultStats(me, d)}</div>
    ${resultRounds(me, d, mine)}
    <div class="rs-board"><div class="rs-sec"><span class="lbl">Tableau des scores</span></div>${board}</div>
  </div>`;
}

/** Bandeau « Dernière partie » du salon. */
function lastResultCard() {
  const r = app.demo ? window.DEMO?.result(app.assets, currentSnap()) : app.result;
  const d = r?.detail;
  const me = d?.teams.flatMap((t) => t.players).find((p) => p.puuid === r.puuid);
  if (!me) return "";
  const mine = d.teams.find((t) => t.players.includes(me));
  const them = d.teams.find((t) => t !== mine);
  const draw = d.teams.length === 2 && them.roundsWon === mine.roundsWon;
  const cls = draw ? "draw" : mine.won ? "win" : "loss";
  const map = mapById(d.mapId);
  const stat = (label, html) => `<div class="lr-stat"><span class="lbl">${label}</span>${html}</div>`;
  return `
    <div class="last-res ${cls}" data-act="result" style="--i:6">
      <div class="lr-bg" style="background-image:url('${esc(map?.splash || "")}')"></div>
      <div class="lr-l"><span class="lbl">Dernière partie · ${esc(timeAgo(d.startMs + (d.lengthMs || 0)))}</span><b>${draw ? "Égalité" : mine.won ? "Victoire" : "Défaite"}</b><em>${esc(map?.name || "")} · ${esc(QUEUES[d.queueId] || d.queueId || "Partie")}</em></div>
      ${d.teams.length === 2 ? `<div class="lr-score"><b class="good">${mine.roundsWon}</b><span>:</span><b class="bad">${them.roundsWon}</b></div>` : ""}
      ${me.medal ? stat("Médaille", `<b class="lr-medal m-${esc(me.medal)}"><i></i>${(MEDALS[me.medal] || MEDALS.pass).label}</b>`) : ""}
      ${stat("ACS", `<b>${me.acs}</b>`)}
      ${stat("K / D / A", `<b>${me.kills} / ${me.deaths} / ${me.assists}</b>`)}
      ${r.rr ? stat("RR", `<b class="${r.rr.earned >= 0 ? "good" : "bad"}">${signed(r.rr.earned)}</b>`) : ""}
      <span class="lr-go">Voir le résultat<svg viewBox="0 0 24 24"><path d="M9 5l7 7-7 7"/></svg></span>
    </div>`;
}

function openResult(r) {
  r ||= app.demo || !TAURI ? window.DEMO?.result(app.assets, currentSnap()) : app.result;
  if (!r) return;
  app.resultPending = false;
  if (app.view?.kind === "result" && app.view.data.detail.matchId === r.detail.matchId) return;
  pushView({ kind: "result", data: r, perspective: r.puuid });
  countUp($("#view"));
}

/** Compteurs : de 0 à la valeur finale, une seule boucle requestAnimationFrame (~0,7 s). */
function countUp(root) {
  if (app.cfg?.animations === false) return;
  const els = [...root.querySelectorAll("[data-n]")];
  if (!els.length) return;
  const items = els.map((el) => ({ el, to: +el.dataset.n, d: +el.dataset.d || 0, s: el.dataset.s || "", sign: !!el.dataset.sign }));
  const t0 = performance.now();
  const dur = 700;
  const step = (now) => {
    const k = Math.min(1, (now - t0) / dur);
    const e = 1 - Math.pow(1 - k, 3);
    for (const it of items) {
      const v = it.to * e;
      it.el.textContent = (it.sign ? signed(v, it.d) : fmt(v, it.d)) + it.s;
    }
    if (k < 1) requestAnimationFrame(step);
  };
  requestAnimationFrame(step);
}

/* ── Navigation ── */

function pushView(view) {
  if (app.view) {
    app.view.scroll = $("#view").scrollTop;
    app.stack.push(app.view);
  }
  app.view = view;
  $("#view").scrollTop = 0;
  transition();
}

function goBack() {
  app.view = app.stack.pop() || null;
  transition();
  if (app.view) {
    $("#view").scrollTop = app.view.scroll || 0;
    if (app.view.data) countUp($("#view"));
    // Chargement encore en cours pour cette carrière : il continue, pas de nouvelle requête
    if (app.view.kind === "career" && !app.view.data?.done && !app.view.loading) fetchCareer(app.view);
  }
}

/** Joueur à partir de son identifiant : partie en cours en priorité, sinon ligne d'un match. */
function playerFor(puuid) {
  const live = currentSnap()?.players.find((p) => p.puuid === puuid);
  if (live) return live;
  const v = app.view;
  const detail = v?.kind === "match" ? v.data : v?.kind === "result" ? v.data.detail : null;
  const line = detail?.teams.flatMap((t) => t.players).find((p) => p.puuid === puuid);
  if (!line) return null;
  const me = currentSnap()?.players.find((p) => p.isMe);
  const known = knownName(puuid, line);
  return {
    puuid,
    name: known?.name || null,
    tag: known?.tag || null,
    agentId: line.agentId,
    level: line.level,
    cardId: line.cardId,
    isMe: me?.puuid === puuid,
    party: null,
    rank: null,
  };
}

const careerKey = (v) => `${v.player.puuid}|${v.actId || "cur"}|${v.competitive ? "c" : "a"}`;

function openCareer(puuid, given = null) {
  const player = given || playerFor(puuid);
  if (!player) return;
  const view = { kind: "career", player, competitive: true, actId: null, tab: "matches", shown: 20, data: null, error: null };
  // Déjà consulté : affichage immédiat, puis actualisation discrète.
  view.data = app.careerCache.get(careerKey(view)) || null;
  view.filled = !!view.data;
  pushView(view);
  if (view.data) countUp($("#perf"));
  fetchCareer(view);
  fetchRank(view);
}

async function fetchRank(view) {
  try {
    view.detail = app.demo || !TAURI
      ? await window.DEMO.rank(view.player)
      : await TAURI.core.invoke("get_rank", { puuid: view.player.puuid });
  } catch {
    view.detailError = true;
  }
  updateSeason(view);
  if (app.view === view) {
    // Le rang actuel de la carte peut changer, et l'en-tête des stats utilise le bilan de l'acte.
    const card = $(".c-card .rank");
    if (card) card.outerHTML = rankBlock(careerRank(view));
    if (view.data?.done) updatePerf(view);
  }
}

async function fetchCareer(view) {
  if (!view || view.kind !== "career") return;
  const request = ++app.careerReq;
  view.request = request;
  view.error = null;
  const key = careerKey(view);
  let lastPartial = 0;
  const apply = (data) => {
    if (view.request !== request) return;
    if (!data.done) {
      // Stats complètes déjà affichées (cache) : on les garde jusqu'au résultat final.
      if (view.data?.done) return;
      // Progression : seule la barre bouge, mise à jour sur place (au plus 4 fois par seconde).
      const now = performance.now();
      if (now - lastPartial < 250) return;
      lastPartial = now;
      if (view.data?.mainReady && data.mainReady) {
        view.data = data; // stats principales identiques : rien à redessiner avant la fin
        return;
      }
      const label = app.view === view && $("#progLabel");
      if (label && view.data && !view.data.done && !data.mainReady) {
        view.data = data;
        const f = data.found ? data.analyzed / data.found : 0;
        label.textContent = data.found ? `Analyse des matchs · ${data.analyzed} / ${data.found}` : "Recherche des matchs";
        $("#progBar")?.style.setProperty("--f", Math.max(0.04, f).toFixed(3));
        return;
      }
    } else {
      app.careerCache.set(key, data);
      const same = view.data?.done && view.data.analyzed === data.analyzed && view.data.matches[0]?.matchId === data.matches[0]?.matchId;
      if (same) return;
    }
    view.data = data;
    updatePerf(view);
  };
  // Chaque chargement reçoit uniquement sa propre progression, et va au bout même si on ouvre
  // une autre carrière entre-temps : au retour, le résultat est là.
  view.loading = request;
  try {
    if (app.demo || !TAURI) {
      apply(await window.DEMO.career(app.assets, view.player, view.competitive, view.actId, apply));
    } else {
      app.careerApplies.set(request, apply);
      apply(await TAURI.core.invoke("get_career", { puuid: view.player.puuid, competitive: view.competitive, actId: view.actId, request }));
    }
  } catch (e) {
    if (view.request !== request) return;
    view.error = String(e);
    updatePerf(view);
  } finally {
    app.careerApplies.delete(request);
    if (view.loading === request) view.loading = null;
  }
}

/** Ne remplace que la zone des stats : pas de rechargement ni d'animation de toute la page. */
function updatePerf(view) {
  if (app.view !== view) return;
  const el = $("#perf");
  if (!el) return;
  // Animation d’entrée et compteurs : une seule fois, à l’arrivée des stats finales
  const first = !view.filled && !!(view.data?.done || view.data?.mainReady);
  syncEnter($("#root"), app.enterAt);
  syncEnter(el, el._enterAt);
  el.innerHTML = perfHtml(view);
  updatePlayerCard(view);
  if (first) {
    view.filled = true;
    animateIn(el);
    countUp(el);
  }
}

function updateSeason(view) {
  if (app.view !== view) return;
  syncEnter($("#root"), app.enterAt);
  const el = $("#season");
  if (el) el.innerHTML = careerSeason(view);
}

/** Relance les animations d'entrée d'une seule zone. */
function animateIn(el) {
  if (app.cfg?.animations === false) return;
  el._enterAt = performance.now();
  el.style.setProperty("--ea", "0ms");
  el.classList.remove("enter");
  void el.offsetWidth;
  el.classList.add("enter");
  clearTimeout(el._enterT);
  el._enterT = setTimeout(() => {
    el.classList.remove("enter");
    el.style.removeProperty("--ea");
  }, 2000);
}

/** Pendant une entrée animée, les éléments recréés reprennent leur animation où elle en est. */
function syncEnter(el, since) {
  if (el?.classList.contains("enter") && since) el.style.setProperty("--ea", `-${Math.round(performance.now() - since)}ms`);
}

/** Bannière et niveau connus seulement après l'analyse (joueur trouvé par la recherche). */
function updatePlayerCard(view) {
  const d = view.data;
  if (!d) return;
  const p = view.player;
  const card = !p.cardId && d.playerCard;
  const level = p.level == null && d.playerLevel;
  if (!card && !level) return;
  view.player = { ...p, cardId: p.cardId || d.playerCard, level: p.level ?? d.playerLevel };
  const el = $(".c-card");
  if (el && card) {
    el.style.backgroundImage = `url('${cardUrl(view.player.cardId, "largeart")}')`;
    el.classList.add("card-in");
  }
  const sub = $(".c-card .sub");
  if (sub && level) sub.outerHTML = subBlock(view.player);
}

/** Change la période ou le mode sans quitter la carrière. */
function switchScope(view, patch) {
  Object.assign(view, patch, { shown: 20 });
  view.data = app.careerCache.get(careerKey(view)) || null;
  view.filled = false;
  updateSeason(view);
  if (view.data) {
    updatePerf(view);
    animateIn($("#perf"));
    countUp($("#perf"));
    view.filled = true;
  } else {
    updatePerf(view);
  }
  document.querySelectorAll(".seg [data-queue]").forEach((b) => b.classList.toggle("on", (b.dataset.queue === "comp") === view.competitive));
  fetchCareer(view);
}

function selectAct(id) {
  const v = app.view;
  if (v?.kind !== "career") return;
  const act = v.detail?.history.find((a) => a.id === id);
  const actId = !act || act.current ? null : id;
  if (actId === v.actId) return;
  switchScope(v, { actId });
}

function showMoreRows() {
  const v = app.view;
  if (v?.kind !== "career" || !v.data) return;
  v.shown = (v.shown || 20) + 20;
  const body = $("[data-tabbody]");
  if (body) body.innerHTML = careerTabBody(v);
}

function openMatch(matchId) {
  const from = app.view;
  if (from?.kind !== "career") return;
  const view = { kind: "match", id: matchId, perspective: from.player.puuid, data: null, error: null };
  pushView(view);
  fetchMatch(view);
}

async function fetchMatch(view) {
  try {
    view.data = app.demo || !TAURI
      ? await window.DEMO.match(app.assets, view.id, view.perspective)
      : await TAURI.core.invoke("get_match", { matchId: view.id, puuid: view.perspective });
  } catch (e) {
    view.error = String(e);
  }
  if (app.view !== view) return;
  const el = $("#mdbody");
  if (!el) return render();
  el.innerHTML = matchBody(view);
  animateIn(el);
}

function setTab(tab) {
  const c = app.view;
  if (c?.kind !== "career" || !c.data || c.tab === tab) return;
  c.tab = tab;
  document.querySelectorAll(".tabs [data-tab]").forEach((b) => b.classList.toggle("on", b.dataset.tab === tab));
  const body = $("[data-tabbody]");
  body.innerHTML = careerTabBody(c);
  body.classList.remove("swap");
  void body.offsetWidth;
  body.classList.add("swap");
}

/** Infobulle des graphiques : point le plus proche de la souris. */
function chartHover(e) {
  const chart = e.target.closest(".chart");
  document.querySelectorAll(".chart.hover").forEach((c) => c !== chart && c.classList.remove("hover"));
  if (!chart) return;
  const pts = app.charts[chart.dataset.chart];
  const plot = chart.querySelector(".plot");
  if (!pts?.length || !plot) return;
  const r = plot.getBoundingClientRect();
  const x = ((e.clientX - r.left) / r.width) * 100;
  let best = 0;
  pts.forEach((p, i) => {
    if (Math.abs(p.x - x) < Math.abs(pts[best].x - x)) best = i;
  });
  const p = pts[best];
  chart.classList.add("hover");
  const guide = plot.querySelector(".guide");
  guide.style.left = `${p.x}%`;
  const tip = plot.querySelector(".tip");
  tip.innerHTML = p.tip;
  tip.style.left = `${p.x}%`;
  tip.style.top = `${p.y}%`;
  tip.classList.toggle("left", p.x > 60);
  tip.classList.toggle("below", p.y < 35);
  plot.querySelectorAll(".dot.on").forEach((d) => d.classList.remove("on"));
  plot.querySelectorAll(".dot")[best]?.classList.add("on");
}

/** Précharge en arrière-plan la carrière des joueurs de la partie (clic instantané ensuite). */
function prefetchPlayers(s) {
  if (!TAURI || !s || (s.phase !== "pregame" && s.phase !== "ingame")) return;
  const ids = s.players.map((p) => p.puuid).filter(Boolean);
  const key = ids.join(",");
  if (!ids.length || key === app.prefetched) return;
  app.prefetched = key;
  TAURI.core.invoke("prefetch_players", { puuids: ids }).catch(() => {});
}

/* ───────────── Rendu ───────────── */

function renderHeader(s) {
  const m = mapById(s.mapId);
  const q = queueLabel(s);
  const meta = $("#meta");
  if (m && (s.phase === "ingame" || s.phase === "pregame")) {
    meta.innerHTML = `<div class="meta-bg" style="background-image:url('${esc(m.strip || m.splash)}')"></div>
      <div class="meta-txt"><div class="meta-map">${esc(m.name)}</div><div class="meta-sub">${esc(q)}${s.actName ? ` <b>//</b> ${esc(s.actName)}` : ""}</div></div>`;
  } else {
    meta.innerHTML = `<div class="meta-txt"><div class="meta-map">Valorant</div><div class="meta-sub">${esc(s.actName || "")}</div></div>`;
  }
  const [label, cls] = PHASES[s.phase] || PHASES.offline;
  $("#status").innerHTML = `<span class="pill ${cls}"><i></i>${esc(label)}</span>`;

  const v = app.view;
  const focusAgent = v?.kind === "match" || v?.kind === "result"
    ? (v.kind === "result" ? v.data.detail : v.data)?.teams.flatMap((t) => t.players).find((p) => p.puuid === v.perspective)?.agentId
    : (v?.player || s.players.find((p) => p.isMe))?.agentId;
  const bgAgent = agent(focusAgent) || agent(v?.data?.agents?.[0]?.agentId) || agent(DEFAULT_BG_AGENT);
  const img = $("#bgAgent");
  const src = bgAgent?.portrait || "";
  if (img.getAttribute("src") !== src) img.setAttribute("src", src);
  $("#bgWord").textContent = m && s.phase !== "menus" ? m.name : "VALORANT";
}

function render() {
  const s = currentSnap();
  if (!s || !app.assets) return;
  // Pendant l'entrée, les éléments recréés reprennent leur animation là où elle en est
  // (au lieu de repartir de zéro à chaque mise à jour de la partie).
  const root = $("#root");
  if (root.classList.contains("enter")) root.style.setProperty("--ea", `-${Math.round(performance.now() - app.enterAt)}ms`);
  renderHeader(s);
  let html;
  if (app.view) html = app.view.kind === "match" ? viewMatch(app.view) : app.view.kind === "result" ? viewResult(app.view) : viewCareer(app.view);
  else {
    switch (s.phase) {
      case "ingame": html = viewIngame(s); break;
      case "pregame": html = viewPregame(s); break;
      case "menus": html = viewMenus(s); break;
      case "waiting": html = viewEmpty("En attente de Valorant", s.message || "Lance le jeu, l'overlay se connectera tout seul."); break;
      default: html = viewEmpty("Client Riot introuvable", s.message || "Ouvre le client Riot et connecte-toi.");
    }
  }
  const view = $("#view");
  // Partie en cours inchangée : pas de reconstruction de la page (mises à jour fréquentes du client)
  const live = !app.view;
  if (live && html === app.lastLiveHtml && view.childElementCount) return tickTimer(s);
  app.lastLiveHtml = live ? html : null;
  const scroll = view.scrollTop;
  view.innerHTML = html;
  view.scrollTop = scroll;
  tickTimer(s);
}

function tickTimer(s) {
  clearInterval(app.timer);
  const end = app.demo ? (app.demoEnd ??= Date.now() + 42_000) : s?.phaseEndsAt;
  if (!end || app.view) return;
  const draw = () => {
    const el = $("#timer");
    if (!el) return clearInterval(app.timer);
    const sec = Math.max(0, Math.ceil((end - Date.now()) / 1000));
    el.textContent = `${Math.floor(sec / 60)}:${String(sec % 60).padStart(2, "0")}`;
    el.classList.toggle("low", sec <= 10);
  };
  draw();
  app.timer = setInterval(draw, 1000);
}

/** Deux bandes inclinées (rouge puis blanche) qui traversent la fenêtre, puis entrée des éléments. */
function playWipe() {
  const w = $("#wipe");
  w.classList.remove("go");
  void w.offsetWidth;
  w.classList.add("go");
}

function transition() {
  playWipe();
  render();
  playEnter();
}

function playEnter() {
  const root = $("#root");
  app.enterAt = performance.now();
  root.style.setProperty("--ea", "0ms");
  root.classList.remove("enter");
  void root.offsetWidth;
  root.classList.add("enter");
  clearTimeout(app.enterT);
  app.enterT = setTimeout(() => root.classList.remove("enter"), 2000);
}

/* ───────────── Intro ───────────── */

const INTRO_MS = 2150;

const introMode = () => (app.cfg?.animations === false ? "never" : app.cfg?.intro || "launch");

/** Intro à l'ouverture de l'overlay : selon la config "daily" (1re du jour) ou "always".
 *  En mode "launch", elle n'est jouée qu'au démarrage de l'application (voir main). */
function introDue() {
  const mode = introMode();
  if (mode === "never" || mode === "launch") return false;
  if (mode === "always") return true;
  const today = new Date().toDateString();
  try {
    if (localStorage.getItem("introDay") === today) return false;
    localStorage.setItem("introDay", today);
  } catch {
    /* stockage indisponible : on joue l'intro */
  }
  return true;
}

function playIntro() {
  const me = currentSnap()?.players.find((p) => p.isMe);
  const date = new Date().toLocaleDateString("fr-FR", { weekday: "long", day: "numeric", month: "long" });
  const hello = me?.name ? `Bienvenue, <b>${esc(me.name)}</b>` : "Bienvenue";
  $("#introSub").innerHTML = `<span>${hello}</span><em>${esc(date)}</em>`;
  render();
  const el = $("#intro");
  el.classList.remove("play", "out");
  void el.offsetWidth;
  el.classList.add("play");
  clearTimeout(app.introT);
  app.introT = setTimeout(endIntro, INTRO_MS);
}

/** Sortie de l'intro (à la fin, ou plus tôt sur clic / Échap) : l'interface arrive dessous. */
function endIntro() {
  clearTimeout(app.introT);
  const el = $("#intro");
  if (!el.classList.contains("play") || el.classList.contains("out")) return;
  el.classList.add("out");
  playWipe();
  playEnter();
  setTimeout(() => el.classList.remove("play", "out"), 650);
}

const introPlaying = () => $("#intro").classList.contains("play");

function hideOverlay() {
  if (TAURI) TAURI.core.invoke("hide_overlay");
}

/* ───────────── Interactions ───────────── */

function setCompactIcon(compact) {
  document.documentElement.classList.toggle("compact", !!compact);
  const b = $("#compactBtn");
  if (b) b.title = b.ariaLabel = compact ? "Agrandir la fenêtre" : "Réduire la fenêtre";
}

function bindUi() {
  $("#view").addEventListener("click", (e) => {
    const el = e.target.closest("[data-puuid], [data-match], [data-act-id], [data-act], [data-queue], [data-tab]");
    if (!el) return;
    if (el.dataset.puuid) openCareer(el.dataset.puuid);
    else if (el.dataset.tab) setTab(el.dataset.tab);
    else if (el.dataset.match) openMatch(el.dataset.match);
    else if (el.dataset.actId) selectAct(el.dataset.actId);
    else if (el.dataset.act === "cur-act" && app.view?.kind === "career") switchScope(app.view, { actId: null });
    else if (el.dataset.act === "back") goBack();
    else if (el.dataset.act === "result") openResult();
    else if (el.dataset.act === "more") showMoreRows();
    else if (el.dataset.queue && app.view?.kind === "career") {
      const comp = el.dataset.queue === "comp";
      if (comp !== app.view.competitive) switchScope(app.view, { competitive: comp });
    }
  });
  $("#close").addEventListener("click", hideOverlay);
  // Réduire / agrandir, et redimensionner en tirant sur les bords
  $("#compactBtn").addEventListener("click", async () => {
    if (!TAURI) return document.documentElement.classList.toggle("compact");
    const compact = await TAURI.core.invoke("toggle_compact").catch(() => false);
    setCompactIcon(compact);
  });
  document.querySelectorAll("[data-resize]").forEach((el) =>
    el.addEventListener("mousedown", (e) => {
      if (e.button !== 0 || !TAURI) return;
      e.preventDefault();
      TAURI.window.getCurrentWindow().startResizeDragging(el.dataset.resize);
    }),
  );
  $("#view").addEventListener("mousemove", chartHover);
  $("#view").addEventListener("mouseleave", () => document.querySelectorAll(".chart.hover").forEach((c) => c.classList.remove("hover")));
  $("#intro").addEventListener("click", endIntro);
  // Au relâchement : si on masque sur l'appui, le relâchement d'Échap part dans le jeu
  // (qui ouvre alors son menu).
  document.addEventListener("keyup", (e) => {
    if (e.key !== "Escape" || Date.now() - (app.lastEsc || 0) < 250) return;
    app.lastEsc = Date.now();
    if (introPlaying()) endIntro();
    else if (app.view) goBack();
    else hideOverlay();
  });
  // Déplacer la fenêtre en la tenant par l'en-tête.
  $("#top").addEventListener("mousedown", (e) => {
    if (e.button === 0 && TAURI && !e.target.closest("button")) TAURI.window.getCurrentWindow().startDragging();
  });
  document.addEventListener("contextmenu", (e) => e.preventDefault());
}

/* ───────────── Démarrage ───────────── */

async function main() {
  app.assets = await loadAssets();
  bindUi();

  if (TAURI) {
    const { invoke } = TAURI.core;
    const { listen } = TAURI.event;
    app.cfg = await invoke("get_config").catch(() => null);
    invoke("is_compact").then(setCompactIcon).catch(() => {});
    document.documentElement.classList.toggle("no-anim", app.cfg?.animations === false);
    app.snap = await invoke("get_snapshot");
    await listen("snapshot", (e) => {
      const phaseChanged = app.snap?.phase !== e.payload.phase;
      app.snap = e.payload;
      prefetchPlayers(e.payload);
      // Fenêtre masquée : rien à dessiner (économise le CPU/GPU pendant la partie)
      if (!app.visible) {
        app.dirty = true;
        return;
      }
      // La carrière ouverte reste affichée ; seul l'en-tête suit la partie.
      if (app.demo) return;
      if (app.view) renderHeader(app.snap);
      else if (phaseChanged) transition();
      else render();
    });
    // Pseudos d'un match arrivés après son affichage : mise à jour sans animation
    await listen("match-names", (e) => {
      for (const v of [app.view, ...app.stack]) {
        if (v?.kind === "match" && v.id === e.payload.matchId) v.data = e.payload.detail;
      }
      const v = app.view;
      const el = v?.kind === "match" && v.id === e.payload.matchId && $("#mdbody");
      if (el) el.innerHTML = matchBody(v);
    });
    // Partie terminée : écran de fin de partie (tout de suite si l'overlay est ouvert sur la partie)
    app.result = await invoke("get_last_result").catch(() => null);
    await listen("match-result", (e) => {
      app.result = e.payload;
      if (app.demo) return;
      if (app.visible && !app.view) return openResult(e.payload);
      app.resultPending = true;
    });
    await listen("career-progress", (e) => {
      const v = app.view;
      app.careerApplies.get(e.payload.request)?.(e.payload.career);
    });
    await listen("overlay-visibility", (e) => {
      app.visible = !!e.payload;
      if (!e.payload) {
        clearInterval(app.timer);
        return;
      }
      if (app.resultPending && app.result && !app.demo) {
        // Partie terminée pendant que l'overlay était masqué : il s'ouvre sur le résultat
        app.resultPending = false;
        if (app.view?.kind !== "result" || app.view.data.detail.matchId !== app.result.detail.matchId) {
          if (app.view) app.stack.push(app.view);
          app.view = { kind: "result", data: app.result, perspective: app.result.puuid };
          $("#view").scrollTop = 0;
        }
        app.dirty = false;
        render();
        countUp($("#view"));
      } else if (app.dirty) {
        app.dirty = false;
        if (app.view) renderHeader(app.snap);
        else render();
      }
      if (app.launchIntro || introDue()) {
        app.launchIntro = false;
        playIntro();
      } else {
        playWipe();
        playEnter();
      }
    });
    await listen("demo-mode", (e) => {
      app.demo = !!e.payload;
      app.view = null;
      app.stack = [];
      transition();
    });
  } else {
    // Prévisualisation dans un navigateur : données de démo, phase via ?phase=
    app.demo = true;
    app.demoPhase = new URLSearchParams(location.search).get("phase") || "ingame";
  }
  // Aperçu navigateur : ?intro pour rejouer l'intro
  if (!TAURI && new URLSearchParams(location.search).has("intro")) playIntro();
  else transition();

  // Aperçu navigateur (captures d'écran) : ?open=career ou ?open=match ouvre directement la vue
  const open = !TAURI && new URLSearchParams(location.search).get("open");
  if (open === "result") openResult();
  else if (open) {
    const first = currentSnap()?.players.find((p) => p.isMe) || currentSnap()?.players[0];
    if (first) openCareer(first.puuid);
    if (open === "match") {
      const wait = setInterval(() => {
        const id = app.view?.data?.done && app.view.data.matches[0]?.matchId;
        if (!id) return;
        clearInterval(wait);
        openMatch(id);
      }, 200);
    }
  }

  // Démarrage de l'application (après l'avoir quittée) : la fenêtre s'ouvre sur l'intro.
  if (TAURI && introMode() === "launch") {
    app.launchIntro = true;
    TAURI.core.invoke("show_overlay");
  }
}

main();

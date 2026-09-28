"use strict";

// Données fictives pour prévisualiser le design sans être en partie
// (menu de l'icône > « Mode démo », ou ouvrir index.html dans un navigateur).
window.DEMO = {
  /** Générateur pseudo-aléatoire stable (même joueur → mêmes données). */
  _rng(key) {
    let seed = [...key].reduce((s, c) => s + c.charCodeAt(0), 0) || 1;
    return () => ((seed = (seed * 9301 + 49297) % 233280) / 233280);
  },

  _maps(assets) {
    // Cartes de compétition seulement (les cartes sont aussi indexées par identifiant : on ne garde que les chemins)
    return [...(assets?.maps.entries() || [])].filter(([url, m]) => m.strip && url.startsWith("/game/maps/") && !/range|npev2|duel|hurm|abilitydraft|plummet/i.test(url));
  },

  /** Recherche fictive par Riot ID (« inconnu#… » = introuvable). */
  async rank(player) {
    await new Promise((r) => setTimeout(r, 400));
    const rnd = this._rng(player.puuid);
    const live = player.rank && !player.rank.error ? player.rank : null;
    const tier = live?.tier ?? 12 + Math.floor(rnd() * 13);
    const names = ["V24 · ACTE IV", "V24 · ACTE V", "V24 · ACTE VI", "V25 · ACTE I", "V25 · ACTE II", "V25 · ACTE III", "V25 · ACTE IV", "V25 · ACTE V", "V25 · ACTE VI", "V26 · ACTE V"];
    const history = names.map((name, i) => {
      const current = i === names.length - 1;
      const t = current ? tier : Math.max(3, tier - 4 + Math.floor(rnd() * 6));
      const games = current ? live?.games ?? 17 : 15 + Math.floor(rnd() * 70);
      const wins = Math.round(games * (0.42 + rnd() * 0.2));
      const peakT = t + (rnd() > 0.5 ? 1 : 0);
      const w1 = Math.round(wins * 0.3), w3 = peakT > t ? Math.round(wins * 0.15) : 0;
      const winsByTier = [[t - 1, w1], [t, wins - w1 - w3], [peakT, w3]].filter(([, w]) => w > 0);
      return { id: `act-${i}`, name, start: `2025-${String(i + 1).padStart(2, "0")}-01`, tier: t, peak: peakT, games, wins, current, winsByTier };
    });
    const peak = history.reduce((a, b) => (b.peak > a.peak ? b : a));
    return {
      tier, rr: live?.rr ?? Math.floor(rnd() * 100), wins: history.at(-1).wins, games: history.at(-1).games, leaderboard: null,
      actPeakTier: history.at(-1).peak, peakTier: peak.peak, peakAct: peak.name, prevTier: 0, prevAct: null, error: false,
      currentActName: "V26 · ACTE V", history,
      totalGames: history.reduce((s, a) => s + a.games, 0), totalWins: history.reduce((s, a) => s + a.wins, 0),
    };
  },

  /** Carrière fictive sur un acte entier, avec un chargement progressif simulé. */
  async career(assets, player, competitive, actId, onProgress) {
    const rnd = this._rng(player.puuid + (competitive ? "c" : "a") + (actId || "cur"));
    const maps = this._maps(assets);
    const agents = [...(assets?.agents.keys() || [])];
    const main = player.agentId?.toLowerCase() || agents[0];
    const queues = competitive ? ["competitive"] : ["competitive", "unrated", "swiftplay", "competitive"];
    const tierNow = player.rank?.tier || 15;
    const count = 30 + Math.floor(rnd() * 40);
    const matches = Array.from({ length: count }, (_, i) => {
      const won = rnd() > 0.45;
      const us = won ? 13 : Math.floor(rnd() * 12);
      const them = won ? Math.floor(rnd() * 12) : 13;
      const rounds = us + them;
      const kills = Math.round(rounds * (0.55 + rnd() * 0.6));
      const deaths = Math.round(rounds * (0.55 + rnd() * 0.3));
      const queueId = queues[i % queues.length];
      const [mapId] = maps[Math.floor(rnd() * maps.length)] || [""];
      const rr = queueId === "competitive" ? (won ? 14 + Math.round(rnd() * 10) : -(12 + Math.round(rnd() * 8))) : null;
      const acs = Math.round(160 + rnd() * 140);
      return {
        matchId: `demo-${player.puuid}-${actId || "cur"}-${i}`, mapId, queueId, startMs: Date.now() - (i * 9 + 1) * 3_600_000 * (0.4 + rnd()),
        agentId: rnd() > 0.35 ? main : agents[Math.floor(rnd() * agents.length)],
        won, scoreUs: us, scoreThem: them, kills, deaths, assists: Math.round(rnd() * 8),
        acs, adr: Math.round(110 + rnd() * 80), hs: Math.round(14 + rnd() * 22),
        kast: Math.round(58 + rnd() * 25), ddelta: Math.round(-40 + rnd() * 90), rounds,
        firstBloods: Math.floor(rnd() * 5), multikills: Math.floor(rnd() * 4), tier: tierNow,
        mvp: acs > 270, teamMvp: acs > 240,
        rrChange: rr, tierAfter: rr == null ? null : tierNow,
      };
    });

    const build = (list, done) => {
      const sum = (k, l = list) => l.reduce((s, m) => s + (m[k] || 0), 0);
      const avg = (k, l = list) => (l.length ? sum(k, l) / l.length : 0);
      const wins = list.filter((m) => m.won).length;
      const rounds = sum("rounds") || 1;
      const group = (key) => {
        const out = {};
        list.forEach((m) => (out[m[key]] ||= []).push(m));
        return Object.entries(out).sort((a, b) => b[1].length - a[1].length);
      };
      return {
        actId, actName: actId ? "V25 · ACTE VI" : "V26 · ACTE V", actCurrent: !actId,
        playerCard: (assets?.cards || [])[count % Math.max(1, (assets?.cards || []).length)] || null, playerLevel: 40 + count,
        found: count, analyzed: list.length, done, historyLimited: false, partial: false,
        currentTier: tierNow, currentRr: 42,
        matches: list,
        summary: {
          matches: list.length, wins, losses: list.length - wins,
          kills: sum("kills"), deaths: sum("deaths"), assists: sum("assists"),
          rounds, roundsWon: sum("scoreUs"), roundWin: (sum("scoreUs") / rounds) * 100,
          kd: sum("kills") / Math.max(1, sum("deaths")), kad: (sum("kills") + sum("assists")) / Math.max(1, sum("deaths")), kpr: sum("kills") / rounds,
          hs: avg("hs"), adr: avg("adr"), acs: avg("acs"), kast: avg("kast"), ddelta: avg("ddelta"),
          firstBloods: sum("firstBloods"), firstDeaths: Math.round(sum("firstBloods") * 0.8), flawless: Math.round(rounds * 0.06),
          multikills: sum("multikills"), aces: 1, clutches: Math.round(list.length * 0.3), econ: 62 + list.length % 20, mvps: list.filter((m) => m.mvp).length,
          rrNet: sum("rrChange"),
        },
        agents: group("agentId").map(([agentId, l]) => ({
          agentId, matches: l.length, wins: l.filter((m) => m.won).length,
          kd: sum("kills", l) / Math.max(1, sum("deaths", l)), adr: avg("adr", l), acs: avg("acs", l), ddelta: avg("ddelta", l),
          bestMap: l[0].mapId, bestMapWr: Math.round((l.filter((m) => m.won).length / l.length) * 100),
        })),
        maps: group("mapId").map(([mapId, l]) => ({
          mapId, matches: l.length, wins: l.filter((m) => m.won).length,
          kd: sum("kills", l) / Math.max(1, sum("deaths", l)), acs: avg("acs", l), adr: avg("adr", l),
          roundWin: (sum("scoreUs", l) / sum("rounds", l)) * 100,
        })),
      };
    };

    // Actes anciens : Riot ne garde plus leurs matchs (comme en vrai)
    if (actId && Number(actId.split("-")[1]) < 7) {
      await new Promise((r) => setTimeout(r, 500));
      return { ...build([], true), historyLimited: true, historyOldest: Date.now() - 58 * 86_400_000, found: 0, analyzed: 0 };
    }

    // Chargement progressif, comme le backend
    for (let k = 0; k < count; k += 12) {
      onProgress?.(build(matches.slice(0, k), false));
      await new Promise((r) => setTimeout(r, 160));
    }
    return build(matches, true);
  },

  async match(assets, id, perspective) {
    await new Promise((r) => setTimeout(r, 600));
    return this._match(assets, id, perspective);
  },

  /** Écran de fin de partie fictif (toi = « Nova », distinction et MVP). */
  result(assets, snap) {
    const me = snap?.players.find((p) => p.isMe) || { puuid: "demo-a0", agentId: null };
    if (this._result?.puuid === me.puuid) return this._result;
    const d = this._match(assets, "demo-result", me.puuid, { win: true });
    const players = d.teams.flatMap((t) => t.players);
    const line = players.find((p) => p.puuid === me.puuid);
    // Toi en tête du match (MVP) : les autres restent sous ton score
    for (const p of players) Object.assign(p, { mvp: false, acs: Math.min(p.acs, 296) });
    const total = d.rounds.length;
    Object.assign(line, {
      agentId: me.agentId || line.agentId, kills: 27, deaths: 13, assists: 6, acs: 318, adr: 196, hs: 31, kast: 82, ddelta: 48,
      firstBloods: 5, firstDeaths: 2, multikills: 3, aces: 1, clutches: 2, plants: 4, defuses: 1, econ: 88,
      mvp: true, teamMvp: true, medal: "distinction", perf: 448, offense: 462, support: 301, offTrend: "double_up", supTrend: "up",
      offFactors: [["killImpact", "double_up"], ["damage", "double_up"], ["trades", "up"], ["deathImpact", "neutral"]],
      supFactors: [["assists", "neutral"], ["utilityUsage", "up"], ["plants", "up"], ["defuses", "neutral"]],
    });
    line.score = line.acs * total;
    line.damage = line.adr * total;
    this._spread(line, total, this._rng("demo-result-me"));
    // Ton ace et ton clutch
    const won = d.rounds.map((r, i) => (r.winner === "Blue" ? i : -1)).filter((i) => i >= 0);
    const ace = won.reduce((best, i) => (line.roundKills[i] > line.roundKills[best] ? i : best), won[0]);
    line.roundKills[ace] = 5;
    Object.assign(d.rounds[ace], { ceremony: "Ace", player: me.puuid });
    const clutch = d.rounds.findIndex((r, i) => i !== ace && r.winner === "Blue" && i > 3);
    if (clutch >= 0) Object.assign(d.rounds[clutch], { ceremony: "Clutch", player: me.puuid });
    for (const t of d.teams) t.players.sort((a, b) => b.acs - a.acs);
    const endMs = Date.now() - 2 * 60000;
    d.startMs = endMs - d.lengthMs;
    this._result = { puuid: me.puuid, detail: d, rr: { earned: 21, tierBefore: 20, tierAfter: 21, rrBefore: 88, rrAfter: 9, afkPenalty: 0 }, at: endMs };
    return this._result;
  },

  /** Répartit le score et les kills d'un joueur sur les manches. */
  _spread(p, total, rnd) {
    const w = Array.from({ length: total }, () => 0.08 + Math.pow(rnd(), 1.7));
    const sum = w.reduce((a, b) => a + b, 0);
    p.roundScores = w.map((x) => Math.round((x / sum) * p.score));
    p.roundKills = Array(total).fill(0);
    for (let k = 0; k < p.kills; k++) {
      let r = 0;
      let x = rnd() * sum;
      while (r < total - 1 && (x -= w[r]) > 0) r++;
      if (p.roundKills[r] < 4) p.roundKills[r]++;
    }
  },

  _match(assets, id, perspective, { win } = {}) {
    const rnd = this._rng(id);
    const maps = this._maps(assets);
    const agents = [...(assets?.agents.keys() || [])];
    const cards = assets?.cards || [];
    const [mapId] = maps[Math.floor(rnd() * maps.length)] || [""];
    const blueWon = win ?? rnd() > 0.45;
    const loser = 5 + Math.floor(rnd() * 7);
    const [blueR, redR] = blueWon ? [13, loser] : [loser, 13];
    const results = ["Elimination", "Elimination", "Elimination", "Detonate", "Defuse", "Timer"];
    const winners = [...Array(blueR).fill("Blue"), ...Array(redR).fill("Red")].sort(() => rnd() - 0.5);
    // La dernière manche revient au vainqueur.
    const last = blueWon ? "Blue" : "Red";
    const li = winners.lastIndexOf(last);
    [winners[li], winners[winners.length - 1]] = [winners[winners.length - 1], winners[li]];
    const rounds = winners.map((w) => ({ winner: w, result: results[Math.floor(rnd() * results.length)], ceremony: "", player: "" }));
    const names = ["Nova", "Nyx", "Kairo", "Vesper", "mirage", "Tenz0r", "lxrd", "Saphir", "Wildfire", "momo"];
    const total = blueR + redR;
    const players = names.map((name, i) => {
      const kills = Math.round(total * (0.4 + rnd() * 0.8));
      const deaths = Math.round(total * (0.5 + rnd() * 0.3));
      return {
        puuid: i === 0 ? perspective : `demo-m-${id}-${i}`,
        name, tag: ["EUW", "0707", "FR1", "333", "777", "NA1", "EUW", "2525", "000", "uwu"][i],
        agentId: agents[(i * 3 + Math.floor(rnd() * 5)) % Math.max(1, agents.length)],
        tier: 14 + Math.floor(rnd() * 9), level: 20 + Math.floor(rnd() * 300), cardId: cards[i % Math.max(1, cards.length)] || null,
        party: i < 2 ? 0 : i === 5 || i === 6 ? 1 : null,
        acs: Math.round(120 + rnd() * 200), kills, deaths, assists: Math.floor(rnd() * 9),
        adr: Math.round(90 + rnd() * 90), hs: Math.round(12 + rnd() * 25), kast: Math.round(55 + rnd() * 30),
        ddelta: Math.round(-50 + rnd() * 100), firstBloods: Math.floor(rnd() * 5), firstDeaths: Math.floor(rnd() * 5),
        multikills: Math.floor(rnd() * 4), aces: rnd() > 0.93 ? 1 : 0, mvp: false, teamMvp: false,
      };
    });
    // Médailles de fin de partie et score de chaque manche
    const trend = (v) => (v >= 400 ? "double_up" : v >= 300 ? "up" : v >= 200 ? "neutral" : v >= 120 ? "down" : "double_down");
    const near = (v) => trend(Math.max(0, Math.min(500, v + (rnd() - 0.5) * 160)));
    for (const p of players) {
      const clamp = (v) => Math.round(Math.max(20, Math.min(500, v)));
      const offense = clamp((p.kills / total) * 330 + rnd() * 70);
      const support = clamp(p.assists * 28 + 60 + rnd() * 160);
      const perf = clamp(offense * 0.62 + support * 0.38 + (rnd() - 0.5) * 40);
      Object.assign(p, {
        score: p.acs * total, damage: p.adr * total, rounds: total, clutches: rnd() > 0.75 ? 1 : 0,
        plants: Math.floor(rnd() * 4), defuses: Math.floor(rnd() * 2), econ: Math.round(50 + rnd() * 40),
        perf, offense, support, medal: perf >= 420 ? "distinction" : perf >= 330 ? "merit" : "pass",
        offTrend: trend(offense), supTrend: trend(support),
        offFactors: ["damage", "deathImpact", "killImpact", "trades"].map((k) => [k, near(offense)]),
        supFactors: ["assists", "defuses", "plants", "utilityUsage"].map((k) => [k, near(support)]),
      });
      this._spread(p, total, rnd);
    }
    const team = (teamId, list, won, roundsWon) => {
      list.sort((a, b) => b.acs - a.acs);
      list[0].teamMvp = true;
      return { teamId, won, roundsWon, players: list };
    };
    const teams = [team("Blue", players.slice(0, 5), blueWon, blueR), team("Red", players.slice(5), !blueWon, redR)];
    const top = [...players].sort((a, b) => b.acs - a.acs)[0];
    top.mvp = true;
    const perfScale = { avg: 250, max: 500, merit: 330, distinction: 420 };
    return { matchId: id, mapId, queueId: "competitive", actName: "V26 · ACTE V", startMs: Date.now() - 5 * 3_600_000, lengthMs: 38 * 60000, teams, rounds, perfScale };
  },

  snapshot(assets, phase = "ingame") {
    const agents = [...(assets?.agents.values() || [])];
    const pick = (name, i) => (agents.find((a) => a.name.toLowerCase() === name) || agents[i % Math.max(1, agents.length)])?.id;
    const cards = assets?.cards || [];
    const card = (i) => cards[(i * 7) % Math.max(1, cards.length)] || null;
    const r = (tier, rr, wins, games, peakTier, peakAct, extra = {}) => ({
      tier, rr, wins, games, leaderboard: null, peakTier, peakAct, prevTier: 0, prevAct: null, error: false, ...extra,
    });

    const allies = [
      ["Nova", "EUW", "jett", 212, r(21, 64, 18, 31, 22, "V25 · ACTE II"), true, 0],
      ["Nyx", "0707", "omen", 145, r(19, 22, 12, 25, 21, "V25 · ACTE I"), false, 0],
      ["Kairo", "FR1", "sova", 88, r(20, 91, 9, 20, 20, "V26 · ACTE I"), false, null],
      [null, null, "killjoy", 301, r(18, 40, 14, 30, 22, "V25 · ACTE III"), false, null],
      ["mirage", "777", "skye", 57, null, false, null],
    ];
    const enemies = [
      ["Tenz0r", "NA1", "raze", 402, r(24, 187, 41, 70, 25, "V25 · ACTE III", { leaderboard: 812 }), false, 1],
      ["lxrd", "EUW", "viper", 120, r(21, 12, 8, 19, 21, "V26 · ACTE I"), false, 1],
      ["Saphir", "2525", "clove", 33, r(0, 0, 0, 3, 17, "V25 · ACTE I", { prevTier: 17 }), false, null],
      ["Wildfire", "000", "phoenix", 176, r(22, 55, 20, 38, 23, "V25 · ACTE II"), false, null],
      ["momo", "uwu", "cypher", 260, r(20, 77, 11, 27, 21, "V24 · ACTE III"), false, null],
    ];

    const mk = (row, i, isAlly) => ({
      puuid: `demo-${isAlly ? "a" : "e"}${i}`,
      name: row[0],
      tag: row[1],
      incognito: !row[0],
      team: isAlly ? "Blue" : "Red",
      isMe: row[5],
      isAlly,
      agentId: pick(row[2], i),
      agentState: i < 3 ? "locked" : i === 3 ? "selected" : "",
      level: row[3],
      cardId: card(i + (isAlly ? 0 : 5)),
      party: row[6],
      // Groupe adverse : déduit du dernier match (Riot ne montre que tes amis)
      partyGuess: !isAlly && row[6] != null,
      rank: row[4],
    });

    const base = { message: null, mapId: "/Game/Maps/Ascent/Ascent", queueId: "competitive", provisioningFlow: "Matchmaking", actName: "V26 · ACTE II", phaseEndsAt: null };

    if (phase === "pregame") {
      return { ...base, phase, players: allies.map((row, i) => mk(row, i, true)) };
    }
    if (phase === "menus") {
      return { ...base, phase, mapId: null, players: allies.slice(0, 3).map((row, i) => ({ ...mk(row, i, true), party: null })) };
    }
    if (phase === "waiting" || phase === "offline") {
      return { ...base, phase, mapId: null, players: [] };
    }
    return {
      ...base,
      phase: "ingame",
      players: [...allies.map((row, i) => ({ ...mk(row, i, true), agentState: "locked" })), ...enemies.map((row, i) => ({ ...mk(row, i, false), agentState: "locked" }))],
    };
  },
};

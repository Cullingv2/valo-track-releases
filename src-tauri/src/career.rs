//! Carrière d'un joueur : statistiques détaillées sur un acte entier (K/D/A, HS %, ADR, ACS,
//! KAST, DDΔ, first bloods, aces, manches flawless, stats par agent et par carte), et détail
//! complet d'un match (les 10 joueurs, manche par manche).
//!
//! Chaque match n'est téléchargé qu'une fois : il est gardé en mémoire et sur disque, et sert
//! pour les 10 joueurs qui y ont participé. Les matchs d'un acte sont analysés en parallèle et
//! l'interface reçoit les résultats au fur et à mesure (événement `career-progress`).

use crate::directory::{Directory, Known};
use crate::henrik::Henrik;
use crate::riot::Riot;
use crate::tracker::{ActWindow, Phase, Shared};
use futures_util::{stream, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter, State};

/// Riot limite l'historique à 20 entrées par requête.
const PAGE: usize = 20;
/// Au-delà, on s'arrête (un acte dépasse rarement ce nombre de parties).
const MAX_MATCHES: usize = 2000;
/// Hors acte connu : nombre de matchs récents analysés.
const FALLBACK_MATCHES: usize = 20;
/// Téléchargements de matchs en parallèle (une moitié chez Riot, l'autre sur le serveur relais).
const PARALLEL: usize = 6;
/// Modes sans vraies manches (ou parties personnalisées) : exclus des statistiques,
/// sinon l'ACS, l'ADR ou les scores n'ont plus de sens (un match à mort = 1 « manche »).
const EXCLUDED_QUEUES: &[&str] = &["deathmatch", "hurm", "ggteam", "snowball", "abilitydraftarena", ""];
/// Patience maximale pour un même appel quand Riot limite les requêtes : ensuite, on passe au suivant.
const PATIENCE: Duration = Duration::from_secs(10 * 60);
/// Premier affichage d'une carrière : on n'attend pas Riot plus que ça (le reste suit en fond).
const FIRST_PASS: Duration = Duration::from_millis(2500);
/// Budgets de temps d'un chargement de carrière : quoi qu'il arrive (Riot qui limite, quota du
/// relais épuisé), les stats s'affichent au bout de ~30 s au plus avec ce qui est disponible.
const SUMMARY_BUDGET: Duration = Duration::from_secs(5);
const HISTORY_RETRY_BUDGET: Duration = Duration::from_secs(8);
const DETAILS_BUDGET: Duration = Duration::from_secs(7);
/// Plus aucun match reçu depuis ce délai (et la majorité prête) : on affiche sans attendre la fin.
const DETAILS_STALL: Duration = Duration::from_millis(1200);
/// Attente maximale des matchs sans résumé avant d'afficher les stats principales
const MAIN_BUDGET: Duration = Duration::from_millis(1500);
/// Pages d'historique parcourues au plus (20 matchs par page).
const MAX_PAGES: usize = 150;
/// Sans session Riot, détails de match demandés à HenrikDev (stats avancées) : les plus récents.
const OFFLINE_DETAILS: usize = 15;

/// Chargements de carrière en cours
static ACTIVE_LOADS: AtomicU32 = AtomicU32::new(0);
/// Préchargement en cours (une nouvelle partie remplace le précédent)
static PREFETCH_GEN: AtomicU32 = AtomicU32::new(0);
/// Matchs préchargés au plus par joueur de la partie (les plus récents d'abord)
const PREFETCH_MATCHES: usize = 60;

fn round_based(queue: &str) -> bool {
    !EXCLUDED_QUEUES.contains(&queue)
}

/// Dossier des matchs analysés. Changé quand le calcul des stats évolue : les matchs sont alors
/// réanalysés (l'ancien dossier `matches` reste en secours pour ceux que Riot ne fournit plus).
const MATCH_DIR: &str = "matches-v2";

/// Une mort est « échangée » si le tueur meurt dans les 5 s.
const TRADE_WINDOW_MS: u64 = 5000;

/// Présence d'un joueur dans un match (index local).
#[derive(Serialize, Deserialize, Clone, Copy)]
struct Seen {
    start: u64,
    comp: bool,
}

/// Cache et archive locale : les matchs analysés (un fichier par match) et, pour chaque joueur,
/// la liste des matchs où il a été vu. Riot ne garde que les matchs récents : grâce à cette
/// archive, les stats d'un acte restent consultables après leur disparition chez Riot.
pub struct CareerCache {
    matches: Mutex<HashMap<String, Arc<ParsedMatch>>>,
    index: Mutex<HashMap<String, HashMap<String, Seen>>>,
    dirty: Mutex<HashSet<String>>,
    /// Téléchargements de matchs simultanés, tous chargements confondus
    net: tokio::sync::Semaphore,
    /// Résumés HenrikDev par joueur / acte / mode : (date, matchs, début de période atteint)
    quick: Mutex<HashMap<String, (Instant, Arc<Vec<QuickMatch>>, bool)>>,
    /// Dossier racine du cache (`matches/` et `players/`)
    dir: Option<PathBuf>,
    /// Pages d'historique Riot récentes (changement d'acte ou de mode instantané)
    pages: Mutex<HashMap<String, (Instant, Value)>>,
    /// Données publiques (sans le jeu) et joueurs connus
    henrik: Arc<Henrik>,
    people: Arc<Directory>,
    /// Dernière partie terminée (écran de fin de partie) et match en cours de récupération
    last_result: Mutex<Option<MatchResult>>,
    /// Match guetté : (identifiant, fin de l'attente, résultat livré)
    result_for: Mutex<Option<(String, Instant, bool)>>,
}

impl CareerCache {
    pub fn new(dir: Option<PathBuf>, henrik: Arc<Henrik>, people: Arc<Directory>) -> Self {
        if let Some(d) = &dir {
            let _ = std::fs::create_dir_all(d.join(MATCH_DIR));
            let _ = std::fs::create_dir_all(d.join("players"));
        }
        Self {
            matches: Mutex::new(HashMap::new()),
            index: Mutex::new(HashMap::new()),
            dirty: Mutex::new(HashSet::new()),
            net: tokio::sync::Semaphore::new(PARALLEL),
            quick: Mutex::new(HashMap::new()),
            dir,
            pages: Mutex::new(HashMap::new()),
            henrik,
            people,
            last_result: Mutex::new(None),
            result_for: Mutex::new(None),
        }
    }

    /// Page d'historique Riot (match-history, competitiveupdates), mémorisée 2 minutes.
    async fn pd_page(&self, riot: &Riot, path: &str, patience: Duration) -> anyhow::Result<Option<Value>> {
        const TTL: Duration = Duration::from_secs(120);
        if let Some((at, v)) = self.pages.lock().unwrap().get(path) {
            if at.elapsed() < TTL {
                return Ok(Some(v.clone()));
            }
        }
        let v = riot.pd_patient(path, patience).await?;
        if let Some(v) = &v {
            let mut pages = self.pages.lock().unwrap();
            pages.retain(|_, (at, _)| at.elapsed() < TTL);
            pages.insert(path.to_string(), (Instant::now(), v.clone()));
        }
        Ok(v)
    }

    fn match_path(&self, id: &str) -> Option<PathBuf> {
        Some(self.dir.as_ref()?.join(MATCH_DIR).join(format!("{id}.json")))
    }

    /// Analyse faite par une version précédente : utilisée seulement si Riot ne fournit plus le match.
    fn get_legacy(&self, id: &str) -> Option<Arc<ParsedMatch>> {
        let path = self.dir.as_ref()?.join("matches").join(format!("{id}.json"));
        let p: ParsedMatch = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
        self.note_match(id, &p);
        Some(Arc::new(p))
    }

    fn player_path(&self, puuid: &str) -> Option<PathBuf> {
        Some(self.dir.as_ref()?.join("players").join(format!("{puuid}.json")))
    }

    fn get(&self, id: &str) -> Option<Arc<ParsedMatch>> {
        if let Some(p) = self.matches.lock().unwrap().get(id).cloned() {
            return Some(p);
        }
        let p: ParsedMatch = serde_json::from_slice(&std::fs::read(self.match_path(id)?).ok()?).ok()?;
        self.note_match(id, &p);
        let p = Arc::new(p);
        self.matches.lock().unwrap().insert(id.to_string(), p.clone());
        Some(p)
    }

    fn has(&self, id: &str) -> bool {
        self.matches.lock().unwrap().contains_key(id) || self.match_path(id).is_some_and(|p| p.exists())
    }

    fn put(&self, id: &str, p: ParsedMatch) -> Arc<ParsedMatch> {
        if let Some(path) = self.match_path(id) {
            if let Ok(json) = serde_json::to_vec(&p) {
                let _ = std::fs::write(path, json);
            }
        }
        self.note_match(id, &p);
        let p = Arc::new(p);
        self.matches.lock().unwrap().insert(id.to_string(), p.clone());
        p
    }

    /// Index d'un joueur, chargé depuis le disque au premier accès.
    fn with_player<R>(&self, puuid: &str, f: impl FnOnce(&mut HashMap<String, Seen>) -> R) -> R {
        let mut index = self.index.lock().unwrap();
        let entry = index.entry(puuid.to_string()).or_insert_with(|| {
            self.player_path(puuid)
                .and_then(|p| std::fs::read(p).ok())
                .and_then(|b| serde_json::from_slice(&b).ok())
                .unwrap_or_default()
        });
        f(entry)
    }

    fn note(&self, puuid: &str, match_id: &str, seen: Seen) {
        let added = self.with_player(puuid, |m| m.insert(match_id.to_string(), seen).is_none());
        if added {
            self.dirty.lock().unwrap().insert(puuid.to_string());
        }
    }

    /// Un match analysé renseigne l'index des 10 joueurs.
    fn note_match(&self, id: &str, p: &ParsedMatch) {
        let seen = Seen { start: p.start_ms, comp: p.queue_id == "competitive" };
        for puuid in p.players.keys() {
            self.note(puuid, id, seen);
        }
    }

    /// Matchs connus localement pour ce joueur sur la période, et date du plus ancien.
    fn known(&self, puuid: &str, window: Option<&ActWindow>, comp_only: bool) -> (Vec<(String, u64)>, Option<u64>) {
        self.with_player(puuid, |m| {
            let relevant = m.iter().filter(|(_, s)| !comp_only || s.comp);
            let oldest = relevant.clone().map(|(_, s)| s.start).min();
            let list = match window {
                Some(w) => relevant.filter(|(_, s)| s.start >= w.start && s.start < w.end).map(|(id, s)| (id.clone(), s.start)).collect(),
                None => Vec::new(),
            };
            (list, oldest)
        })
    }

    /// Écrit sur disque les index modifiés.
    fn flush(&self) {
        let dirty: Vec<String> = self.dirty.lock().unwrap().drain().collect();
        let index = self.index.lock().unwrap();
        for puuid in dirty {
            if let (Some(path), Some(m)) = (self.player_path(&puuid), index.get(&puuid)) {
                if let Ok(json) = serde_json::to_vec(m) {
                    let _ = std::fs::write(path, json);
                }
            }
        }
    }
}

/// Archive automatiquement tes matchs (toutes files) : au démarrage puis toutes les 20 min,
/// jamais pendant une partie, une requête par seconde au plus.
pub fn spawn_archiver(riot: Arc<Riot>, cache: Arc<CareerCache>, shared: Arc<Shared>) {
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_secs(45)).await;
        loop {
            if let Err(e) = archive_once(&riot, &cache, &shared).await {
                eprintln!("archivage : {e:#}");
            }
            cache.flush();
            tokio::time::sleep(Duration::from_secs(20 * 60)).await;
        }
    });
}

fn in_match(shared: &Shared) -> bool {
    matches!(shared.phase(), Phase::Pregame | Phase::Ingame)
}

async fn archive_once(riot: &Riot, cache: &CareerCache, shared: &Shared) -> anyhow::Result<()> {
    let me = riot.puuid();
    if me.is_empty() || in_match(shared) {
        return Ok(());
    }
    let mut ids = Vec::new();
    let mut start = 0;
    for _ in 0..MAX_PAGES {
        let url = format!("/match-history/v1/history/{me}?startIndex={start}&endIndex={}", start + PAGE);
        let Some(v) = riot.pd_patient(&url, PATIENCE).await? else { break };
        let items = v["History"].as_array().cloned().unwrap_or_default();
        for h in &items {
            if let Some(id) = h["MatchID"].as_str() {
                let seen = Seen { start: h["GameStartTime"].as_u64().unwrap_or(0), comp: h["QueueID"] == "competitive" };
                cache.note(&me, id, seen);
                ids.push(id.to_string());
            }
        }
        start += PAGE;
        let total = v["Total"].as_u64().unwrap_or(0) as usize;
        if items.is_empty() || (total > 0 && start >= total) || (total == 0 && items.len() < PAGE) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    for id in ids.iter().filter(|id| !cache.has(id)) {
        while in_match(shared) {
            tokio::time::sleep(Duration::from_secs(60)).await;
        }
        let _ = match_cached(riot, cache, id, "", false).await;
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    Ok(())
}

#[derive(Serialize, Deserialize, Clone, Default)]
struct ParsedMatch {
    map_id: String,
    queue_id: String,
    season_id: String,
    start_ms: u64,
    length_ms: u64,
    /// équipe → (gagnée, manches gagnées)
    teams: HashMap<String, (bool, u32)>,
    players: HashMap<String, PlayerStats>,
    rounds: Vec<RoundInfo>,
    /// MVP officiels (Riot) : du match, et de chaque équipe
    #[serde(default)]
    mvp: String,
    #[serde(default)]
    team_mvps: HashMap<String, String>,
    /// Barème du score de performance (médailles)
    #[serde(default)]
    perf_scale: Option<PerfScale>,
    /// Version de l'analyse Riot (`PARSE_REV`) ; 0 = HenrikDev ou analyse sans médailles
    #[serde(default)]
    rev: u32,
}

/// Analyse Riot avec médailles, score de performance et score par manche.
const PARSE_REV: u32 = 2;

/// Barème du score de performance de fin de partie (0 à `max`, moyenne `avg`).
#[derive(Serialize, Deserialize, Clone, Copy, Default, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PerfScale {
    pub avg: f32,
    pub max: f32,
    pub merit: f32,
    pub distinction: f32,
}

#[derive(Default, Clone, Serialize, Deserialize)]
#[serde(default)]
struct PlayerStats {
    team: String,
    agent: String,
    name: String,
    tag: String,
    tier: u32,
    level: u32,
    card: String,
    party_id: String,
    kills: u32,
    deaths: u32,
    assists: u32,
    score: u32,
    rounds: u32,
    rounds_won: u32,
    damage: u32,
    damage_taken: u32,
    head: u32,
    body: u32,
    leg: u32,
    kast_rounds: u32,
    first_bloods: u32,
    first_deaths: u32,
    /// manches à 3 ennemis tués ou plus
    multikills: u32,
    aces: u32,
    /// manches gagnées en clutch (reconnues par Riot)
    clutches: u32,
    flawless: u32,
    /// crédits dépensés (pour l'Econ rating)
    spent: u32,
    /// Médaille de fin de partie (« distinction », « merit », « pass »), vide si inconnue
    medal: String,
    /// Score de performance (0 à 500) et ses deux volets, attaque et soutien
    perf: f32,
    offense: f32,
    support: f32,
    /// Tendances attaque / soutien (« double_up », « up », « neutral », « down », « double_down »)
    off_trend: String,
    sup_trend: String,
    /// Facteurs détaillés : (facteur, tendance)
    off_factors: Vec<(String, String)>,
    sup_factors: Vec<(String, String)>,
    /// Score de combat et kills de chaque manche
    round_scores: Vec<u32>,
    round_kills: Vec<u32>,
    plants: u32,
    defuses: u32,
    /// Ligne provisoire (résumé HenrikDev) : pas de stats avancées
    #[serde(skip)]
    light: bool,
    /// Manches selon le résumé (base de l'ACS et du K/manche affichés d'emblée), 0 = celles du détail
    #[serde(skip)]
    summary_rounds: u32,
}

impl PlayerStats {
    fn acs(&self) -> u32 {
        div_round(self.score, self.rounds)
    }
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct RoundInfo {
    pub winner: String,
    /// "Elimination", "Defuse", "Detonate", "Timer", "Surrender"…
    pub result: String,
    /// Cérémonie de la manche : "Ace", "Clutch", "Flawless", "Thrifty", "TeamAce", "Closer"…
    #[serde(default)]
    pub ceremony: String,
    /// Joueur de la cérémonie (ace, clutch)
    #[serde(default)]
    pub player: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Career {
    pub matches: Vec<CareerMatch>,
    pub summary: Summary,
    pub agents: Vec<AgentStat>,
    pub maps: Vec<MapStat>,
    /// Acte couvert (None = derniers matchs, acte inconnu)
    pub act_id: Option<String>,
    pub act_name: Option<String>,
    pub act_current: bool,
    /// Matchs de la période trouvés dans l'historique / déjà analysés
    pub found: u32,
    pub analyzed: u32,
    /// Vrai quand l'analyse est terminée
    pub done: bool,
    /// L'historique Riot s'arrête avant le début de l'acte (matchs anciens indisponibles)
    pub history_limited: bool,
    /// Date du plus ancien match disponible pour ce joueur (Riot + archive locale)
    pub history_oldest: Option<u64>,
    /// Bannière et niveau du joueur (lus dans son match le plus récent)
    pub player_card: Option<String>,
    pub player_level: Option<u32>,
    /// Rang et RR après le dernier match classé (utile quand le rang actuel n'est pas connu).
    pub current_tier: Option<u32>,
    pub current_rr: Option<u32>,
    /// Stats principales prêtes (liste des matchs complète) : affichables tout de suite, elles ne
    /// changent plus ; seules les stats détaillées arrivent ensuite (`done`).
    pub main_ready: bool,
    /// Matchs dont le détail n'est pas encore arrivé : téléchargés en arrière-plan ensuite
    #[serde(skip)]
    pub pending: Vec<String>,
    /// Vrai si des matchs n'ont pas pu être lus (liste partielle)
    pub partial: bool,
    /// Matchs illisibles malgré les nouvelles tentatives
    pub failed: u32,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct CareerMatch {
    pub match_id: String,
    pub map_id: String,
    pub queue_id: String,
    pub start_ms: u64,
    pub agent_id: String,
    /// `None` = égalité ou mode sans équipes
    pub won: Option<bool>,
    pub score_us: u32,
    pub score_them: u32,
    pub kills: u32,
    pub deaths: u32,
    pub assists: u32,
    pub acs: u32,
    pub adr: u32,
    pub hs: u32,
    pub kast: u32,
    pub ddelta: i32,
    pub first_bloods: u32,
    pub multikills: u32,
    /// Rang du joueur pendant ce match
    pub tier: u32,
    /// Meilleur ACS du match / de son équipe
    pub mvp: bool,
    pub team_mvp: bool,
    pub rr_change: Option<i32>,
    pub tier_after: Option<u32>,
    /// Résumé seulement (détail en cours de téléchargement)
    pub light: bool,
}

#[derive(Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Summary {
    pub matches: u32,
    pub wins: u32,
    pub losses: u32,
    pub kills: u32,
    pub deaths: u32,
    pub assists: u32,
    pub rounds: u32,
    pub rounds_won: u32,
    pub kd: f32,
    pub kad: f32,
    pub kpr: f32,
    pub hs: f32,
    pub adr: f32,
    pub acs: f32,
    pub kast: f32,
    pub ddelta: f32,
    pub round_win: f32,
    pub first_bloods: u32,
    pub first_deaths: u32,
    pub flawless: u32,
    pub multikills: u32,
    pub aces: u32,
    pub clutches: u32,
    /// Econ rating : dégâts infligés pour 1 000 crédits dépensés
    pub econ: f32,
    pub mvps: u32,
    /// Matchs dont le détail est chargé (base des stats avancées)
    pub advanced_matches: u32,
    pub rr_net: i32,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentStat {
    pub agent_id: String,
    pub matches: u32,
    pub wins: u32,
    pub kd: f32,
    pub adr: f32,
    pub acs: f32,
    pub ddelta: f32,
    pub best_map: Option<String>,
    pub best_map_wr: u32,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MapStat {
    pub map_id: String,
    pub matches: u32,
    pub wins: u32,
    pub kd: f32,
    pub acs: f32,
    pub adr: f32,
    pub round_win: f32,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct MatchDetail {
    pub match_id: String,
    pub map_id: String,
    pub queue_id: String,
    pub act_name: Option<String>,
    pub start_ms: u64,
    pub length_ms: u64,
    pub teams: Vec<TeamLine>,
    pub rounds: Vec<RoundInfo>,
    /// Pseudos en cours de récupération (événement `match-names` à l'arrivée)
    pub names_pending: bool,
    /// Barème des médailles (absent : match sans médailles)
    pub perf_scale: Option<PerfScale>,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct TeamLine {
    pub team_id: String,
    pub won: bool,
    pub rounds_won: u32,
    pub players: Vec<PlayerLine>,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct PlayerLine {
    pub puuid: String,
    pub name: Option<String>,
    pub tag: Option<String>,
    pub agent_id: String,
    pub tier: u32,
    pub level: Option<u32>,
    pub card_id: Option<String>,
    /// Numéro de groupe (≥ 2 joueurs ensemble dans ce match)
    pub party: Option<u32>,
    pub acs: u32,
    pub kills: u32,
    pub deaths: u32,
    pub assists: u32,
    pub adr: u32,
    pub hs: u32,
    pub kast: u32,
    pub ddelta: i32,
    pub first_bloods: u32,
    pub first_deaths: u32,
    pub multikills: u32,
    pub aces: u32,
    pub mvp: bool,
    pub team_mvp: bool,
    /// Score de combat total, dégâts infligés et manches jouées
    pub score: u32,
    pub damage: u32,
    pub rounds: u32,
    pub clutches: u32,
    pub plants: u32,
    pub defuses: u32,
    /// Econ rating (dégâts pour 1000 crédits dépensés)
    pub econ: u32,
    pub medal: Option<String>,
    pub perf: Option<u32>,
    pub offense: Option<u32>,
    pub support: Option<u32>,
    pub off_trend: Option<String>,
    pub sup_trend: Option<String>,
    pub off_factors: Vec<(String, String)>,
    pub sup_factors: Vec<(String, String)>,
    pub round_scores: Vec<u32>,
    pub round_kills: Vec<u32>,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct Progress<'a> {
    request: u32,
    career: &'a Career,
}

/// Carrière sur un acte (`act_id`, par défaut l'acte en cours). Les résultats partiels sont
/// envoyés à l'interface pendant l'analyse ; la valeur de retour est le résultat final.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn get_career(
    puuid: String,
    competitive: bool,
    act_id: Option<String>,
    request: u32,
    app: AppHandle,
    riot: State<'_, Arc<Riot>>,
    cache: State<'_, Arc<CareerCache>>,
    shared: State<'_, Arc<Shared>>,
) -> Result<Career, String> {
    let region = cache.people.region_for(&puuid, &riot.region());
    // Juste après le lancement, la liste des actes peut ne pas être encore chargée : on l'attend
    // un peu plutôt que de retomber sur les « derniers matchs ».
    let mut window = shared.act_window(act_id.as_deref());
    for _ in 0..30 {
        if window.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
        window = shared.act_window(act_id.as_deref());
    }
    let emit = |c: &Career| {
        let _ = app.emit("career-progress", Progress { request, career: c });
    };
    let label = window.as_ref().map_or("derniers matchs".to_string(), |w| w.name.clone());
    let started = Instant::now();
    ACTIVE_LOADS.fetch_add(1, Ordering::Relaxed);
    let res = load(&riot, &cache, &puuid, competitive, window, &region, emit).await;
    ACTIVE_LOADS.fetch_sub(1, Ordering::Relaxed);
    let line = match &res {
        Ok(c) => format!(
            "carrière {} ({label}, {}{}) : {} matchs trouvés, {} analysés, {} retenus, {:.1} s{}{}",
            &puuid[..8.min(puuid.len())],
            if competitive { "compétition" } else { "tous modes" },
            if riot.connected() { "" } else { ", sans session Riot" },
            c.found,
            c.analyzed,
            c.matches.len(),
            started.elapsed().as_secs_f32(),
            if c.partial { ", résultat partiel" } else { "" },
            if c.history_limited { ", historique Riot incomplet pour cette période" } else { "" },
        ),
        Err(e) => format!("carrière {} : erreur {e:#}", &puuid[..8.min(puuid.len())]),
    };
    crate::tracker::diag(&app, &line);
    if let Ok(c) = &res {
        if !c.pending.is_empty() {
            let (riot, cache, pending) = (riot.inner().clone(), cache.inner().clone(), c.pending.clone());
            tauri::async_runtime::spawn(async move {
                let _ = stream::iter(pending.into_iter().enumerate())
                    .map(|(i, id)| {
                        let (riot, cache, region) = (&riot, &cache, &region);
                        async move { match_cached(riot, cache, &id, region, i % 2 == 1).await }
                    })
                    .buffer_unordered(3)
                    .collect::<Vec<_>>()
                    .await;
                cache.flush();
            });
        }
    }
    res.map_err(|e| format!("{e:#}"))
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct MatchNames<'a> {
    match_id: &'a str,
    detail: &'a MatchDetail,
}

/// Détail d'un match. `puuid` : joueur depuis lequel il est ouvert (sa région, sans le jeu).
/// Riot ne donne plus les pseudos dans le détail des matchs : ils sont complétés par HenrikDev.
/// Si HenrikDev tarde (quota de la minute occupé par un chargement de carrière), le match
/// s'affiche tout de suite et les pseudos suivent (événement `match-names`).
#[tauri::command]
pub async fn get_match(
    match_id: String,
    puuid: Option<String>,
    app: AppHandle,
    riot: State<'_, Arc<Riot>>,
    cache: State<'_, Arc<CareerCache>>,
    shared: State<'_, Arc<Shared>>,
) -> Result<MatchDetail, String> {
    let region = cache.people.region_for(puuid.as_deref().unwrap_or(""), &riot.region());
    let parsed = match match_cached(&riot, &cache, &match_id, &region, false).await {
        Ok(Some(p)) => p,
        Ok(None) => return Err("match introuvable".into()),
        Err(e) => return Err(format!("{e:#}")),
    };
    let act = shared.act_name(&parsed.season_id);
    let mut first = detail(&match_id, &parsed, act.clone());
    fill_tiers(&mut first, &shared, &cache.people);
    let medals = parsed.rev < PARSE_REV && riot.connected();
    let names = parsed.players.values().any(|p| p.name.is_empty()) && cache.henrik.available();
    if !medals && !names {
        return Ok(first);
    }
    // Médailles (match analysé par une ancienne version) et pseudos (HenrikDev) en arrière-plan :
    // inclus s'ils arrivent en moins de 0,6 s, sinon le match s'affiche et ils suivent (`match-names`).
    let (riot2, cache2, shared2, app2, id) = (riot.inner().clone(), cache.inner().clone(), shared.inner().clone(), app.clone(), match_id.clone());
    let mut task = tauri::async_runtime::spawn(async move {
        let mut p = parsed;
        if medals {
            p = with_medals(&riot2, &cache2, &id, p).await;
        }
        if names {
            match fill_names(&cache2, &id, &region, &p).await {
                Some(named) => p = named,
                None => crate::tracker::diag(&app2, &format!("pseudos du match {} : HenrikDev ne les a pas fournis", &id[..8.min(id.len())])),
            }
        }
        let mut d = detail(&id, &p, act);
        fill_tiers(&mut d, &shared2, &cache2.people);
        d
    });
    match tokio::time::timeout(Duration::from_millis(600), &mut task).await {
        Ok(Ok(d)) => Ok(d),
        Ok(Err(_)) => Ok(first),
        Err(_) => {
            first.names_pending = names;
            tauri::async_runtime::spawn(async move {
                if let Ok(d) = task.await {
                    let _ = app.emit("match-names", MatchNames { match_id: &match_id, detail: &d });
                }
            });
            Ok(first)
        }
    }
}

/// Parties non classées et combat à mort : Riot n'y donne pas le rang des joueurs (0). On affiche
/// alors leur rang actuel s'il est connu (lu pendant la session, ou joueur déjà croisé).
fn fill_tiers(d: &mut MatchDetail, shared: &Shared, people: &Directory) {
    if d.queue_id == "competitive" {
        return;
    }
    for p in d.teams.iter_mut().flat_map(|t| t.players.iter_mut()).filter(|p| p.tier == 0) {
        p.tier = match shared.live_tier(&p.puuid) {
            0 => people.get(&p.puuid).map_or(0, |k| k.tier),
            t => t,
        };
    }
}

/// Match analysé avant l'arrivée des médailles (ou venu de HenrikDev) : relu une fois chez Riot,
/// pseudos conservés. Sans réponse rapide de Riot, l'ancienne analyse est affichée.
async fn with_medals(riot: &Riot, cache: &CareerCache, id: &str, old: Arc<ParsedMatch>) -> Arc<ParsedMatch> {
    if old.rev >= PARSE_REV || !riot.connected() {
        return old;
    }
    let path = format!("/match-details/v1/matches/{id}");
    let Ok(Ok(Some(v))) = tokio::time::timeout(Duration::from_secs(4), riot.pd(&path)).await else { return old };
    let mut fresh = parse_match(&v);
    if fresh.players.is_empty() {
        return old;
    }
    for (puuid, p) in fresh.players.iter_mut().filter(|(_, p)| p.name.is_empty()) {
        if let Some(o) = old.players.get(puuid) {
            p.name = o.name.clone();
            p.tag = o.tag.clone();
        }
    }
    cache.put(id, fresh)
}

/// Groupes probables d'une partie en cours. Riot ne montre les groupes que de tes amis ; mais un
/// joueur qui était dans le même groupe qu'un coéquipier actuel lors de son dernier match joue
/// presque toujours encore avec lui (mesuré sur de vrais matchs : 12 groupes justes, 0 faux).
/// Chaque joueur est analysé l'un après l'autre (dernier match chez Riot, sinon HenrikDev) et le
/// résultat est ajouté au fur et à mesure : la boucle de suivi l'affiche à son passage suivant.
pub fn spawn_party_hints(app: AppHandle, riot: Arc<Riot>, cache: Arc<CareerCache>, shared: Arc<Shared>, match_id: String, puuids: Vec<String>) {
    tauri::async_runtime::spawn(async move {
        let started = Instant::now();
        let mut linked = 0;
        let mut failed = 0;
        for puuid in &puuids {
            let mates = last_party(&riot, &cache, puuid).await;
            match &mates {
                Some(m) => linked += (!m.is_empty()) as u32,
                None => failed += 1,
            }
            shared.set_party_hint(&match_id, puuid, mates);
        }
        crate::tracker::diag(
            &app,
            &format!("groupes probables : {} joueur(s) analysé(s) en {:.1} s, {linked} en groupe au dernier match, {failed} sans réponse", puuids.len(), started.elapsed().as_secs_f32()),
        );
    });
}

/// Coéquipiers de groupe d'un joueur lors de son dernier match (vide : il jouait seul).
async fn last_party(riot: &Riot, cache: &CareerCache, puuid: &str) -> Option<Vec<String>> {
    let region = cache.people.region_for(puuid, &riot.region());
    let path = format!("/match-history/v1/history/{puuid}?startIndex=0&endIndex=1");
    let mut id = match cache.pd_page(riot, &path, Duration::from_secs(3)).await {
        Ok(Some(v)) => v["History"][0]["MatchID"].as_str().map(String::from),
        _ => None,
    };
    if id.is_none() && cache.henrik.available() {
        // Riot limite les requêtes : dernier match connu de HenrikDev (relais)
        let v = cache.henrik.try_get(&format!("valorant/v1/by-puuid/stored-matches/{region}/{puuid}?size=3")).await;
        id = v.and_then(|v| {
            let list = v["data"].as_array()?.clone();
            let newest = list.iter().max_by_key(|m| m["meta"]["started_at"].as_str().and_then(crate::tracker::iso_ms).unwrap_or(0))?;
            newest["meta"]["id"].as_str().map(String::from)
        });
    }
    let id = id?;
    let m = tokio::time::timeout(Duration::from_secs(25), match_cached(riot, cache, &id, &region, false)).await.ok()?.ok()??;
    let party = &m.players.get(puuid)?.party_id;
    if party.is_empty() {
        return Some(Vec::new());
    }
    Some(m.players.iter().filter(|(q, p)| *q != puuid && p.party_id == *party).map(|(q, _)| q.clone()).collect())
}

/// Écran de fin de partie : le match qui vient de se terminer, vu par le joueur.
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct MatchResult {
    pub puuid: String,
    pub detail: MatchDetail,
    /// Évolution du classement (parties classées)
    pub rr: Option<RrChange>,
    /// Arrivée du résultat (ms)
    pub at: u64,
}

#[derive(Serialize, Clone, Default, Debug)]
#[serde(rename_all = "camelCase")]
pub struct RrChange {
    pub earned: i32,
    pub tier_before: u32,
    pub tier_after: u32,
    pub rr_before: u32,
    pub rr_after: u32,
    pub afk_penalty: i32,
}

#[tauri::command]
pub fn get_last_result(cache: State<'_, Arc<CareerCache>>) -> Option<MatchResult> {
    cache.last_result.lock().unwrap().clone()
}

/// Fin d'une partie : Riot publie le détail du match quelques secondes à quelques minutes plus
/// tard. On le guette en arrière-plan (une requête toutes les 5 s), puis l'écran de fin de partie
/// est envoyé à l'interface (événement `match-result`). `names` : pseudos vus pendant la partie
/// (`None` = joueur en mode incognito, qui le reste).
pub fn spawn_result(app: AppHandle, riot: Arc<Riot>, cache: Arc<CareerCache>, shared: Arc<Shared>, match_id: String, names: HashMap<String, Option<(String, String)>>) {
    const WATCH: Duration = Duration::from_secs(300);
    {
        let mut current = cache.result_for.lock().unwrap();
        if let Some((_, until, delivered)) = current.as_mut().filter(|(id, _, _)| *id == match_id) {
            // Déjà livré, ou guet en cours (fausse fin de partie plus tôt : on prolonge l'attente)
            if !*delivered {
                *until = Instant::now() + WATCH;
            }
            return;
        }
        *current = Some((match_id.clone(), Instant::now() + WATCH, false));
    }
    tauri::async_runtime::spawn(async move {
        let me = riot.puuid();
        let path = format!("/match-details/v1/matches/{match_id}");
        let started = Instant::now();
        let mut parsed = None;
        let watching = || cache.result_for.lock().unwrap().as_ref().is_some_and(|(id, until, _)| *id == match_id && Instant::now() < *until);
        while watching() {
            if let Ok(Some(v)) = riot.pd(&path).await {
                let p = parse_match(&v);
                if p.players.contains_key(&me) {
                    parsed = Some(cache.put(&match_id, p));
                    break;
                }
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
        let short = &match_id[..8.min(match_id.len())];
        let Some(parsed) = parsed else {
            crate::tracker::diag(&app, &format!("fin de partie {short} : détail du match indisponible"));
            // Nouvel essai possible si la même partie est de nouveau vue terminée
            let mut current = cache.result_for.lock().unwrap();
            if current.as_ref().is_some_and(|(id, _, _)| *id == match_id) {
                *current = None;
            }
            return;
        };
        // Historique récent désormais périmé : la carrière doit montrer ce match
        cache.pages.lock().unwrap().clear();
        let mut named = (*parsed).clone();
        for (puuid, p) in named.players.iter_mut() {
            match names.get(puuid) {
                Some(Some((name, tag))) => {
                    p.name = name.clone();
                    p.tag = tag.clone();
                }
                Some(None) if *puuid != me => {
                    p.name.clear();
                    p.tag.clear();
                }
                _ => {}
            }
        }
        let rr = if parsed.queue_id == "competitive" { rr_change(&riot, &me, &match_id).await } else { None };
        let mut detail = detail(&match_id, &named, shared.act_name(&parsed.season_id));
        fill_tiers(&mut detail, &shared, &cache.people);
        let at = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
        let medal = named.players.get(&me).map(|p| p.medal.clone()).unwrap_or_default();
        let result = MatchResult { puuid: me, detail, rr, at };
        crate::tracker::diag(
            &app,
            &format!(
                "fin de partie {short} : résultat prêt en {:.0} s (médaille {}, RR {})",
                started.elapsed().as_secs_f32(),
                if medal.is_empty() { "—" } else { medal.as_str() },
                result.rr.as_ref().map_or("—".to_string(), |r| format!("{:+}", r.earned)),
            ),
        );
        *cache.last_result.lock().unwrap() = Some(result.clone());
        if let Some((_, _, delivered)) = cache.result_for.lock().unwrap().as_mut() {
            *delivered = true;
        }
        let _ = app.emit("match-result", &result);
    });
}

/// Points de classement gagnés ou perdus sur ce match (publiés par Riot peu après le match).
async fn rr_change(riot: &Riot, puuid: &str, match_id: &str) -> Option<RrChange> {
    let path = format!("/mmr/v1/players/{puuid}/competitiveupdates?startIndex=0&endIndex=5&queue=competitive");
    for _ in 0..12 {
        if let Ok(Some(v)) = riot.pd(&path).await {
            if let Some(m) = v["Matches"].as_array().into_iter().flatten().find(|m| m["MatchID"].as_str() == Some(match_id)) {
                let n = |k: &str| m[k].as_i64().unwrap_or(0);
                return Some(RrChange {
                    earned: n("RankedRatingEarned") as i32,
                    tier_before: n("TierBeforeUpdate") as u32,
                    tier_after: n("TierAfterUpdate") as u32,
                    rr_before: n("RankedRatingBeforeUpdate") as u32,
                    rr_after: n("RankedRatingAfterUpdate") as u32,
                    afk_penalty: n("AFKPenalty") as i32,
                });
            }
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
    None
}

/// Pseudos d'un match lu chez Riot, complétés par HenrikDev et gardés en cache.
async fn fill_names(cache: &CareerCache, id: &str, region: &str, parsed: &ParsedMatch) -> Option<Arc<ParsedMatch>> {
    let named = henrik_match(cache, id, region).await?;
    let mut merged = parsed.clone();
    for (puuid, p) in merged.players.iter_mut() {
        if let Some(n) = named.players.get(puuid).filter(|n| !n.name.is_empty()) {
            p.name = n.name.clone();
            p.tag = n.tag.clone();
        }
    }
    let merged = cache.put(id, merged);
    cache.people.save();
    Some(merged)
}

/// Détails d'un match : mémoire, puis disque, sinon serveurs Riot (avec nouvel essai si limité),
/// sinon HenrikDev (jeu fermé, ou match que Riot ne fournit plus).
/// `relay_first` : demander d'abord au serveur relais (sans attendre son quota), puis à Riot.
async fn match_cached(riot: &Riot, cache: &CareerCache, id: &str, region: &str, relay_first: bool) -> anyhow::Result<Option<Arc<ParsedMatch>>> {
    if let Some(p) = cache.get(id) {
        return Ok(Some(p));
    }
    let _permit = cache.net.acquire().await?;
    if let Some(p) = cache.get(id) {
        return Ok(Some(p)); // téléchargé entre-temps par un autre chargement
    }
    if relay_first && !region.is_empty() && cache.henrik.available() {
        if let Some(p) = henrik_match_with(cache, id, region, false).await {
            return Ok(Some(cache.put(id, p)));
        }
    }
    let mut error = None;
    if riot.connected() {
        let path = format!("/match-details/v1/matches/{id}");
        if region.is_empty() || !cache.henrik.available() {
            // Archiveur / préchargement : Riot seul, en patientant (aucun quota relais consommé).
            match riot.pd_patient(&path, PATIENCE).await {
                Ok(Some(v)) => return Ok(Some(cache.put(id, parse_match(&v)))),
                Ok(None) => {}
                Err(e) => error = Some(e),
            }
        } else {
            // Riot puis relais, tour à tour, sans jamais attendre l'un des deux : le premier qui
            // répond l'emporte (avant, un refus de Riot bloquait ce match jusqu'à 60 s et plus).
            let deadline = Instant::now() + PATIENCE;
            let mut delay = Duration::from_millis(1500);
            let mut riot_missing = false;
            let mut failures = 0;
            loop {
                if !riot_missing {
                    match riot.pd(&path).await {
                        Ok(Some(v)) => return Ok(Some(cache.put(id, parse_match(&v)))),
                        Ok(None) => riot_missing = true,
                        Err(e) => {
                            if !e.is::<crate::riot::RateLimited>() {
                                failures += 1;
                            }
                            error = Some(e);
                        }
                    }
                }
                if let Some(p) = henrik_match_with(cache, id, region, false).await {
                    return Ok(Some(cache.put(id, p)));
                }
                if riot_missing || failures >= 3 || Instant::now() > deadline {
                    break;
                }
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(Duration::from_secs(8));
            }
            if riot_missing {
                error = None;
            }
        }
    }
    if let Some(p) = cache.get_legacy(id) {
        return Ok(Some(p));
    }
    // L'archiveur (région vide) ne consomme pas le quota HenrikDev.
    if !region.is_empty() && cache.henrik.available() {
        if let Some(p) = henrik_match(cache, id, region).await {
            return Ok(Some(cache.put(id, p)));
        }
    }
    match error {
        Some(e) => Err(e),
        None if riot.connected() || cache.henrik.available() => Ok(None),
        None => Err(anyhow::anyhow!("ouvre le client Riot pour charger ce match")),
    }
}

/// Match complet via HenrikDev, converti au format interne ; les 10 pseudos sont retenus.
async fn henrik_match(cache: &CareerCache, id: &str, region: &str) -> Option<ParsedMatch> {
    henrik_match_with(cache, id, region, true).await
}

async fn henrik_match_with(cache: &CareerCache, id: &str, region: &str, wait: bool) -> Option<ParsedMatch> {
    let path = format!("valorant/v4/match/{region}/{id}");
    let v = if wait { cache.henrik.get(&path).await? } else { cache.henrik.try_get(&path).await? };
    let d = &v["data"];
    let p = parse_henrik_match(d)?;
    let region = d["metadata"]["region"].as_str().unwrap_or(region).to_lowercase();
    for (puuid, s) in &p.players {
        cache.people.remember(Known {
            puuid: puuid.clone(),
            name: s.name.clone(),
            tag: s.tag.clone(),
            card_id: s.card.clone(),
            tier: s.tier,
            level: s.level,
            region: region.clone(),
            seen: 0,
        });
    }
    Some(p)
}

/// Matchs de la période, du plus récent au plus ancien : historique Riot + archive locale.
/// Renvoie aussi vrai si des matchs de la période manquent (trop anciens), et la date du plus
/// ancien match disponible pour ce joueur.
async fn history_ids(
    riot: &Riot,
    cache: &CareerCache,
    puuid: &str,
    competitive: bool,
    window: Option<&ActWindow>,
    ranked: &RankedHistory,
    patience: Duration,
) -> (Vec<String>, bool, Option<u64>, bool) {
    let h = riot_history(riot, cache, puuid, competitive, window, patience).await;
    merge_history(cache, puuid, competitive, window, h, ranked)
}

/// Pages match-history de Riot sur la période.
struct RiotHistory {
    found: HashMap<String, u64>,
    reached_start: bool,
    exhausted: bool,
    busy: bool,
    oldest: Option<u64>,
}

async fn riot_history(riot: &Riot, cache: &CareerCache, puuid: &str, competitive: bool, window: Option<&ActWindow>, patience: Duration) -> RiotHistory {
    let queue = if competitive { "&queue=competitive" } else { "" };
    let limit = if window.is_some() { MAX_MATCHES } else { FALLBACK_MATCHES };
    let mut found: HashMap<String, u64> = HashMap::new();
    let mut start = 0;
    let mut reached_start = false;
    let mut exhausted = false;
    let mut busy = false;
    let mut riot_oldest: Option<u64> = None;
    // Au plus 20 pages : de quoi remonter plusieurs actes en arrière.
    for _ in 0..MAX_PAGES {
        let url = format!("/match-history/v1/history/{puuid}?startIndex={start}&endIndex={}{queue}", start + PAGE);
        let v = match cache.pd_page(riot, &url, patience).await {
            Ok(Some(v)) => v,
            Ok(None) => {
                exhausted = true;
                break;
            }
            // Riot indisponible ou qui limite : archive locale et HenrikDev en attendant
            Err(_) => {
                busy = riot.connected();
                break;
            }
        };
        let items = v["History"].as_array().cloned().unwrap_or_default();
        let total = v["Total"].as_u64().unwrap_or(0) as usize;
        for h in &items {
            let (Some(id), t) = (h["MatchID"].as_str(), h["GameStartTime"].as_u64().unwrap_or(0)) else { continue };
            cache.note(puuid, id, Seen { start: t, comp: h["QueueID"] == "competitive" });
            riot_oldest = Some(riot_oldest.map_or(t, |o| o.min(t)));
            if let Some(w) = window {
                if t < w.start {
                    reached_start = true;
                    break;
                }
                if t >= w.end {
                    continue;
                }
            }
            // Tous modes : seulement les modes à manches (pas téléchargés sinon)
            if !competitive && !round_based(h["QueueID"].as_str().unwrap_or("")) {
                continue;
            }
            found.insert(id.to_string(), t);
        }
        start += PAGE;
        // Fin de l'historique : page vide, ou total annoncé atteint (une page courte ne suffit pas)
        exhausted = items.is_empty() || (total > 0 && start >= total) || (total == 0 && items.len() < PAGE);
        if reached_start || exhausted || found.len() >= limit {
            break;
        }
    }
    RiotHistory { found, reached_start, exhausted, busy, oldest: riot_oldest }
}

/// Liste finale : historique Riot + archive locale + historique de rang et résumés HenrikDev.
fn merge_history(cache: &CareerCache, puuid: &str, competitive: bool, window: Option<&ActWindow>, h: RiotHistory, ranked: &RankedHistory) -> (Vec<String>, bool, Option<u64>, bool) {
    let limit = if window.is_some() { MAX_MATCHES } else { FALLBACK_MATCHES };
    let RiotHistory { mut found, reached_start, exhausted, busy, oldest: riot_oldest } = h;
    // Archive locale : matchs plus anciens que ceux que Riot garde encore.
    let (local, local_oldest) = cache.known(puuid, window, competitive);
    for (id, t) in local {
        found.entry(id).or_insert(t);
    }
    // Parties connues par l'historique de rang et HenrikDev (plus profonds que match-history)
    for (id, t) in &ranked.matches {
        cache.note(puuid, id, Seen { start: *t, comp: competitive });
        found.entry(id.clone()).or_insert(*t);
    }
    cache.flush();

    let oldest = [riot_oldest.filter(|_| exhausted), local_oldest, ranked.oldest].into_iter().flatten().min();
    let limited = window.is_some_and(|w| !reached_start && !ranked.reached_start && oldest.is_none_or(|o| o > w.start));
    let mut ids: Vec<(String, u64)> = found.into_iter().collect();
    ids.sort_by(|a, b| b.1.cmp(&a.1));
    ids.truncate(limit);
    (ids.into_iter().map(|(id, _)| id).collect(), limited, oldest, busy)
}

/// Historique des mises à jour de rang (compétition).
struct RankedHistory {
    /// match → (RR gagnés, rang après le match)
    rr: HashMap<String, (i32, u32)>,
    /// Parties classées de la période : (match, début). Cet historique remonte plus loin que
    /// match-history (plafonné à ~100 matchs tous modes) : ses matchs complètent la liste.
    matches: Vec<(String, u64)>,
    /// Rang et RR après le dernier match classé
    current: Option<(u32, u32)>,
    /// L'historique atteint le début de la période
    reached_start: bool,
    oldest: Option<u64>,
    /// Riot a limité ou refusé une page (liste incomplète)
    busy: bool,
}

/// Résumé d'un match fourni par HenrikDev : les stats principales du joueur, sans le détail
/// des manches. Suffit pour afficher immédiatement K/D, ACS, ADR, HS %, winrate, agents, cartes…
/// (valeurs identiques au détail Riot, vérifié sur des matchs réels).
#[derive(Clone)]
struct QuickMatch {
    id: String,
    start: u64,
    /// uuid de la carte (l'interface sait aussi l'afficher)
    map: String,
    queue: String,
    agent: String,
    tier: u32,
    level: u32,
    score: u32,
    kills: u32,
    deaths: u32,
    assists: u32,
    head: u32,
    body: u32,
    leg: u32,
    damage: u32,
    damage_taken: u32,
    rounds_us: u32,
    rounds_them: u32,
}

/// Nom de mode HenrikDev → identifiant de file Riot.
fn henrik_queue(mode: &str) -> String {
    let m = mode.trim().to_lowercase();
    match m.as_str() {
        "competitive" => "competitive",
        "unrated" => "unrated",
        "swiftplay" => "swiftplay",
        "spike rush" | "spikerush" => "spikerush",
        "premier" => "premier",
        "replication" => "onefa",
        "new map" => "newmap",
        "deathmatch" => "deathmatch",
        "team deathmatch" => "hurm",
        "escalation" => "ggteam",
        "snowball fight" => "snowball",
        _ if m.contains("custom") || m.is_empty() => "",
        _ => return m.replace(' ', ""),
    }
    .to_string()
}

fn henrik_round_based(mode: &str) -> bool {
    round_based(&henrik_queue(mode))
}

/// Résumés HenrikDev des matchs de la période (mémorisés 10 min : préchargement et réouverture
/// instantanés). Renvoie aussi vrai si la liste atteint le début de la période.
async fn henrik_matches(cache: &CareerCache, region: &str, puuid: &str, competitive: bool, window: &ActWindow) -> (Arc<Vec<QuickMatch>>, bool) {
    let memo_key = format!("{puuid}|{}|{competitive}", window.id);
    if let Some((at, list, reached)) = cache.quick.lock().unwrap().get(&memo_key) {
        if at.elapsed() < Duration::from_secs(600) {
            return (list.clone(), *reached);
        }
    }
    let region = if region.is_empty() { "eu" } else { region };
    let mode = if competitive { "&mode=competitive" } else { "" };
    let num = |x: &Value| x.as_u64().unwrap_or(0) as u32;
    let mut out = Vec::new();
    let mut reached = false;
    let mut complete = true;
    let fetch = |page: usize| {
        let path = format!("valorant/v1/by-puuid/stored-matches/{region}/{puuid}?page={page}&size=25{mode}");
        async move { cache.henrik.get(&path).await }
    };
    // Page 1, puis les suivantes par lots de 4 en parallèle (un gros joueur a 10 pages et plus)
    let mut pages: std::collections::VecDeque<Option<Value>> = std::collections::VecDeque::from([fetch(1).await]);
    let total_pages = pages[0].as_ref().map_or(1, |v| (v["results"]["total"].as_u64().unwrap_or(0) as usize).div_ceil(25).clamp(1, 80));
    let mut next_page = 2;
    loop {
        let Some(page) = pages.pop_front() else {
            if next_page > total_pages || reached {
                break;
            }
            let batch: Vec<usize> = (next_page..=(next_page + 3).min(total_pages)).collect();
            next_page += batch.len();
            pages.extend(futures_util::future::join_all(batch.into_iter().map(fetch)).await);
            continue;
        };
        let Some(v) = page else {
            complete = false;
            break;
        };
        let items = v["data"].as_array().cloned().unwrap_or_default();
        if items.is_empty() {
            break;
        }
        for m in &items {
            let Some(t) = m["meta"]["started_at"].as_str().and_then(crate::tracker::iso_ms) else { continue };
            if t < window.start {
                reached = true;
                break;
            }
            let mode = m["meta"]["mode"].as_str().unwrap_or("");
            if t >= window.end || (!competitive && !henrik_round_based(mode)) {
                continue;
            }
            let (Some(id), s) = (m["meta"]["id"].as_str(), &m["stats"]) else { continue };
            let team = s["team"].as_str().unwrap_or("").to_lowercase();
            let (red, blue) = (num(&m["teams"]["red"]), num(&m["teams"]["blue"]));
            let (us, them) = if team == "blue" { (blue, red) } else { (red, blue) };
            out.push(QuickMatch {
                id: id.to_string(),
                start: t,
                map: m["meta"]["map"]["id"].as_str().unwrap_or("").to_lowercase(),
                queue: henrik_queue(mode),
                agent: s["character"]["id"].as_str().unwrap_or("").to_lowercase(),
                tier: num(&s["tier"]),
                level: num(&s["level"]),
                score: num(&s["score"]),
                kills: num(&s["kills"]),
                deaths: num(&s["deaths"]),
                assists: num(&s["assists"]),
                head: num(&s["shots"]["head"]),
                body: num(&s["shots"]["body"]),
                leg: num(&s["shots"]["leg"]),
                damage: num(&s["damage"]["made"]),
                damage_taken: num(&s["damage"]["received"]),
                rounds_us: us,
                rounds_them: them,
            });
        }
        if reached || v["results"]["after"].as_u64().unwrap_or(0) == 0 {
            break;
        }
    }
    let list = Arc::new(out);
    if complete {
        cache.quick.lock().unwrap().insert(memo_key, (Instant::now(), list.clone(), reached));
    }
    (list, reached)
}

/// Ligne « match » provisoire à partir du résumé HenrikDev (remplacée par le détail Riot dès
/// qu'il est téléchargé ; les stats avancées ne viennent que du détail).
fn quick_row(q: &QuickMatch, rr: &HashMap<String, (i32, u32)>, competitive: bool) -> Option<(CareerMatch, PlayerStats)> {
    let rounds = q.rounds_us + q.rounds_them;
    if (competitive && q.queue != "competitive") || !round_based(&q.queue) || rounds < 3 {
        return None;
    }
    let (rr_change, tier_after) = match rr.get(&q.id) {
        Some((e, t)) if q.queue == "competitive" => (Some(*e), Some(*t)),
        _ => (None, None),
    };
    let stats = PlayerStats {
        agent: q.agent.clone(),
        tier: q.tier,
        level: q.level,
        kills: q.kills,
        deaths: q.deaths,
        assists: q.assists,
        score: q.score,
        rounds,
        rounds_won: q.rounds_us,
        damage: q.damage,
        damage_taken: q.damage_taken,
        head: q.head,
        body: q.body,
        leg: q.leg,
        light: true,
        ..Default::default()
    };
    Some((
        CareerMatch {
            match_id: q.id.clone(),
            map_id: q.map.clone(),
            queue_id: q.queue.clone(),
            start_ms: q.start,
            agent_id: q.agent.clone(),
            won: if q.rounds_us == q.rounds_them { None } else { Some(q.rounds_us > q.rounds_them) },
            score_us: q.rounds_us,
            score_them: q.rounds_them,
            kills: q.kills,
            deaths: q.deaths,
            assists: q.assists,
            acs: div_round(q.score, rounds),
            adr: div_round(q.damage, rounds),
            hs: pct(q.head, q.head + q.body + q.leg) as u32,
            kast: 0,
            ddelta: ((q.damage as f64 - q.damage_taken as f64) / rounds as f64).round() as i32,
            first_bloods: 0,
            multikills: 0,
            tier: q.tier,
            mvp: false,
            team_mvp: false,
            rr_change,
            tier_after,
            light: true,
        },
        stats,
    ))
}

/// Préchargement (sélection d'agents / partie en cours) : résumés de l'acte des joueurs, en
/// arrière-plan, sans gêner un chargement de carrière en cours.
#[tauri::command]
pub async fn prefetch_players(
    puuids: Vec<String>,
    riot: State<'_, Arc<Riot>>,
    cache: State<'_, Arc<CareerCache>>,
    shared: State<'_, Arc<Shared>>,
) -> Result<(), String> {
    let Some(window) = shared.act_window(None) else { return Ok(()) };
    if !cache.henrik.available() {
        return Ok(());
    }
    let (riot, cache) = (riot.inner().clone(), cache.inner().clone());
    let generation = PREFETCH_GEN.fetch_add(1, Ordering::Relaxed) + 1;
    let current = move || PREFETCH_GEN.load(Ordering::Relaxed) == generation;
    tauri::async_runtime::spawn(async move {
        let wait_idle = || async {
            while ACTIVE_LOADS.load(Ordering::Relaxed) > 0 {
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        };
        // 1. Résumés de l'acte (relais) : la liste des matchs de chaque joueur
        let mut lists = Vec::new();
        for puuid in &puuids {
            wait_idle().await;
            if !current() {
                return;
            }
            let region = cache.people.region_for(puuid, &riot.region());
            let (list, _) = henrik_matches(&cache, &region, puuid, true, &window).await;
            lists.push(list);
        }
        // 2. Détail de ces matchs chez Riot, à petit rythme, pendant la partie : la carrière d'un
        // joueur de la partie s'ouvre ensuite instantanément avec ses stats complètes.
        tokio::time::sleep(Duration::from_secs(6)).await;
        let mut round = 0;
        while round < PREFETCH_MATCHES {
            let mut any = false;
            for list in &lists {
                let Some(q) = list.get(round) else { continue };
                any = true;
                wait_idle().await;
                if !current() || !riot.connected() {
                    return;
                }
                if cache.has(&q.id) {
                    continue;
                }
                let _ = match_cached(&riot, &cache, &q.id, "", false).await;
                tokio::time::sleep(Duration::from_millis(350)).await;
            }
            if !any {
                break;
            }
            round += 1;
        }
        cache.flush();
    });
    Ok(())
}

/// Gains / pertes de RR par match sur la période, et rang + RR après le dernier match classé.
async fn rr_changes(riot: &Riot, cache: &CareerCache, puuid: &str, window: Option<&ActWindow>, want: usize, patience: Duration) -> RankedHistory {
    let mut busy = false;
    let mut rr = HashMap::new();
    let mut matches = Vec::new();
    let mut current = None;
    let mut reached_start = false;
    let mut oldest: Option<u64> = None;
    let mut start = 0;
    for _ in 0..MAX_PAGES {
        let url = format!("/mmr/v1/players/{puuid}/competitiveupdates?startIndex={start}&endIndex={}&queue=competitive", start + PAGE);
        let v = match cache.pd_page(riot, &url, patience).await {
            Ok(Some(v)) => v,
            Ok(None) => break,
            Err(_) => {
                busy = riot.connected();
                break;
            }
        };
        let items = v["Matches"].as_array().cloned().unwrap_or_default();
        let mut past = false;
        for m in &items {
            let tier = m["TierAfterUpdate"].as_u64().unwrap_or(0) as u32;
            if current.is_none() {
                current = Some((tier, m["RankedRatingAfterUpdate"].as_u64().unwrap_or(0) as u32));
            }
            let t = m["MatchStartTime"].as_u64().unwrap_or(0);
            oldest = Some(oldest.map_or(t, |o| o.min(t)));
            if window.is_some_and(|w| t < w.start) {
                past = true;
                reached_start = true;
                break;
            }
            if let Some(id) = m["MatchID"].as_str() {
                rr.insert(id.to_string(), (m["RankedRatingEarned"].as_i64().unwrap_or(0) as i32, tier));
                if window.is_none_or(|w| t < w.end) {
                    matches.push((id.to_string(), t));
                }
            }
        }
        start += PAGE;
        if past || items.is_empty() || (window.is_none() && start >= want) {
            break;
        }
    }
    RankedHistory { rr, matches, current, reached_start, oldest, busy }
}

struct LoadState<'a> {
    window: Option<&'a ActWindow>,
    found: u32,
    analyzed: u32,
    done: bool,
    main_ready: bool,
    partial: bool,
    failed: u32,
    limited: bool,
    oldest: Option<u64>,
    current: Option<(u32, u32)>,
}

fn build(rows: &[(CareerMatch, PlayerStats)], s: &LoadState) -> Career {
    Career {
        matches: rows.iter().map(|(m, _)| m.clone()).collect(),
        summary: summarize(rows),
        agents: agent_stats(rows),
        maps: map_stats(rows),
        act_id: s.window.map(|w| w.id.clone()),
        act_name: s.window.map(|w| w.name.clone()),
        act_current: s.window.is_some_and(|w| w.current),
        found: s.found,
        analyzed: s.analyzed,
        done: s.done,
        history_limited: s.limited,
        history_oldest: s.oldest,
        player_card: rows.first().map(|(_, p)| p.card.clone()).filter(|c| !c.is_empty()),
        player_level: rows.first().map(|(_, p)| p.level).filter(|l| *l > 0),
        current_tier: s.current.map(|c| c.0),
        current_rr: s.current.map(|c| c.1),
        main_ready: s.main_ready || s.done,
        pending: Vec::new(),
        partial: s.partial,
        failed: s.failed,
    }
}

async fn load(
    riot: &Riot,
    cache: &CareerCache,
    puuid: &str,
    competitive: bool,
    window: Option<ActWindow>,
    region: &str,
    emit: impl Fn(&Career),
) -> anyhow::Result<Career> {
    // 1. Listes de matchs : historique de rang Riot et résumés HenrikDev, en parallèle
    let quick_fut = async {
        match (cache.henrik.available(), window.as_ref()) {
            (true, Some(w)) => tokio::time::timeout(SUMMARY_BUDGET, henrik_matches(cache, region, puuid, competitive, w))
                .await
                .unwrap_or_else(|_| (Arc::new(Vec::new()), false)),
            _ => (Arc::new(Vec::new()), false),
        }
    };
    // Riot n'est attendu que quelques secondes : s'il limite les requêtes (pendant une partie,
    // l'overlay lit déjà les rangs des 10 joueurs), la carrière s'affiche avec le serveur relais et
    // l'archive locale, puis l'historique Riot est repris en fond et complète la liste.
    let (ranked, (quick, quick_reached), riot_hist) = tokio::join!(
        rr_changes(riot, cache, puuid, window.as_ref(), FALLBACK_MATCHES, FIRST_PASS),
        quick_fut,
        riot_history(riot, cache, puuid, competitive, window.as_ref(), FIRST_PASS)
    );
    let quick_by_id: HashMap<&str, &QuickMatch> = quick.iter().map(|q| (q.id.as_str(), q)).collect();
    let with_quick = |mut ranked: RankedHistory| {
        for q in quick.iter() {
            ranked.oldest = Some(ranked.oldest.map_or(q.start, |o| o.min(q.start)));
            ranked.matches.push((q.id.clone(), q.start));
        }
        ranked.reached_start |= quick_reached;
        ranked
    };
    let ranked = with_quick(ranked);
    let (mut ids, limited, oldest, busy) = merge_history(cache, puuid, competitive, window.as_ref(), riot_hist, &ranked);
    let riot_busy = busy || ranked.busy;
    let mut rr = ranked.rr;

    let mut state = LoadState {
        window: window.as_ref(),
        found: ids.len() as u32,
        analyzed: 0,
        done: false,
        main_ready: false,
        partial: false,
        failed: 0,
        limited,
        oldest,
        current: ranked.current,
    };

    // 2. Affichage immédiat : détail déjà en cache, sinon résumé HenrikDev
    let mut rows: HashMap<String, (CareerMatch, PlayerStats)> = HashMap::new();
    let mut unknown = Vec::new(); // ni détail ni résumé : à télécharger en priorité
    let mut upgrade = Vec::new(); // résumé seulement : détail pour les stats avancées
    let mut classify = |ids: &[String], rr: &HashMap<String, (i32, u32)>, rows: &mut HashMap<String, (CareerMatch, PlayerStats)>, state: &mut LoadState| {
        rows.clear();
        unknown.clear();
        upgrade.clear();
        state.analyzed = 0;
        for id in ids {
            if let Some(parsed) = cache.get(id) {
                state.analyzed += 1;
                if let Some(mut row) = career_row(id, &parsed, puuid, rr, competitive) {
                    if let Some(q) = quick_by_id.get(id.as_str()) {
                        keep_summary(&mut row, q);
                    }
                    rows.insert(id.clone(), row);
                }
            } else if let Some(q) = quick_by_id.get(id.as_str()) {
                if let Some(row) = quick_row(q, rr, competitive) {
                    rows.insert(id.clone(), row);
                    upgrade.push(id.clone());
                }
            } else {
                unknown.push(id.clone());
            }
        }
    };
    let ordered = |ids: &[String], rows: &HashMap<String, (CareerMatch, PlayerStats)>| -> Vec<(CareerMatch, PlayerStats)> {
        ids.iter().filter_map(|id| rows.get(id).cloned()).collect()
    };
    classify(&ids, &rr, &mut rows, &mut state);
    emit(&build(&ordered(&ids, &rows), &state));

    // 2 bis. Riot limitait et rien d'autre n'est connu : on reprend son historique en patientant un peu.
    if riot_busy && ids.is_empty() {
        let ranked = with_quick(rr_changes(riot, cache, puuid, window.as_ref(), FALLBACK_MATCHES, HISTORY_RETRY_BUDGET).await);
        let (ids2, limited2, oldest2, _) = history_ids(riot, cache, puuid, competitive, window.as_ref(), &ranked, HISTORY_RETRY_BUDGET).await;
        if !ranked.rr.is_empty() {
            rr = ranked.rr;
            state.current = ranked.current.or(state.current);
        }
        if ids2.len() >= ids.len() {
            ids = ids2;
            state.limited = limited2;
            state.oldest = oldest2;
        }
        state.found = ids.len() as u32;
        classify(&ids, &rr, &mut rows, &mut state);
        emit(&build(&ordered(&ids, &rows), &state));
    }

    // 3. Détails Riot en arrière-plan : d'abord les matchs inconnus, puis les stats avancées.
    // Sans session Riot, seuls les plus récents sont demandés à HenrikDev (quota partagé).
    let mut queue: Vec<String> = unknown.iter().chain(upgrade.iter()).cloned().collect();
    if !riot.connected() {
        queue.truncate(OFFLINE_DETAILS);
    }
    let queued = queue.clone();
    let mut received: HashSet<String> = HashSet::new();
    let mut last_progress = tokio::time::Instant::now();
    state.found = state.analyzed + queue.len() as u32;
    // Un match sur deux est demandé d'abord au relais : deux sources en parallèle, deux fois plus vite.
    let mut results = stream::iter(queue.into_iter().enumerate())
        .map(|(i, id)| async move {
            let r = match_cached(riot, cache, &id, region, i % 2 == 1).await;
            (id, r)
        })
        .buffer_unordered(PARALLEL);
    let mut last_emit = Instant::now();
    let deadline = tokio::time::Instant::now() + DETAILS_BUDGET;
    // Matchs sans résumé : il faut leur détail pour que la liste (et donc les stats principales)
    // soit complète. Au-delà de MAIN_BUDGET, on affiche quand même.
    let mut missing: HashSet<String> = unknown.iter().cloned().collect();
    let main_deadline = tokio::time::Instant::now() + MAIN_BUDGET;
    state.main_ready = missing.is_empty();
    if state.main_ready {
        emit(&build(&ordered(&ids, &rows), &state));
    }
    loop {
        let mut limit = if state.main_ready { deadline } else { main_deadline.min(deadline) };
        // Les derniers matchs traînent (Riot limite, quota du relais) : on n'attend plus qu'eux
        // quand la majorité est prête ; ils finissent de se télécharger en arrière-plan.
        if state.main_ready && received.len() * 10 >= queued.len() * 6 {
            limit = limit.min(last_progress + DETAILS_STALL);
        }
        let Ok(next) = tokio::time::timeout_at(limit, results.next()).await else {
            if !state.main_ready && tokio::time::Instant::now() < deadline {
                state.main_ready = true;
                emit(&build(&ordered(&ids, &rows), &state));
                continue;
            }
            // Budget écoulé : on affiche avec les matchs prêts (les autres gardent leur résumé).
            state.partial = true;
            break;
        };
        let Some((id, res)) = next else { break };
        state.analyzed += 1;
        missing.remove(&id);
        received.insert(id.clone());
        last_progress = tokio::time::Instant::now();
        match res {
            Ok(Some(parsed)) => match career_row(&id, &parsed, puuid, &rr, competitive) {
                Some(mut row) => {
                    if let Some(q) = quick_by_id.get(id.as_str()) {
                        keep_summary(&mut row, q);
                    }
                    rows.insert(id, row);
                }
                None => {
                    rows.remove(&id);
                }
            },
            Ok(None) => {}
            Err(e) => {
                eprintln!("match {id} : {e:#}");
                state.failed += 1;
            }
        }
        if !state.main_ready && missing.is_empty() {
            state.main_ready = true;
            emit(&build(&ordered(&ids, &rows), &state));
            last_emit = Instant::now();
        } else if last_emit.elapsed() > Duration::from_millis(400) {
            emit(&build(&ordered(&ids, &rows), &state));
            last_emit = Instant::now();
        }
    }
    state.done = true;
    let mut career = build(&ordered(&ids, &rows), &state);
    career.pending = queued.into_iter().filter(|id| !received.contains(id)).collect();
    Ok(career)
}

/// Stats principales d'un match reprises du résumé HenrikDev : elles sont affichées dès
/// l'ouverture de la carrière et ne doivent plus changer quand le détail arrive (le détail ne
/// fournit que les stats avancées).
fn keep_summary(row: &mut (CareerMatch, PlayerStats), q: &QuickMatch) {
    let (m, p) = row;
    let rounds = q.rounds_us + q.rounds_them;
    if rounds < 3 {
        return;
    }
    p.kills = q.kills;
    p.deaths = q.deaths;
    p.assists = q.assists;
    p.score = q.score;
    p.rounds_won = q.rounds_us;
    // Le détail garde ses propres manches (base des stats avancées : ADR, KAST…)
    m.kills = q.kills;
    m.deaths = q.deaths;
    m.assists = q.assists;
    m.acs = div_round(q.score, rounds);
    m.score_us = q.rounds_us;
    m.score_them = q.rounds_them;
    m.won = if q.rounds_us == q.rounds_them { None } else { Some(q.rounds_us > q.rounds_them) };
    p.summary_rounds = rounds;
}

/// Ligne « match » de la carrière d'un joueur.
fn career_row(id: &str, parsed: &ParsedMatch, puuid: &str, rr: &HashMap<String, (i32, u32)>, competitive: bool) -> Option<(CareerMatch, PlayerStats)> {
    // Filtre final, quelle que soit la source de la liste (Riot, archive, HenrikDev)
    if (competitive && parsed.queue_id != "competitive") || !round_based(&parsed.queue_id) || parsed.teams.len() != 2 || parsed.rounds.len() < 3 {
        return None;
    }
    let me = parsed.players.get(puuid)?;
    let (won, score_us) = parsed.teams.get(&me.team).copied().unwrap_or((false, 0));
    let score_them = parsed.teams.iter().filter(|(t, _)| **t != me.team).map(|(_, (_, r))| *r).max().unwrap_or(0);
    let draw = parsed.teams.len() == 2 && score_us == score_them;
    let rounds = me.rounds.max(1);
    let (mvp, team_mvp) = mvp_flags(parsed, puuid);
    let (rr_change, tier_after) = match rr.get(id) {
        Some((e, t)) if parsed.queue_id == "competitive" => (Some(*e), Some(*t)),
        _ => (None, None),
    };
    Some((
        CareerMatch {
            match_id: id.to_string(),
            map_id: parsed.map_id.clone(),
            queue_id: parsed.queue_id.clone(),
            start_ms: parsed.start_ms,
            agent_id: me.agent.clone(),
            won: if draw || parsed.teams.len() > 2 { None } else { Some(won) },
            score_us,
            score_them,
            kills: me.kills,
            deaths: me.deaths,
            assists: me.assists,
            acs: me.acs(),
            adr: div_round(me.damage, rounds),
            hs: pct(me.head, me.head + me.body + me.leg) as u32,
            kast: pct(me.kast_rounds, rounds) as u32,
            ddelta: ((me.damage as f64 - me.damage_taken as f64) / rounds as f64).round() as i32,
            first_bloods: me.first_bloods,
            multikills: me.multikills,
            tier: me.tier,
            mvp,
            team_mvp,
            rr_change,
            tier_after,
            light: false,
        },
        me.clone(),
    ))
}

/// (meilleur ACS du match, meilleur ACS de son équipe)
fn mvp_flags(m: &ParsedMatch, puuid: &str) -> (bool, bool) {
    let Some(me) = m.players.get(puuid) else { return (false, false) };
    let best = |team: Option<&str>| {
        m.players.values().filter(|p| team.is_none_or(|t| p.team == t)).map(PlayerStats::acs).max().unwrap_or(0)
    };
    let mine = me.acs();
    (mine > 0 && mine >= best(None), mine > 0 && mine >= best(Some(&me.team)))
}

fn detail(id: &str, m: &ParsedMatch, act_name: Option<String>) -> MatchDetail {
    // Groupes : identifiant de groupe partagé par au moins deux joueurs.
    let mut sizes: HashMap<&str, u32> = HashMap::new();
    for p in m.players.values().filter(|p| !p.party_id.is_empty()) {
        *sizes.entry(p.party_id.as_str()).or_default() += 1;
    }
    let mut order: Vec<&str> = sizes.iter().filter(|(_, n)| **n >= 2).map(|(id, _)| *id).collect();
    order.sort_unstable();
    let top = m.players.values().map(PlayerStats::acs).max().unwrap_or(0);

    let mut teams: Vec<TeamLine> = m
        .teams
        .iter()
        .map(|(team_id, (won, rounds_won))| {
            let team_top = m.players.values().filter(|p| p.team == *team_id).map(PlayerStats::acs).max().unwrap_or(0);
            let mut players: Vec<PlayerLine> = m
                .players
                .iter()
                .filter(|(_, p)| p.team == *team_id)
                .map(|(puuid, p)| {
                    let rounds = p.rounds.max(1);
                    PlayerLine {
                        puuid: puuid.clone(),
                        name: Some(p.name.clone()).filter(|s| !s.is_empty()),
                        tag: Some(p.tag.clone()).filter(|s| !s.is_empty()),
                        agent_id: p.agent.clone(),
                        tier: p.tier,
                        level: Some(p.level).filter(|l| *l > 0),
                        card_id: Some(p.card.clone()).filter(|s| !s.is_empty()),
                        party: order.iter().position(|o| *o == p.party_id).map(|i| i as u32),
                        acs: p.acs(),
                        kills: p.kills,
                        deaths: p.deaths,
                        assists: p.assists,
                        adr: div_round(p.damage, rounds),
                        hs: pct(p.head, p.head + p.body + p.leg).round() as u32,
                        kast: pct(p.kast_rounds, rounds).round() as u32,
                        ddelta: ((p.damage as f64 - p.damage_taken as f64) / rounds as f64).round() as i32,
                        first_bloods: p.first_bloods,
                        first_deaths: p.first_deaths,
                        multikills: p.multikills,
                        aces: p.aces,
                        // MVP officiels de Riot quand ils existent, sinon le meilleur ACS
                        mvp: if m.mvp.is_empty() { top > 0 && p.acs() == top } else { m.mvp == *puuid },
                        team_mvp: match m.team_mvps.get(team_id) {
                            Some(id) => id == puuid,
                            None => team_top > 0 && p.acs() == team_top,
                        },
                        score: p.score,
                        damage: p.damage,
                        rounds: p.rounds,
                        clutches: p.clutches,
                        plants: p.plants,
                        defuses: p.defuses,
                        econ: if p.spent > 0 { (p.damage as f64 * 1000.0 / p.spent as f64).round() as u32 } else { 0 },
                        medal: Some(p.medal.clone()).filter(|s| !s.is_empty()),
                        perf: Some(p.perf.round() as u32).filter(|_| !p.medal.is_empty()),
                        offense: Some(p.offense.round() as u32).filter(|_| !p.off_trend.is_empty()),
                        support: Some(p.support.round() as u32).filter(|_| !p.sup_trend.is_empty()),
                        off_trend: Some(p.off_trend.clone()).filter(|s| !s.is_empty()),
                        sup_trend: Some(p.sup_trend.clone()).filter(|s| !s.is_empty()),
                        off_factors: p.off_factors.clone(),
                        sup_factors: p.sup_factors.clone(),
                        round_scores: p.round_scores.clone(),
                        round_kills: p.round_kills.clone(),
                    }
                })
                .collect();
            players.sort_by(|a, b| b.acs.cmp(&a.acs));
            TeamLine { team_id: team_id.clone(), won: *won, rounds_won: *rounds_won, players }
        })
        .collect();
    teams.sort_by(|a, b| a.team_id.cmp(&b.team_id));

    MatchDetail {
        match_id: id.to_string(),
        map_id: m.map_id.clone(),
        queue_id: m.queue_id.clone(),
        act_name,
        start_ms: m.start_ms,
        length_ms: m.length_ms,
        teams,
        rounds: m.rounds.clone(),
        names_pending: false,
        perf_scale: m.perf_scale.filter(|_| m.players.values().any(|p| !p.medal.is_empty())),
    }
}

/// Division arrondie à l'entier le plus proche (comme les trackers : 268,7 → 269).
fn div_round(a: u32, b: u32) -> u32 {
    ((a as f64) / (b.max(1) as f64)).round() as u32
}

fn pct(part: u32, total: u32) -> f32 {
    if total == 0 { 0.0 } else { part as f32 * 100.0 / total as f32 }
}

fn ratio(a: u32, b: u32) -> f32 {
    a as f32 / b.max(1) as f32
}

/// Totaux d'une liste de matchs.
#[derive(Default)]
struct Totals {
    matches: u32,
    wins: u32,
    losses: u32,
    mvps: u32,
    s: PlayerStats,
    /// Matchs détaillés (stats avancées) et leurs totaux
    adv_matches: u32,
    adv: PlayerStats,
    rr: i32,
}

fn totals<'a>(rows: impl Iterator<Item = &'a (CareerMatch, PlayerStats)>) -> Totals {
    let mut t = Totals::default();
    for (m, p) in rows {
        t.matches += 1;
        t.wins += (m.won == Some(true)) as u32;
        t.losses += (m.won == Some(false)) as u32;
        if !p.light {
            t.adv_matches += 1;
            t.mvps += m.mvp as u32;
            let a = &mut t.adv;
            a.rounds += p.rounds;
            a.damage += p.damage;
            a.damage_taken += p.damage_taken;
            a.head += p.head;
            a.body += p.body;
            a.leg += p.leg;
            a.kast_rounds += p.kast_rounds;
            a.first_bloods += p.first_bloods;
            a.first_deaths += p.first_deaths;
            a.multikills += p.multikills;
            a.aces += p.aces;
            a.clutches += p.clutches;
            a.spent += p.spent;
            a.flawless += p.flawless;
        }
        t.rr += m.rr_change.unwrap_or(0);
        let s = &mut t.s;
        s.kills += p.kills;
        s.deaths += p.deaths;
        s.assists += p.assists;
        s.score += p.score;
        s.rounds += if p.summary_rounds > 0 { p.summary_rounds } else { p.rounds };
        s.rounds_won += p.rounds_won;
        s.damage += p.damage;
        s.damage_taken += p.damage_taken;
        s.head += p.head;
        s.body += p.body;
        s.leg += p.leg;
        s.kast_rounds += p.kast_rounds;
        s.first_bloods += p.first_bloods;
        s.first_deaths += p.first_deaths;
        s.multikills += p.multikills;
        s.aces += p.aces;
        s.clutches += p.clutches;
        s.spent += p.spent;
        s.flawless += p.flawless;
    }
    t
}

fn summarize(rows: &[(CareerMatch, PlayerStats)]) -> Summary {
    let t = totals(rows.iter());
    let s = &t.s;
    let a = &t.adv;
    Summary {
        matches: t.matches,
        wins: t.wins,
        losses: t.losses,
        kills: s.kills,
        deaths: s.deaths,
        assists: s.assists,
        rounds: s.rounds,
        rounds_won: s.rounds_won,
        kd: ratio(s.kills, s.deaths),
        kad: ratio(s.kills + s.assists, s.deaths),
        kpr: ratio(s.kills, s.rounds),
        hs: pct(a.head, a.head + a.body + a.leg),
        adr: ratio(a.damage, a.rounds),
        acs: ratio(s.score, s.rounds),
        kast: pct(a.kast_rounds, a.rounds),
        ddelta: (a.damage as f32 - a.damage_taken as f32) / a.rounds.max(1) as f32,
        round_win: pct(s.rounds_won, s.rounds),
        first_bloods: a.first_bloods,
        first_deaths: a.first_deaths,
        flawless: a.flawless,
        multikills: a.multikills,
        aces: a.aces,
        clutches: a.clutches,
        econ: if a.spent > 0 { a.damage as f32 * 1000.0 / a.spent as f32 } else { 0.0 },
        mvps: t.mvps,
        advanced_matches: t.adv_matches,
        rr_net: t.rr,
    }
}

fn agent_stats(rows: &[(CareerMatch, PlayerStats)]) -> Vec<AgentStat> {
    let agents: Vec<String> = rows.iter().map(|(m, _)| m.agent_id.clone()).collect::<HashSet<_>>().into_iter().collect();
    let mut out: Vec<AgentStat> = agents
        .into_iter()
        .map(|agent| {
            let mine: Vec<&(CareerMatch, PlayerStats)> = rows.iter().filter(|(m, _)| m.agent_id == agent).collect();
            let t = totals(mine.iter().copied());
            // Meilleure carte : meilleur winrate, puis le plus de parties.
            let mut maps: HashMap<&str, (u32, u32)> = HashMap::new();
            for (m, _) in &mine {
                let e = maps.entry(m.map_id.as_str()).or_default();
                e.0 += (m.won == Some(true)) as u32;
                e.1 += 1;
            }
            let best = maps.into_iter().max_by(|a, b| {
                let (wa, wb) = (pct(a.1 .0, a.1 .1), pct(b.1 .0, b.1 .1));
                wa.partial_cmp(&wb).unwrap().then(a.1 .1.cmp(&b.1 .1))
            });
            AgentStat {
                agent_id: agent,
                matches: t.matches,
                wins: t.wins,
                kd: ratio(t.s.kills, t.s.deaths),
                adr: ratio(t.adv.damage, t.adv.rounds),
                acs: ratio(t.s.score, t.s.rounds),
                ddelta: (t.adv.damage as f32 - t.adv.damage_taken as f32) / t.adv.rounds.max(1) as f32,
                best_map: best.map(|(id, _)| id.to_string()),
                best_map_wr: best.map_or(0, |(_, (w, n))| pct(w, n).round() as u32),
            }
        })
        .collect();
    out.sort_by(|a, b| b.matches.cmp(&a.matches).then(b.acs.partial_cmp(&a.acs).unwrap()));
    out
}

fn map_stats(rows: &[(CareerMatch, PlayerStats)]) -> Vec<MapStat> {
    let maps: HashSet<&str> = rows.iter().map(|(m, _)| m.map_id.as_str()).collect();
    let mut out: Vec<MapStat> = maps
        .into_iter()
        .map(|map| {
            let t = totals(rows.iter().filter(|(m, _)| m.map_id == map));
            MapStat {
                map_id: map.to_string(),
                matches: t.matches,
                wins: t.wins,
                kd: ratio(t.s.kills, t.s.deaths),
                acs: ratio(t.s.score, t.s.rounds),
                adr: ratio(t.adv.damage, t.adv.rounds),
                round_win: pct(t.s.rounds_won, t.s.rounds),
            }
        })
        .collect();
    out.sort_by(|a, b| b.matches.cmp(&a.matches).then(b.wins.cmp(&a.wins)));
    out
}

struct Kill {
    time: u64,
    killer: String,
    victim: String,
    assists: Vec<String>,
}

fn parse_match(v: &Value) -> ParsedMatch {
    let num = |x: &Value| x.as_u64().unwrap_or(0) as u32;
    let text = |x: &Value| x.as_str().unwrap_or("").to_string();
    let info = &v["matchInfo"];
    let mut players: HashMap<String, PlayerStats> = HashMap::new();
    let mut perf_scale = None;
    for p in v["players"].as_array().into_iter().flatten() {
        let Some(id) = p["subject"].as_str() else { continue };
        if p["isObserver"].as_bool().unwrap_or(false) {
            continue;
        }
        let st = &p["stats"];
        let mut medal = PlayerStats::default();
        if let Some(scale) = read_medal(&p["scores"], &mut medal) {
            perf_scale = Some(scale);
        }
        players.insert(
            id.to_string(),
            PlayerStats {
                medal: medal.medal,
                perf: medal.perf,
                offense: medal.offense,
                support: medal.support,
                off_trend: medal.off_trend,
                sup_trend: medal.sup_trend,
                off_factors: medal.off_factors,
                sup_factors: medal.sup_factors,
                team: text(&p["teamId"]),
                agent: text(&p["characterId"]).to_lowercase(),
                name: text(&p["gameName"]),
                tag: text(&p["tagLine"]),
                tier: num(&p["competitiveTier"]),
                level: num(&p["accountLevel"]),
                card: text(&p["playerCard"]),
                party_id: text(&p["partyId"]),
                kills: num(&st["kills"]),
                deaths: num(&st["deaths"]),
                assists: num(&st["assists"]),
                score: num(&st["score"]),
                rounds: num(&st["roundsPlayed"]),
                ..Default::default()
            },
        );
    }
    let team_of: HashMap<String, String> = players.iter().map(|(id, p)| (id.clone(), p.team.clone())).collect();
    let mut rounds_won: HashMap<String, u32> = HashMap::new();
    let mut flawless: HashMap<String, u32> = HashMap::new();
    let mut rounds: Vec<RoundInfo> = Vec::new();
    let round_list = v["roundResults"].as_array().cloned().unwrap_or_default();
    for p in players.values_mut() {
        p.round_scores = vec![0; round_list.len()];
        p.round_kills = vec![0; round_list.len()];
    }

    for (index, round) in round_list.iter().enumerate() {
        for s in round["playerScores"].as_array().into_iter().flatten() {
            if let Some(p) = s["subject"].as_str().and_then(|id| players.get_mut(id)) {
                p.round_scores[index] = num(&s["score"]);
            }
        }
        if let Some(p) = round["bombPlanter"].as_str().and_then(|id| players.get_mut(id)) {
            p.plants += 1;
        }
        if let Some(p) = round["bombDefuser"].as_str().and_then(|id| players.get_mut(id)) {
            p.defuses += 1;
        }
        let mut kills: Vec<Kill> = Vec::new();
        for ps in round["playerStats"].as_array().into_iter().flatten() {
            let subject = ps["subject"].as_str().unwrap_or("");
            if let Some(p) = players.get_mut(subject) {
                p.spent += num(&ps["economy"]["spent"]);
            }
            for d in ps["damage"].as_array().into_iter().flatten() {
                // Dégâts sur soi-même ou sur un coéquipier : ignorés (comme les trackers)
                let receiver = d["receiver"].as_str().unwrap_or("");
                if receiver == subject || (team_of.contains_key(receiver) && team_of.get(receiver) == team_of.get(subject)) {
                    continue;
                }
                let dmg = num(&d["damage"]);
                if let Some(p) = players.get_mut(subject) {
                    p.damage += dmg;
                    p.head += num(&d["headshots"]);
                    p.body += num(&d["bodyshots"]);
                    p.leg += num(&d["legshots"]);
                }
                if let Some(r) = d["receiver"].as_str().and_then(|r| players.get_mut(r)) {
                    r.damage_taken += dmg;
                }
            }
            for k in ps["kills"].as_array().into_iter().flatten() {
                kills.push(Kill {
                    time: k["roundTime"].as_u64().or(k["gameTime"].as_u64()).unwrap_or(0),
                    killer: k["killer"].as_str().unwrap_or(subject).to_string(),
                    victim: k["victim"].as_str().unwrap_or("").to_string(),
                    assists: k["assistants"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|a| a.as_str().map(String::from))
                        .collect(),
                });
            }
        }
        let first_blood = round["firstBloodPlayer"].as_str().filter(|s| !s.is_empty()).map(String::from);
        let ceremony = round["roundCeremony"].as_str().unwrap_or("");
        let ceremony_player = round["ceremonyPlayer"].as_str().unwrap_or("");
        let winner = text(&round["winningTeam"]);
        for k in kills.iter().filter(|k| team_of.contains_key(&k.victim) && team_of.get(&k.killer) != team_of.get(&k.victim)) {
            if let Some(p) = players.get_mut(&k.killer) {
                p.round_kills[index] += 1;
            }
        }
        let t = RoundTally { team_of: &team_of, rounds_won: &mut rounds_won, flawless: &mut flawless };
        score_round(&mut players, t, kills, first_blood, ceremony, ceremony_player, &winner);
        let ceremony = ceremony.trim_start_matches("Ceremony");
        let ceremony = if ceremony == "Default" { "" } else { ceremony };
        rounds.push(RoundInfo { winner, result: round_result(round), ceremony: ceremony.to_string(), player: ceremony_player.to_string() });
    }
    for p in players.values_mut() {
        p.rounds_won = rounds_won.get(&p.team).copied().unwrap_or(0);
        p.flawless = flawless.get(&p.team).copied().unwrap_or(0);
    }

    let teams = v["teams"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|t| {
            Some((t["teamId"].as_str()?.to_string(), (t["won"].as_bool().unwrap_or(false), num(&t["roundsWon"]))))
        })
        .collect();
    let team_mvps = v["teams"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|t| Some((t["teamId"].as_str()?.to_string(), t["mvp"].as_str().filter(|s| !s.is_empty())?.to_string())))
        .collect();
    ParsedMatch {
        map_id: text(&info["mapId"]),
        queue_id: info["queueID"].as_str().or(info["queueId"].as_str()).unwrap_or("").to_string(),
        season_id: text(&info["seasonId"]),
        start_ms: info["gameStartMillis"].as_u64().unwrap_or(0),
        length_ms: info["gameLengthMillis"].as_u64().unwrap_or(0),
        teams,
        players,
        rounds,
        mvp: text(&v["matchMvp"]),
        team_mvps,
        perf_scale,
        rev: PARSE_REV,
    }
}

const MEDALS: [&str; 3] = ["distinction", "merit", "pass"];
const TRENDS: [&str; 5] = ["double_up", "up", "neutral", "down", "double_down"];

/// Médailles de fin de partie (2026). Riot range ces données sous des clés provisoires
/// (« TempValue… ») : clés connues d'abord, sinon chaque donnée est reconnue à sa forme.
///   F = score de performance, G / H = volets attaque / soutien (0 à 500, moyenne 250)
///   L = { O: médaille, M / N: tendances attaque / soutien, P / Q: facteurs détaillés }
///   T = { R: moyenne, U: maximum, S: { distinction, merit, pass } seuils des médailles }
fn read_medal(scores: &Value, s: &mut PlayerStats) -> Option<PerfScale> {
    let obj = scores.as_object()?;
    let is_medal = |x: &Value| x.as_str().is_some_and(|m| MEDALS.contains(&m));
    let is_trend = |x: &Value| x.as_str().is_some_and(|t| TRENDS.contains(&t));
    // Bloc de la médaille : l'objet qui contient « distinction », « merit » ou « pass »
    let l = obj
        .get("TempValueL")
        .filter(|l| l.as_object().is_some_and(|o| o.values().any(is_medal)))
        .or_else(|| obj.values().find(|v| v.as_object().is_some_and(|o| o.values().any(is_medal))))?
        .as_object()?;
    s.medal = l.get("TempValueO").filter(|m| is_medal(m)).or_else(|| l.values().find(|v| is_medal(v)))?.as_str()?.to_string();
    // Tendances : attaque puis soutien (ordre des clés M, N)
    let trends: Vec<String> = l.values().filter(|v| is_trend(v)).filter_map(|v| v.as_str().map(String::from)).collect();
    let trend = |key: &str, i: usize| l.get(key).filter(|t| is_trend(t)).and_then(|t| t.as_str().map(String::from)).or_else(|| trends.get(i).cloned());
    s.off_trend = trend("TempValueM", 0).unwrap_or_default();
    s.sup_trend = trend("TempValueN", 1).unwrap_or_default();
    let factors = |marker: &str| -> Vec<(String, String)> {
        l.values()
            .filter_map(Value::as_object)
            .find(|o| o.contains_key(marker))
            .map(|o| o.iter().filter(|(_, t)| is_trend(t)).map(|(k, t)| (k.clone(), t.as_str().unwrap_or("").to_string())).collect())
            .unwrap_or_default()
    };
    s.off_factors = factors("killImpact");
    s.sup_factors = factors("utilityUsage");
    let number = |key: &str| obj.get(key).and_then(Value::as_f64).unwrap_or(0.0) as f32;
    s.perf = number("TempValueF");
    s.offense = number("TempValueG");
    s.support = number("TempValueH");
    // Barème : l'objet qui contient les seuils { distinction, merit }
    let has_thresholds = |o: &serde_json::Map<String, Value>| o.values().any(|v| v.get("distinction").is_some_and(Value::is_number));
    let t = obj.get("TempValueT").and_then(Value::as_object).filter(|o| has_thresholds(o)).or_else(|| obj.values().filter_map(Value::as_object).find(|o| has_thresholds(o)));
    let get = |o: Option<&serde_json::Map<String, Value>>, key: &str, or: f32| o.and_then(|o| o.get(key)).and_then(Value::as_f64).map_or(or, |v| v as f32);
    let th = t.and_then(|t| t.values().filter_map(Value::as_object).find(|o| o.contains_key("distinction")));
    Some(PerfScale {
        avg: get(t, "TempValueR", 250.0),
        max: get(t, "TempValueU", 500.0),
        merit: get(th, "merit", 330.0),
        distinction: get(th, "distinction", 420.0),
    })
}

/// Compteurs de manches par équipe, remplis par `score_round`.
struct RoundTally<'a> {
    team_of: &'a HashMap<String, String>,
    rounds_won: &'a mut HashMap<String, u32>,
    flawless: &'a mut HashMap<String, u32>,
}

/// Stats d'une manche, communes aux formats Riot et HenrikDev : first blood / death, multi-kills,
/// aces et clutchs (cérémonies officielles quand elles sont connues), KAST, manches flawless.
fn score_round(
    players: &mut HashMap<String, PlayerStats>,
    tally: RoundTally,
    mut kills: Vec<Kill>,
    first_blood: Option<String>,
    ceremony: &str,
    ceremony_player: &str,
    winner: &str,
) {
    let team_of = tally.team_of;
    kills.sort_by_key(|k| k.time);

    // First blood : donnée officielle de Riot si présente, sinon le premier kill de la manche
    let first_blood = first_blood.or_else(|| kills.first().map(|k| k.killer.clone()));
    if let Some(p) = first_blood.and_then(|id| players.get_mut(&id)) {
        p.first_bloods += 1;
    }
    if let Some(p) = kills.first().and_then(|k| players.get_mut(&k.victim)) {
        p.first_deaths += 1;
    }
    // Ennemis distincts tués (un ennemi réanimé puis retué ne compte qu'une fois)
    let mut enemies_killed: HashMap<&str, HashSet<&str>> = HashMap::new();
    for k in &kills {
        if team_of.get(&k.killer) != team_of.get(&k.victim) {
            enemies_killed.entry(k.killer.as_str()).or_default().insert(k.victim.as_str());
        }
    }
    for (id, p) in players.iter_mut() {
        let distinct = enemies_killed.get(id.as_str()).map_or(0, |v| v.len());
        if distinct >= 3 {
            p.multikills += 1;
        }
        // Ace et clutch : cérémonies officielles de Riot quand elles existent
        let is_ace = if ceremony.is_empty() { distinct >= 5 } else { ceremony == "CeremonyAce" && ceremony_player == id };
        if is_ace {
            p.aces += 1;
        }
        if ceremony == "CeremonyClutch" && ceremony_player == id {
            p.clutches += 1;
        }
        // KAST : kill, assist, survie ou mort échangée.
        let assisted = kills.iter().any(|k| k.assists.iter().any(|a| a == id));
        let death = kills.iter().find(|k| k.victim == *id);
        let traded = death.is_some_and(|d| {
            kills.iter().any(|k2| k2.victim == d.killer && k2.time >= d.time && k2.time - d.time <= TRADE_WINDOW_MS)
        });
        if distinct > 0 || assisted || death.is_none() || traded {
            p.kast_rounds += 1;
        }
    }

    if !winner.is_empty() {
        *tally.rounds_won.entry(winner.to_string()).or_default() += 1;
        let lost_someone = kills.iter().any(|k| team_of.get(&k.victim).map(String::as_str) == Some(winner));
        if !lost_someone {
            *tally.flawless.entry(winner.to_string()).or_default() += 1;
        }
    }
}

/// Match HenrikDev (v4) → format interne. Mêmes règles que pour Riot : dégâts sur soi et sur les
/// coéquipiers exclus, ennemis distincts, cérémonies de manche. HenrikDev ne nomme pas le joueur
/// d'une cérémonie : l'ace revient au meilleur tueur de la manche, le clutch au seul survivant.
fn parse_henrik_match(d: &Value) -> Option<ParsedMatch> {
    let num = |x: &Value| x.as_u64().unwrap_or(0) as u32;
    let text = |x: &Value| x.as_str().unwrap_or("").to_string();
    let meta = &d["metadata"];
    // Manches jouées comme les compte Riot : en cas d'abandon, les manches jouées + la manche
    // d'abandon (pas les manches « offertes » ensuite, que HenrikDev liste aussi).
    let all_rounds = d["rounds"].as_array().cloned().unwrap_or_default();
    let surrender = all_rounds.iter().position(|r| r["result"].as_str().is_some_and(|x| x.to_lowercase().contains("surrender")));
    // Match nul par vote en prolongation (13-13, 14-14…) : Riot compte aussi la manche du vote.
    let team_won: Vec<u64> = d["teams"].as_array().into_iter().flatten().map(|t| t["rounds"]["won"].as_u64().unwrap_or(0)).collect();
    let draw_vote = surrender.is_none() && team_won.len() == 2 && team_won[0] == team_won[1] && team_won[0] >= 13;
    let rounds_played = match surrender {
        Some(i) => i + 1,
        None => all_rounds.len() + draw_vote as usize,
    } as u32;
    let mut players: HashMap<String, PlayerStats> = HashMap::new();
    for p in d["players"].as_array()? {
        let Some(id) = p["puuid"].as_str() else { continue };
        let st = &p["stats"];
        players.insert(
            id.to_string(),
            PlayerStats {
                team: text(&p["team_id"]),
                agent: text(&p["agent"]["id"]).to_lowercase(),
                name: text(&p["name"]),
                tag: text(&p["tag"]),
                tier: num(&p["tier"]["id"]),
                level: num(&p["account_level"]),
                card: text(&p["customization"]["card"]),
                party_id: text(&p["party_id"]),
                kills: num(&st["kills"]),
                deaths: num(&st["deaths"]),
                assists: num(&st["assists"]),
                score: num(&st["score"]),
                rounds: rounds_played,
                spent: num(&p["economy"]["spent"]["overall"]),
                ..Default::default()
            },
        );
    }
    if players.is_empty() {
        return None;
    }
    let team_of: HashMap<String, String> = players.iter().map(|(id, p)| (id.clone(), p.team.clone())).collect();
    let puuid_of = |x: &Value| x["puuid"].as_str().or(x.as_str()).unwrap_or("").to_string();

    // Kills regroupés par manche
    let mut kills_by_round: HashMap<u64, Vec<Kill>> = HashMap::new();
    for k in d["kills"].as_array().into_iter().flatten() {
        kills_by_round.entry(k["round"].as_u64().unwrap_or(0)).or_default().push(Kill {
            time: k["time_in_round_in_ms"].as_u64().unwrap_or(0),
            killer: puuid_of(&k["killer"]),
            victim: puuid_of(&k["victim"]),
            assists: k["assistants"].as_array().into_iter().flatten().map(puuid_of).collect(),
        });
    }

    let mut rounds_won: HashMap<String, u32> = HashMap::new();
    let mut flawless: HashMap<String, u32> = HashMap::new();
    let mut rounds: Vec<RoundInfo> = Vec::new();
    for (i, round) in d["rounds"].as_array().into_iter().flatten().enumerate() {
        for ps in round["stats"].as_array().into_iter().flatten() {
            let subject = puuid_of(&ps["player"]);
            for e in ps["damage_events"].as_array().into_iter().flatten() {
                let receiver = puuid_of(&e["player"]);
                if receiver == subject || (team_of.contains_key(&receiver) && team_of.get(&receiver) == team_of.get(&subject)) {
                    continue;
                }
                let dmg = num(&e["damage"]);
                if let Some(p) = players.get_mut(&subject) {
                    p.damage += dmg;
                    p.head += num(&e["headshots"]);
                    p.body += num(&e["bodyshots"]);
                    p.leg += num(&e["legshots"]);
                }
                if let Some(r) = players.get_mut(&receiver) {
                    r.damage_taken += dmg;
                }
            }
        }
        let id = round["id"].as_u64().unwrap_or(i as u64);
        let kills = kills_by_round.remove(&id).unwrap_or_default();
        let winner = text(&round["winning_team"]);
        let ceremony = round["ceremony"].as_str().unwrap_or("");
        let ceremony_player = match ceremony {
            "CeremonyAce" => {
                let mut distinct: HashMap<&str, HashSet<&str>> = HashMap::new();
                for k in kills.iter().filter(|k| team_of.get(&k.killer) != team_of.get(&k.victim)) {
                    distinct.entry(k.killer.as_str()).or_default().insert(k.victim.as_str());
                }
                distinct.into_iter().max_by_key(|(_, v)| v.len()).map(|(k, _)| k.to_string()).unwrap_or_default()
            }
            "CeremonyClutch" => {
                // Le clutcheur : le coéquipier resté seul en vie quand le reste de l'équipe gagnante
                // est tombé (il peut mourir ensuite, après la pose ou le désamorçage).
                let mut alive: HashSet<&str> = players.iter().filter(|(_, p)| p.team == winner).map(|(pid, _)| pid.as_str()).collect();
                let mut ordered: Vec<&Kill> = kills.iter().collect();
                ordered.sort_by_key(|k| k.time);
                let mut last = String::new();
                for k in ordered {
                    if alive.remove(k.victim.as_str()) && alive.len() == 1 {
                        last = alive.iter().next().map(|s| s.to_string()).unwrap_or_default();
                        break;
                    }
                }
                last
            }
            _ => String::new(),
        };
        let t = RoundTally { team_of: &team_of, rounds_won: &mut rounds_won, flawless: &mut flawless };
        score_round(&mut players, t, kills, None, ceremony, &ceremony_player, &winner);
        let result = text(&round["result"]);
        let ceremony = ceremony.trim_start_matches("Ceremony");
        let ceremony = if ceremony == "Default" { "" } else { ceremony }.to_string();
        rounds.push(RoundInfo { winner, result: round_result(&serde_json::json!({ "roundResultCode": result, "roundResult": result })), ceremony, player: ceremony_player });
    }
    for p in players.values_mut() {
        p.rounds_won = rounds_won.get(&p.team).copied().unwrap_or(0);
        p.flawless = flawless.get(&p.team).copied().unwrap_or(0);
    }

    let teams = d["teams"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|t| {
            let id = t["team_id"].as_str()?.to_string();
            let won = rounds_won.get(&id).copied().unwrap_or_else(|| num(&t["rounds"]["won"]));
            Some((id, (t["won"].as_bool().unwrap_or(false), won)))
        })
        .collect();
    Some(ParsedMatch {
        map_id: text(&meta["map"]["id"]).to_lowercase(),
        queue_id: henrik_queue(meta["queue"]["id"].as_str().unwrap_or("")),
        season_id: text(&meta["season"]["id"]),
        start_ms: meta["started_at"].as_str().and_then(crate::tracker::iso_ms).unwrap_or(0),
        length_ms: meta["game_length_in_ms"].as_u64().unwrap_or(0),
        teams,
        players,
        rounds,
        ..Default::default()
    })
}

/// Type de fin de manche, normalisé.
fn round_result(round: &Value) -> String {
    let code = round["roundResultCode"].as_str().unwrap_or("");
    let text = round["roundResult"].as_str().unwrap_or("").to_lowercase();
    let r = match code {
        "Elimination" | "Defuse" | "Detonate" | "Surrendered" => code,
        _ if text.contains("defus") => "Defuse",
        _ if text.contains("detonat") => "Detonate",
        _ if text.contains("elimin") => "Elimination",
        _ if text.contains("surrender") => "Surrendered",
        _ => "Timer",
    };
    r.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn test_cache(dir: Option<PathBuf>) -> CareerCache {
        CareerCache::new(dir, Arc::new(Henrik::disabled()), Arc::new(Directory::new(None)))
    }

    #[test]
    fn parses_match_stats() {
        let kill = |t: u64, killer: &str, victim: &str, assists: &[&str]| {
            json!({ "roundTime": t, "killer": killer, "victim": victim, "assistants": assists })
        };
        let v = json!({
            "matchInfo": { "mapId": "/Game/Maps/Ascent/Ascent", "queueID": "competitive", "gameStartMillis": 5, "gameLengthMillis": 1800000, "seasonId": "act" },
            "players": [
                { "subject": "a", "gameName": "Alpha", "tagLine": "EUW", "teamId": "Blue", "partyId": "p1", "characterId": "JETT", "competitiveTier": 21,
                  "stats": { "kills": 2, "deaths": 1, "assists": 0, "score": 600, "roundsPlayed": 2 } },
                { "subject": "c", "teamId": "Blue", "partyId": "p1", "characterId": "SAGE", "stats": { "kills": 0, "deaths": 0, "assists": 1, "score": 100, "roundsPlayed": 2 } },
                { "subject": "b", "teamId": "Red", "partyId": "p2", "characterId": "x", "stats": { "kills": 1, "deaths": 2, "assists": 0, "score": 300, "roundsPlayed": 2 } },
                { "subject": "obs", "teamId": "Neutral", "isObserver": true, "stats": {} }
            ],
            "roundResults": [
                // Manche 1 : a tue b en premier (flawless pour Blue).
                { "winningTeam": "Blue", "roundResultCode": "Elimination", "playerStats": [
                    { "subject": "a", "damage": [ { "receiver": "b", "damage": 150, "headshots": 1, "bodyshots": 2, "legshots": 0 } ],
                      "kills": [ kill(1000, "a", "b", &["c"]) ] } ] },
                // Manche 2 : b tue a, et personne ne venge a.
                { "winningTeam": "Red", "roundResult": "Bomb detonated", "playerStats": [
                    { "subject": "b", "damage": [ { "receiver": "a", "damage": 140, "headshots": 0, "bodyshots": 1, "legshots": 1 } ],
                      "kills": [ kill(2000, "b", "a", &[]) ] } ] }
            ],
            "teams": [ { "teamId": "Blue", "won": true, "roundsWon": 13 }, { "teamId": "Red", "won": false, "roundsWon": 7 } ]
        });
        let p = parse_match(&v);
        assert!(!p.players.contains_key("obs"));
        let a = &p.players["a"];
        assert_eq!((a.damage, a.damage_taken, a.head, a.agent.as_str()), (150, 140, 1, "jett"));
        assert_eq!((a.first_bloods, a.first_deaths), (1, 1));
        assert_eq!(p.players["b"].first_deaths, 1);
        // a : kill en manche 1, mort non échangée en manche 2 → KAST 1/2.
        assert_eq!(a.kast_rounds, 1);
        // c : assist en manche 1, survie en manche 2 → KAST 2/2.
        assert_eq!(p.players["c"].kast_rounds, 2);
        assert_eq!((a.rounds_won, a.flawless), (1, 1));
        assert_eq!(p.teams["Blue"], (true, 13));
        let results: Vec<&str> = p.rounds.iter().map(|r| r.result.as_str()).collect();
        assert_eq!(results, ["Elimination", "Detonate"]);

        let d = detail("m", &p, None);
        let blue = d.teams.iter().find(|t| t.team_id == "Blue").unwrap();
        // Classés par ACS, a (300) devant c (50) ; a et c sont dans le même groupe.
        assert_eq!(blue.players[0].puuid, "a");
        assert_eq!(blue.players[0].name.as_deref(), Some("Alpha"));
        assert!(blue.players[0].mvp && blue.players[0].team_mvp);
        assert_eq!(blue.players[0].party, Some(0));
        assert_eq!(blue.players[1].party, Some(0));
        let red = d.teams.iter().find(|t| t.team_id == "Red").unwrap();
        assert_eq!(red.players[0].party, None);
        assert_eq!(mvp_flags(&p, "a"), (true, true));
    }

    #[test]
    fn official_round_data() {
        let kill = |t: u64, killer: &str, victim: &str| json!({ "roundTime": t, "killer": killer, "victim": victim, "assistants": [] });
        let v = json!({
            "matchInfo": { "mapId": "m", "queueID": "competitive", "gameStartMillis": 1 },
            "players": [
                { "subject": "a", "teamId": "Blue", "stats": { "kills": 6, "roundsPlayed": 2 } },
                { "subject": "t", "teamId": "Blue", "stats": { "roundsPlayed": 2 } },
                { "subject": "e1", "teamId": "Red", "stats": {} }, { "subject": "e2", "teamId": "Red", "stats": {} },
                { "subject": "e3", "teamId": "Red", "stats": {} }, { "subject": "e4", "teamId": "Red", "stats": {} },
                { "subject": "e5", "teamId": "Red", "stats": {} }
            ],
            "roundResults": [
                // 5 kills mais e1 retué après réanimation : 4 ennemis distincts → clutch, pas ace
                { "winningTeam": "Blue", "roundCeremony": "CeremonyClutch", "ceremonyPlayer": "a", "firstBloodPlayer": "a", "playerStats": [
                    { "subject": "a", "economy": { "spent": 3900 },
                      "damage": [ { "receiver": "e1", "damage": 300, "headshots": 1, "bodyshots": 1, "legshots": 0 },
                                  { "receiver": "t", "damage": 50, "headshots": 1, "bodyshots": 0, "legshots": 0 },
                                  { "receiver": "a", "damage": 30, "headshots": 0, "bodyshots": 1, "legshots": 0 } ],
                      "kills": [ kill(1, "a", "e1"), kill(2, "a", "e2"), kill(3, "a", "e1"), kill(4, "a", "e3"), kill(5, "a", "e4") ] } ] },
                // Vrai ace reconnu par Riot
                { "winningTeam": "Blue", "roundCeremony": "CeremonyAce", "ceremonyPlayer": "a", "firstBloodPlayer": "a", "playerStats": [
                    { "subject": "a", "economy": { "spent": 100 }, "kills": [ kill(1, "a", "e1"), kill(2, "a", "e2"), kill(3, "a", "e3"), kill(4, "a", "e4"), kill(5, "a", "e5") ] } ] }
            ],
            "teams": [ { "teamId": "Blue", "won": true, "roundsWon": 13 }, { "teamId": "Red", "won": false, "roundsWon": 5 } ]
        });
        let p = parse_match(&v);
        let a = &p.players["a"];
        assert_eq!((a.aces, a.clutches, a.multikills), (1, 1, 2));
        // Dégâts sur le coéquipier et sur soi ignorés
        assert_eq!((a.damage, a.head), (300, 1));
        assert_eq!(p.players["t"].damage_taken, 0);
        assert_eq!((a.first_bloods, a.spent), (2, 4000));
    }

    #[test]
    fn quick_rows_and_advanced_stats() {
        let q = QuickMatch {
            id: "q".into(), start: 1, map: "m".into(), queue: "competitive".into(), agent: "a".into(), tier: 20, level: 5,
            score: 5000, kills: 20, deaths: 10, assists: 3, head: 10, body: 30, leg: 0, damage: 3000, damage_taken: 2000,
            rounds_us: 13, rounds_them: 7,
        };
        let (m, p) = quick_row(&q, &HashMap::new(), true).unwrap();
        assert_eq!((m.acs, m.adr, m.hs, m.won, m.light), (250, 150, 25, Some(true), true));
        // Une ligne provisoire ne compte pas dans les stats avancées
        let s = summarize(&[(m, p)]);
        assert_eq!((s.matches, s.advanced_matches, s.kills), (1, 0, 20));
        // Pas en mode compétition / modes sans manches
        assert!(quick_row(&QuickMatch { queue: "unrated".into(), ..q.clone() }, &HashMap::new(), true).is_none());
        assert!(quick_row(&QuickMatch { queue: "deathmatch".into(), ..q }, &HashMap::new(), false).is_none());
        assert_eq!(henrik_queue("Spike Rush"), "spikerush");
        assert_eq!(henrik_queue("Custom Game"), "");
    }

    #[test]
    fn excluded_modes() {
        assert!(round_based("competitive") && round_based("unrated") && round_based("swiftplay") && round_based("premier"));
        assert!(!round_based("deathmatch") && !round_based("ggteam") && !round_based("hurm") && !round_based(""));
        assert!(henrik_round_based("Competitive") && henrik_round_based("Unrated") && henrik_round_based("Swiftplay"));
        assert!(!henrik_round_based("Deathmatch") && !henrik_round_based("Team Deathmatch") && !henrik_round_based("Escalation") && !henrik_round_based("Custom Game"));

        // Un match à mort (12 « équipes », 1 manche) n'entre pas dans la carrière.
        let v = json!({
            "matchInfo": { "mapId": "m", "queueID": "deathmatch", "gameStartMillis": 1 },
            "players": [ { "subject": "a", "teamId": "a", "stats": { "kills": 40, "score": 11876, "roundsPlayed": 1 } } ],
            "roundResults": [ { "winningTeam": "a", "playerStats": [] } ],
            "teams": [ { "teamId": "a", "won": true, "roundsWon": 40 } ]
        });
        assert!(career_row("m", &parse_match(&v), "a", &HashMap::new(), false).is_none());
    }

    #[test]
    fn local_archive_index() {
        let dir = std::env::temp_dir().join(format!("valo-overlay-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let v = json!({
            "matchInfo": { "mapId": "m", "queueID": "competitive", "gameStartMillis": 1_000 },
            "players": [ { "subject": "a", "teamId": "Blue", "stats": {} }, { "subject": "b", "teamId": "Red", "stats": {} } ],
            "teams": []
        });
        let cache = test_cache(Some(dir.clone()));
        cache.put("m1", parse_match(&v));
        cache.note("a", "m2", Seen { start: 5_000, comp: false });
        cache.flush();

        let w = ActWindow { id: "act".into(), name: "Acte".into(), start: 500, end: 2_000, current: false };
        // Les deux joueurs du match sont indexés ; filtre par période et par mode.
        assert_eq!(cache.known("b", Some(&w), true).0, vec![("m1".to_string(), 1_000)]);
        assert_eq!(cache.known("a", Some(&w), false).0.len(), 1);
        assert_eq!(cache.known("a", None, false).1, Some(1_000));

        // Rechargé depuis le disque par une nouvelle instance.
        let reloaded = test_cache(Some(dir.clone()));
        assert!(reloaded.has("m1"));
        assert_eq!(reloaded.known("a", Some(&w), true).0.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Vérification réelle des résumés HenrikDev :
    /// `VALO_HENRIK_KEY=… VALO_PUUID=… VALO_ACT_START=ms cargo test real_henrik -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn real_henrik() {
        let key = std::env::var("VALO_HENRIK_KEY").unwrap();
        let puuid = std::env::var("VALO_PUUID").unwrap();
        let start: u64 = std::env::var("VALO_ACT_START").unwrap().parse().unwrap();
        let w = ActWindow { id: "act".into(), name: "Acte".into(), start, end: u64::MAX, current: true };
        let henrik = Arc::new(Henrik::new(&crate::config::Config { henrik_api_key: key.clone(), ..Default::default() }));
        let cache = CareerCache::new(None, henrik, Arc::new(Directory::new(None)));
        let t = Instant::now();
        let (list, reached) = tauri::async_runtime::block_on(henrik_matches(&cache, "eu", &puuid, true, &w));
        let rows: Vec<_> = list.iter().filter_map(|q| quick_row(q, &HashMap::new(), true)).collect();
        let s = summarize(&rows);
        println!(
            "{} résumés en {:.1} s (début d'acte atteint : {reached}) → {} matchs, {} V / {} D, K/D {:.2}, ACS {:.1}, ADR {:.1}, HS {:.1} %",
            list.len(), t.elapsed().as_secs_f32(), s.matches, s.wins, s.losses, s.kd, s.acs, s.adr, s.hs
        );
        assert!(!list.is_empty());
    }

    /// Conversion d'un vrai match HenrikDev, comparée au même match lu chez Riot :
    /// `VALO_HENRIK_SAMPLE=v4.json VALO_RIOT_PARSED=matches-v2/id.json cargo test real_henrik_match -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn real_henrik_match() {
        let v: Value = serde_json::from_str(&std::fs::read_to_string(std::env::var("VALO_HENRIK_SAMPLE").unwrap()).unwrap()).unwrap();
        let h = parse_henrik_match(&v["data"]).unwrap();
        let r: ParsedMatch = serde_json::from_slice(&std::fs::read(std::env::var("VALO_RIOT_PARSED").unwrap()).unwrap()).unwrap();
        assert_eq!((h.queue_id.as_str(), h.season_id.as_str(), h.rounds.len()), (r.queue_id.as_str(), r.season_id.as_str(), r.rounds.len()));
        assert_eq!(h.teams, r.teams);
        let mut diffs = 0;
        for (id, rp) in &r.players {
            let hp = &h.players[id];
            let a = [hp.kills, hp.deaths, hp.assists, hp.score, hp.damage, hp.damage_taken, hp.head, hp.body, hp.leg, hp.kast_rounds, hp.first_bloods, hp.first_deaths, hp.multikills, hp.aces, hp.clutches, hp.flawless, hp.spent];
            let b = [rp.kills, rp.deaths, rp.assists, rp.score, rp.damage, rp.damage_taken, rp.head, rp.body, rp.leg, rp.kast_rounds, rp.first_bloods, rp.first_deaths, rp.multikills, rp.aces, rp.clutches, rp.flawless, rp.spent];
            if a != b {
                diffs += 1;
                println!("{} henrik {a:?}
{} riot   {b:?}", hp.name, " ".repeat(hp.name.len()));
            }
            assert_eq!((hp.team.as_str(), hp.agent.as_str(), hp.tier, hp.party_id.as_str()), (rp.team.as_str(), rp.agent.as_str(), rp.tier, rp.party_id.as_str()));
        }
        println!("{} joueurs, {diffs} différence(s)", r.players.len());
        let ri: Vec<_> = r.rounds.iter().map(|x| (x.winner.as_str(), x.result.as_str())).collect();
        let hi: Vec<_> = h.rounds.iter().map(|x| (x.winner.as_str(), x.result.as_str())).collect();
        assert_eq!(ri, hi);
    }

    /// Carrière sans le jeu ni le client Riot, via le serveur relais :
    /// `VALO_SERVER=http://127.0.0.1:8787 VALO_TOKEN=… VALO_PUUID=… cargo test real_offline -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn real_offline() {
        let cfg = crate::config::Config {
            api_server: std::env::var("VALO_SERVER").unwrap(),
            api_token: std::env::var("VALO_TOKEN").unwrap_or_default(),
            ..Default::default()
        };
        let people = Arc::new(Directory::new(None));
        let cache = CareerCache::new(None, Arc::new(Henrik::new(&cfg)), people.clone());
        let riot = Riot::new();
        tauri::async_runtime::block_on(async {
            let puuid = std::env::var("VALO_PUUID").unwrap();
            let w = ActWindow { id: "8102cd81-43a0-d0d7-bd59-47b8fe9bed1b".into(), name: "V26 · ACTE V".into(), start: crate::tracker::iso_ms("2026-08-19T00:00:00Z").unwrap(), end: u64::MAX, current: true };
            let t = Instant::now();
            let region = people.region_for(&puuid, "");
            let career = load(&riot, &cache, &puuid, true, Some(w), &region, |_| {}).await.unwrap();
            let s = &career.summary;
            println!(
                "carrière en {:.1} s : {} matchs ({} détaillés), {} V / {} D, K/D {:.2}, ACS {:.0}, KAST {:.1} %, first bloods {}",
                t.elapsed().as_secs_f32(), s.matches, s.advanced_matches, s.wins, s.losses, s.kd, s.acs, s.kast, s.first_bloods
            );
            assert!(s.matches > 0 && s.advanced_matches > 0);
        });
    }

    /// Pseudos d'un match complétés par HenrikDev : `VALO_HENRIK_KEY=… VALO_MATCH_ID=… cargo test real_names -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn real_names() {
        let cfg = crate::config::Config { henrik_api_key: std::env::var("VALO_HENRIK_KEY").unwrap(), api_server: std::env::var("VALO_SERVER").unwrap_or_default(), ..Default::default() };
        let cache = CareerCache::new(None, Arc::new(Henrik::new(&cfg)), Arc::new(Directory::new(None)));
        let id = std::env::var("VALO_MATCH_ID").unwrap();
        let named = tauri::async_runtime::block_on(henrik_match(&cache, &id, "eu"));
        let named = named.expect("HenrikDev n'a rien renvoyé");
        println!("{:?}", named.players.values().map(|p| format!("{}#{}", p.name, p.tag)).collect::<Vec<_>>());
    }

    /// Chargement réel (session Riot + relais, cache vide) : chaque mise à jour envoyée à l’écran.
    /// `VALO_PUUID=… VALO_ACT_START=2026-08-19T00:00:00Z cargo test real_emits -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn real_emits() {
        let dir = std::env::temp_dir().join(format!("valo-emits-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cfg = crate::config::Config::default();
        let people = Arc::new(Directory::new(None));
        let cache = CareerCache::new(Some(dir.clone()), Arc::new(Henrik::new(&cfg)), people);
        let riot = Riot::new();
        let puuid = std::env::var("VALO_PUUID").unwrap();
        let start = crate::tracker::iso_ms(&std::env::var("VALO_ACT_START").unwrap()).unwrap();
        let w = ActWindow { id: "8102cd81-43a0-d0d7-bd59-47b8fe9bed1b".into(), name: "Acte".into(), start, end: u64::MAX, current: true };
        let t0 = Instant::now();
        let line = |c: &Career| {
            let s = &c.summary;
            format!(
                "{:>5.1}s  matchs {:>3} (détaillés {:>3})  V {:>2} D {:>2}  K/D {:.2}  ACS {:>5.1}  ADR {:>5.1}  HS {:>4.1}  KAST {:>4.1}  FB {:>3}  aces {}  principales {} done {}",
                t0.elapsed().as_secs_f32(), s.matches, s.advanced_matches, s.wins, s.losses, s.kd, s.acs, s.adr, s.hs, s.kast, s.first_bloods, s.aces, c.main_ready, c.done
            )
        };
        let final_c = tauri::async_runtime::block_on(async {
            let conn = riot.ensure().await;
            println!("session Riot : {}", riot.connected() && conn.is_ok());
            load(&riot, &cache, &puuid, true, Some(w), "eu", |c| println!("{}", line(c))).await.unwrap()
        });
        println!("FINAL {}", line(&final_c));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Chaque match récent d'un joueur lu chez Riot ET via le relais : toute différence est listée.
    /// `VALO_PUUID=… cargo test real_compare_sources -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn real_compare_sources() {
        let henrik = Henrik::new(&crate::config::Config::default());
        let riot = Riot::new();
        let puuid = std::env::var("VALO_PUUID").unwrap();
        tauri::async_runtime::block_on(async {
            riot.ensure().await.unwrap();
            let pages: usize = std::env::var("VALO_PAGES").ok().and_then(|p| p.parse().ok()).unwrap_or(1);
            let mut ids: Vec<String> = Vec::new();
            for page in 0..pages {
                let h = riot
                    .pd_patient(&format!("/mmr/v1/players/{puuid}/competitiveupdates?startIndex={}&endIndex={}&queue=competitive", page * 20, page * 20 + 20), Duration::from_secs(120))
                    .await
                    .unwrap();
                let Some(h) = h else { break };
                let list = h["Matches"].as_array().cloned().unwrap_or_default();
                if list.is_empty() {
                    break;
                }
                ids.extend(list.iter().filter_map(|m| m["MatchID"].as_str().map(String::from)));
            }
            // Liste complète de l'acte (résumés du relais) en plus des matchs classés récents de Riot
            if let Ok(start) = std::env::var("VALO_ACT_START") {
                let w = ActWindow { id: "x".into(), name: "x".into(), start: crate::tracker::iso_ms(&start).unwrap(), end: u64::MAX, current: true };
                let cache = CareerCache::new(None, Arc::new(Henrik::new(&crate::config::Config::default())), Arc::new(Directory::new(None)));
                let (list, _) = henrik_matches(&cache, "eu", &puuid, true, &w).await;
                for q in list.iter() {
                    if !ids.contains(&q.id) {
                        ids.push(q.id.clone());
                    }
                }
            }
            let only_me = std::env::var("VALO_ONLY_ME").is_ok();
            let names = ["kills", "deaths", "assists", "score", "rounds", "rounds_won", "damage", "damage_taken", "head", "body", "leg", "kast", "fb", "fd", "multi", "aces", "clutch", "flawless", "spent"];
            let fields = |a: &PlayerStats| {
                [a.kills, a.deaths, a.assists, a.score, a.rounds, a.rounds_won, a.damage, a.damage_taken, a.head, a.body, a.leg, a.kast_rounds, a.first_bloods, a.first_deaths, a.multikills, a.aces, a.clutches, a.flawless, a.spent]
            };
            let (mut same, mut diff) = (0, 0);
            for id in &ids {
                let r = parse_match(&riot.pd_patient(&format!("/match-details/v1/matches/{id}"), Duration::from_secs(120)).await.unwrap().unwrap());
                let Some(v) = henrik.get(&format!("valorant/v4/match/eu/{id}")).await else {
                    println!("{} : absent du relais", &id[..8]);
                    continue;
                };
                let hm = parse_henrik_match(&v["data"]).unwrap();
                for (p, a) in r.players.iter().filter(|(p, _)| !only_me || **p == puuid) {
                    let Some(b) = hm.players.get(p) else {
                        println!("{} : joueur absent", &id[..8]);
                        diff += 1;
                        continue;
                    };
                    let (fa, fb) = (fields(a), fields(b));
                    if fa == fb {
                        same += 1;
                    } else {
                        diff += 1;
                        let d: Vec<String> = (0..fa.len()).filter(|&i| fa[i] != fb[i]).map(|i| format!("{} riot {} / relais {}", names[i], fa[i], fb[i])).collect();
                        println!("{} {} : {}", &id[..8], &p[..8], d.join(", "));
                    }
                }
                if r.rounds.len() != hm.rounds.len() || r.teams != hm.teams {
                    println!("{} : manches ou équipes différentes", &id[..8]);
                }
                tokio::time::sleep(Duration::from_millis(700)).await;
            }
            println!("{} matchs, joueurs identiques {same}, différents {diff}", ids.len());
        });
    }

    /// Vérification sur un vrai match : `VALO_MATCH_SAMPLE=chemin.json cargo test -- --ignored`
    #[test]
    #[ignore]
    fn real_match_sample() {
        let path = std::env::var("VALO_MATCH_SAMPLE").expect("VALO_MATCH_SAMPLE");
        let v: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let p = parse_match(&v);
        let d = detail("x", &p, None);
        println!("{} manches, fins : {:?}", d.rounds.len(), d.rounds.iter().map(|r| r.result.as_str()).collect::<Vec<_>>());
        for t in &d.teams {
            println!("{} gagné={} manches={}", t.team_id, t.won, t.rounds_won);
            for l in &t.players {
                println!(
                    "  tier {:>2} acs {:>3} {:>2}/{:>2}/{:>2} adr {:>3} hs {:>2}% kast {:>3}% fb {} fd {} mk {} groupe {:?} mvp {}",
                    l.tier, l.acs, l.kills, l.deaths, l.assists, l.adr, l.hs, l.kast, l.first_bloods, l.first_deaths, l.multikills, l.party, l.mvp
                );
            }
        }
        assert_eq!(d.teams.iter().map(|t| t.players.len()).sum::<usize>(), 10);
        assert_eq!(d.rounds.len() as u32, d.teams.iter().map(|t| t.rounds_won).sum::<u32>());
    }

    #[test]
    fn medal_fields() {
        let scores = serde_json::json!({
            "TempValueF": 413.9, "TempValueG": 278.9, "TempValueH": 500,
            "TempValueL": { "TempValueO": "merit", "TempValueM": "neutral", "TempValueN": "double_up",
                "TempValueP": { "damage": "neutral", "deathImpact": "up", "killImpact": "neutral", "trades": "down" },
                "TempValueQ": { "assists": "double_up", "defuses": "neutral", "plants": "neutral", "utilityUsage": "up" } },
            "TempValueT": { "TempValueR": 250, "TempValueV": 0, "TempValueU": 500, "TempValueS": { "distinction": 420, "merit": 330, "pass": 0 } }
        });
        let mut s = PlayerStats::default();
        let scale = read_medal(&scores, &mut s).unwrap();
        assert_eq!(scale, PerfScale { avg: 250.0, max: 500.0, merit: 330.0, distinction: 420.0 });
        assert_eq!((s.medal.as_str(), s.perf.round(), s.offense.round(), s.support.round()), ("merit", 414.0, 279.0, 500.0));
        assert_eq!((s.off_trend.as_str(), s.sup_trend.as_str()), ("neutral", "double_up"));
        assert_eq!(s.off_factors.len(), 4);
        assert!(s.sup_factors.contains(&("assists".to_string(), "double_up".to_string())));
        // Clés renommées par Riot : médaille et facteurs retrouvés à leur forme
        let renamed = serde_json::json!({ "x": { "medal": "distinction", "a": { "killImpact": "up" }, "b": { "utilityUsage": "down" } } });
        let mut s = PlayerStats::default();
        let scale = read_medal(&renamed, &mut s).unwrap();
        assert_eq!((s.medal.as_str(), scale.distinction), ("distinction", 420.0));
        assert_eq!((s.off_factors.len(), s.sup_factors.len()), (1, 1));
        // Match sans médailles
        assert!(read_medal(&serde_json::json!({}), &mut PlayerStats::default()).is_none());
    }

    /// Médailles, score par manche et MVP d'un vrai match Riot (détail brut) ; écrit l'écran de fin
    /// de partie correspondant pour l'aperçu de l'interface :
    /// `VALO_RIOT_RAW=raw-match.json VALO_RESULT_OUT=result.json cargo test real_medals -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn real_medals() {
        let v: Value = serde_json::from_str(&std::fs::read_to_string(std::env::var("VALO_RIOT_RAW").unwrap()).unwrap()).unwrap();
        let m = parse_match(&v);
        let scale = m.perf_scale.expect("barème");
        assert!(!m.mvp.is_empty() && m.team_mvps.len() == 2);
        for (id, p) in &m.players {
            let expected = if p.perf >= scale.distinction { "distinction" } else if p.perf >= scale.merit { "merit" } else { "pass" };
            println!("{} {:<11} perf {:>3.0} att {:>3.0} {:<11} sou {:>3.0} {:<11} acs {:>3} manches {:?}", &id[..8], p.medal, p.perf, p.offense, p.off_trend, p.support, p.sup_trend, p.acs(), p.round_scores);
            assert_eq!(p.medal, expected);
            assert_eq!(p.round_scores.iter().sum::<u32>(), p.score, "score par manche");
            assert_eq!(p.round_scores.len(), m.rounds.len());
            assert!(p.round_kills.iter().sum::<u32>() <= p.kills);
        }
        let d = detail("sample", &m, Some("V26 · ACTE V".into()));
        let mvps: Vec<_> = d.teams.iter().flat_map(|t| &t.players).filter(|p| p.mvp).map(|p| p.puuid.clone()).collect();
        assert_eq!(mvps, vec![m.mvp.clone()]);
        if let Ok(out) = std::env::var("VALO_RESULT_OUT") {
            let me = std::env::var("VALO_PUUID").unwrap_or_else(|_| m.mvp.clone());
            let r = MatchResult { puuid: me, detail: d, rr: Some(RrChange { earned: 21, tier_before: 17, tier_after: 18, rr_before: 88, rr_after: 9, afk_penalty: 0 }), at: 0 };
            std::fs::write(out, serde_json::to_string_pretty(&r).unwrap()).unwrap();
        }
    }

    /// Écran de fin de partie du dernier match du joueur connecté (session Riot locale, lecture seule) :
    /// `cargo test real_result -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn real_result() {
        let riot = Riot::new();
        tauri::async_runtime::block_on(async {
            riot.ensure().await.unwrap();
            let me = riot.puuid();
            let hist = riot.pd(&format!("/match-history/v1/history/{me}?startIndex=0&endIndex=1")).await.unwrap().unwrap();
            let id = hist["History"][0]["MatchID"].as_str().unwrap().to_string();
            let v = riot.pd(&format!("/match-details/v1/matches/{id}")).await.unwrap().unwrap();
            let m = parse_match(&v);
            let d = detail(&id, &m, None);
            let line = d.teams.iter().flat_map(|t| &t.players).find(|p| p.puuid == me).unwrap();
            println!(
                "{} {} : médaille {:?} perf {:?} attaque {:?} {:?} soutien {:?} {:?} · ACS {} KAST {} % · MVP {} / équipe {} · manches {:?}",
                m.queue_id, &id[..8], line.medal, line.perf, line.offense, line.off_trend, line.support, line.sup_trend, line.acs, line.kast, line.mvp, line.team_mvp, line.round_scores
            );
            if m.queue_id == "competitive" {
                let rr = rr_change(&riot, &me, &id).await.expect("évolution du classement");
                println!("RR {rr:?}");
                assert!(rr.tier_after > 0);
            }
        });
    }

    /// Groupes au dernier match des joueurs de mon dernier match (session Riot locale, lecture seule) :
    /// pour ceux qui n'ont pas rejoué depuis, le résultat doit être exactement leur groupe de ce match.
    /// `cargo test real_party_hints -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn real_party_hints() {
        let dir = std::env::temp_dir().join(format!("valo-party-{}", std::process::id()));
        let cfg = crate::config::Config::default();
        let cache = CareerCache::new(Some(dir.clone()), Arc::new(Henrik::new(&cfg)), Arc::new(Directory::new(None)));
        let riot = Riot::new();
        tauri::async_runtime::block_on(async {
            riot.ensure().await.unwrap();
            let me = riot.puuid();
            // VALO_MATCH_INDEX : match plus ancien (ceux qui ont rejoué depuis sont alors comptés à part)
            let index: usize = std::env::var("VALO_MATCH_INDEX").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
            let hist = riot.pd(&format!("/match-history/v1/history/{me}?startIndex={index}&endIndex={}", index + 1)).await.unwrap().unwrap();
            let id = hist["History"][0]["MatchID"].as_str().unwrap().to_string();
            let m = match_cached(&riot, &cache, &id, "eu", false).await.unwrap().unwrap();
            let t0 = Instant::now();
            let (mut same, mut checked) = (0, 0);
            for (p, st) in &m.players {
                let last = riot.pd(&format!("/match-history/v1/history/{p}?startIndex=0&endIndex=1")).await.ok().flatten();
                if last.as_ref().and_then(|v| v["History"][0]["MatchID"].as_str()) != Some(id.as_str()) {
                    println!("{} : a rejoué depuis, ignoré", &p[..8]);
                    continue;
                }
                let mates = last_party(&riot, &cache, p).await;
                let mut truth: Vec<String> = m.players.iter().filter(|(q, s)| *q != p && s.party_id == st.party_id).map(|(q, _)| q.clone()).collect();
                let mut got = mates.clone().unwrap_or_default();
                truth.sort();
                got.sort();
                checked += 1;
                same += (got == truth) as u32;
                println!("{} : {} coéquipier(s) de groupe (réel {}){}", &p[..8], got.len(), truth.len(), if mates.is_none() { " — sans réponse" } else { "" });
            }
            println!("{same} / {checked} identiques au groupe réel, {:.1} s", t0.elapsed().as_secs_f32());
            assert!(same * 10 >= checked * 7, "trop d'écarts");
        });
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Parties non classées / combat à mort : rang actuel connu à la place de « Non classé ».
    #[test]
    fn unrated_match_shows_current_rank() {
        let people = Directory::new(None);
        people.remember(Known { puuid: "a".into(), name: "Nova".into(), tier: 21, ..Default::default() });
        let shared = Shared::default();
        let mut m = ParsedMatch { queue_id: "deathmatch".into(), ..Default::default() };
        for id in ["a", "b"] {
            m.players.insert(id.into(), PlayerStats { team: "Blue".into(), ..Default::default() });
        }
        m.teams.insert("Blue".into(), (true, 1));
        let mut d = detail("x", &m, None);
        fill_tiers(&mut d, &shared, &people);
        let tier = |d: &MatchDetail, id: &str| d.teams[0].players.iter().find(|p| p.puuid == id).unwrap().tier;
        assert_eq!((tier(&d, "a"), tier(&d, "b")), (21, 0));
        // En compétition, 0 = placements : on garde « Non classé »
        m.queue_id = "competitive".into();
        let mut d = detail("x", &m, None);
        fill_tiers(&mut d, &shared, &people);
        assert_eq!(tier(&d, "a"), 0);
    }
}

//! Boucle de suivi : détecte l'état du jeu (salon / sélection / en jeu), récupère
//! joueurs, pseudos et rangs, puis pousse un `Snapshot` à l'interface.

use crate::directory::{Directory, Known};
use crate::henrik::Henrik;
use crate::riot::{Conn, RateLimited, Riot};
use futures_util::{stream, StreamExt};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use std::io::Write;
use tauri::{AppHandle, Emitter, Manager, State};

const RANK_TTL: Duration = Duration::from_secs(600);
const RANK_ERROR_TTL: Duration = Duration::from_secs(60);
const CONTENT_TTL: Duration = Duration::from_secs(3600);
/// Début de l'épisode 5 : avant, les tiers 21-24 valaient Immortel 1-3 / Radiant.
const EP5_START: &str = "2022-06-22";

#[derive(Serialize, Clone, Copy, Default, PartialEq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    #[default]
    Offline,
    Waiting,
    Menus,
    Pregame,
    Ingame,
}

#[derive(Serialize, Clone, Default, PartialEq, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub phase: Phase,
    pub message: Option<String>,
    pub map_id: Option<String>,
    pub queue_id: Option<String>,
    pub provisioning_flow: Option<String>,
    pub act_name: Option<String>,
    pub phase_ends_at: Option<u64>,
    pub players: Vec<PlayerView>,
}

#[derive(Serialize, Clone, Default, PartialEq, Debug)]
#[serde(rename_all = "camelCase")]
pub struct PlayerView {
    pub puuid: String,
    pub name: Option<String>,
    pub tag: Option<String>,
    pub incognito: bool,
    pub team: String,
    pub is_me: bool,
    pub is_ally: bool,
    pub agent_id: Option<String>,
    pub agent_state: String,
    pub level: Option<u32>,
    pub card_id: Option<String>,
    pub party: Option<u32>,
    /// Groupe probable (même groupe lors du dernier match), pas confirmé par Riot
    pub party_guess: bool,
    /// `None` = en cours de chargement
    pub rank: Option<RankInfo>,
}

#[derive(Serialize, Clone, Default, PartialEq, Debug)]
#[serde(rename_all = "camelCase")]
pub struct RankInfo {
    pub tier: u32,
    pub rr: u32,
    pub wins: u32,
    pub games: u32,
    pub leaderboard: Option<u32>,
    /// Meilleur rang atteint pendant l'acte en cours
    pub act_peak_tier: u32,
    pub peak_tier: u32,
    pub peak_act: Option<String>,
    pub prev_tier: u32,
    pub prev_act: Option<String>,
    pub error: bool,
}

/// Bilan d'un acte classé (données officielles Riot).
#[derive(Serialize, Clone, Default, PartialEq, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ActRank {
    pub id: String,
    pub name: Option<String>,
    pub start: String,
    /// Rang de fin d'acte (ou actuel pour l'acte en cours)
    pub tier: u32,
    /// Meilleur rang atteint pendant l'acte
    pub peak: u32,
    pub games: u32,
    pub wins: u32,
    pub current: bool,
    /// Victoires obtenues à chaque rang pendant l'acte : [rang, victoires], du plus bas au plus haut
    pub wins_by_tier: Vec<[u32; 2]>,
}

/// Rang complet d'un joueur, pour sa carrière.
#[derive(Serialize, Clone, Default, Debug)]
#[serde(rename_all = "camelCase")]
pub struct RankDetail {
    #[serde(flatten)]
    pub rank: RankInfo,
    pub current_act_name: Option<String>,
    /// Actes joués, du plus ancien au plus récent
    pub history: Vec<ActRank>,
    pub total_games: u32,
    pub total_wins: u32,
}

/// Période couverte par un acte.
#[derive(Clone, Debug)]
pub struct ActWindow {
    pub id: String,
    pub name: String,
    pub start: u64,
    pub end: u64,
    pub current: bool,
}

#[derive(Default)]
pub struct Shared {
    snapshot: Mutex<Snapshot>,
    /// Actes connus (content-service), pour nommer les saisons et calculer les pics.
    content: Mutex<Content>,
    /// Groupes probables de la partie en cours (voir `career::spawn_party_hints`)
    hints: Mutex<PartyHints>,
    /// Rangs actuels vus pendant la session (joueurs incognito compris)
    tiers: Mutex<HashMap<String, u32>>,
}

/// Pour chaque joueur de la partie : ses coéquipiers de groupe lors de son dernier match.
#[derive(Default)]
struct PartyHints {
    match_id: String,
    mates: HashMap<String, Vec<String>>,
    pending: HashSet<String>,
    /// Joueurs sans réponse : nouvel essai après 30 s
    failed: HashMap<String, Instant>,
}

#[derive(Default, Clone)]
struct Content {
    current_act: Option<String>,
    acts: HashMap<String, Act>,
}

impl Shared {
    pub fn phase(&self) -> Phase {
        self.snapshot.lock().unwrap().phase
    }

    /// Rang actuel d'un joueur vu pendant la session (0 = inconnu ou non classé).
    pub fn live_tier(&self, puuid: &str) -> u32 {
        self.tiers.lock().unwrap().get(puuid).copied().unwrap_or(0)
    }

    /// Résultat pour un joueur (ignoré si la partie a changé entre-temps).
    pub fn set_party_hint(&self, match_id: &str, puuid: &str, mates: Option<Vec<String>>) {
        let mut h = self.hints.lock().unwrap();
        if h.match_id != match_id {
            return;
        }
        h.pending.remove(puuid);
        match mates {
            Some(mates) => {
                h.mates.insert(puuid.to_string(), mates);
            }
            None => {
                h.failed.insert(puuid.to_string(), Instant::now());
            }
        }
    }

    /// Joueurs encore à analyser pour cette partie (marqués « en cours »).
    fn claim_party_hints(&self, match_id: &str, puuids: Vec<String>) -> Vec<String> {
        let mut h = self.hints.lock().unwrap();
        if h.match_id != match_id {
            *h = PartyHints { match_id: match_id.to_string(), ..Default::default() };
        }
        let retry_later = |p: &String| h.failed.get(p).is_some_and(|t| t.elapsed() < Duration::from_secs(30));
        let todo: Vec<String> = puuids.into_iter().filter(|p| !h.mates.contains_key(p) && !h.pending.contains(p) && !retry_later(p)).collect();
        h.pending.extend(todo.iter().cloned());
        todo
    }

    fn party_mates(&self, match_id: &str) -> HashMap<String, Vec<String>> {
        let h = self.hints.lock().unwrap();
        if h.match_id == match_id { h.mates.clone() } else { HashMap::new() }
    }

    pub fn act_name(&self, id: &str) -> Option<String> {
        self.content.lock().unwrap().acts.get(id).map(|a| a.name.clone())
    }

    /// Acte demandé (ou l'acte en cours) : (id, nom, début ms, fin ms).
    pub fn act_window(&self, id: Option<&str>) -> Option<ActWindow> {
        let c = self.content.lock().unwrap();
        let id = id.map(String::from).or_else(|| c.current_act.clone())?;
        let act = c.acts.get(&id)?;
        Some(ActWindow {
            id: id.clone(),
            name: act.name.clone(),
            start: iso_ms(&act.start)?,
            end: iso_ms(&act.end).unwrap_or(u64::MAX),
            current: c.current_act.as_deref() == Some(id.as_str()),
        })
    }
}

#[tauri::command]
pub fn get_snapshot(shared: State<'_, Arc<Shared>>) -> Snapshot {
    shared.snapshot.lock().unwrap().clone()
}

/// Rang d'un joueur qui n'est pas dans la partie en cours (ouvert depuis le détail d'un match).
/// Sans session Riot (jeu et client fermés), le rang vient de HenrikDev.
#[tauri::command]
pub async fn get_rank(
    puuid: String,
    shared: State<'_, Arc<Shared>>,
    riot: State<'_, Arc<Riot>>,
    henrik: State<'_, Arc<Henrik>>,
    people: State<'_, Arc<Directory>>,
) -> Result<RankDetail, String> {
    // Riot d'abord, une seule fois ; s'il limite les requêtes, le relais répond tout de suite
    // (même rang, mêmes actes) au lieu de faire patienter l'écran.
    let path = format!("/mmr/v1/players/{puuid}");
    let mut error = String::from("profil compétitif introuvable");
    let mut riot_busy = false;
    if riot.connected() {
        match riot.pd(&path).await {
            Ok(Some(v)) => return Ok(rank_detail(&v, &shared)),
            Ok(None) => {}
            Err(e) => {
                riot_busy = true;
                error = format!("{e:#}");
            }
        }
    }
    if henrik.available() {
        let region = people.region_for(&puuid, &riot.region());
        let path = format!("valorant/v3/by-puuid/mmr/{region}/pc/{puuid}");
        // Quota du relais épuisé : on n'attend pas plus de quelques secondes
        let got = tokio::time::timeout(Duration::from_secs(8), henrik.get(&path)).await.ok().flatten();
        if let Some(v) = got {
            let content = shared.content.lock().unwrap().clone();
            return Ok(henrik_rank(&v["data"], content.current_act.as_deref(), &content.acts));
        }
    } else if !riot.connected() {
        error = "Ouvre le client Riot pour voir le rang de ce joueur".into();
    }
    if riot_busy {
        match riot.pd_patient(&path, Duration::from_secs(60)).await {
            Ok(Some(v)) => return Ok(rank_detail(&v, &shared)),
            Ok(None) => {}
            Err(e) => error = format!("{e:#}"),
        }
    }
    Err(error)
}

fn rank_detail(v: &Value, shared: &Shared) -> RankDetail {
    let content = shared.content.lock().unwrap().clone();
    let current = content.current_act.as_deref();
    let history = parse_history(v, current, &content.acts);
    RankDetail {
        rank: parse_mmr(v, current, &content.acts),
        current_act_name: current.and_then(|id| content.acts.get(id)).map(|a| a.name.clone()),
        total_games: history.iter().map(|a| a.games).sum(),
        total_wins: history.iter().map(|a| a.wins).sum(),
        history,
    }
}

/// Rang complet à partir de HenrikDev (v3 mmr) : mêmes informations que le profil Riot
/// (rang et RR actuels, pic, bilan et victoires par rang de chaque acte).
fn henrik_rank(d: &Value, current: Option<&str>, acts: &HashMap<String, Act>) -> RankDetail {
    let num = |x: &Value| x.as_u64().unwrap_or(0) as u32;
    let label = |id: &str, short: &str| acts.get(id).map(|a| a.name.clone()).or_else(|| short_act_label(short));
    let mut history: Vec<ActRank> = d["seasonal"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| {
            let id = s["season"]["id"].as_str()?.to_string();
            let games = num(&s["games"]);
            if games == 0 {
                return None;
            }
            let mut by_tier: HashMap<u32, u32> = HashMap::new();
            for w in s["act_wins"].as_array().into_iter().flatten() {
                let t = num(&w["id"]);
                if t > 0 {
                    *by_tier.entry(t).or_default() += 1;
                }
            }
            let mut wins_by_tier: Vec<[u32; 2]> = by_tier.into_iter().map(|(t, w)| [t, w]).collect();
            wins_by_tier.sort_unstable();
            let tier = num(&s["end_tier"]["id"]);
            Some(ActRank {
                name: label(&id, s["season"]["short"].as_str().unwrap_or("")),
                start: acts.get(&id).map(|a| a.start.clone()).unwrap_or_default(),
                peak: wins_by_tier.iter().map(|[t, _]| *t).fold(tier, u32::max),
                tier,
                games,
                wins: num(&s["wins"]),
                current: Some(id.as_str()) == current,
                wins_by_tier,
                id,
            })
        })
        .collect();
    // HenrikDev les donne déjà dans l'ordre ; les dates connues priment.
    if history.iter().all(|a| !a.start.is_empty()) {
        history.sort_by(|a, b| a.start.cmp(&b.start));
    }

    let cur = &d["current"];
    let mut r = RankInfo {
        tier: num(&cur["tier"]["id"]),
        rr: num(&cur["rr"]),
        leaderboard: cur["leaderboard_placement"]["rank"].as_u64().map(|l| l as u32).filter(|l| *l > 0),
        peak_tier: num(&d["peak"]["tier"]["id"]),
        peak_act: d["peak"]["season"]["id"]
            .as_str()
            .and_then(|id| label(id, d["peak"]["season"]["short"].as_str().unwrap_or(""))),
        ..Default::default()
    };
    if let Some(a) = history.iter().find(|a| a.current) {
        r.wins = a.wins;
        r.games = a.games;
        r.act_peak_tier = a.peak.max(r.tier);
    } else {
        r.act_peak_tier = r.tier;
    }
    if let Some(prev) = history.iter().rev().find(|a| !a.current && a.tier > 0) {
        r.prev_tier = prev.tier;
        r.prev_act = prev.name.clone();
    }
    r.peak_tier = r.peak_tier.max(history.iter().map(|a| a.peak).max().unwrap_or(0));
    RankDetail {
        current_act_name: current.and_then(|id| acts.get(id)).map(|a| a.name.clone()),
        total_games: history.iter().map(|a| a.games).sum(),
        total_wins: history.iter().map(|a| a.wins).sum(),
        rank: r,
        history,
    }
}

/// « e10a6 » → « E10 · ACTE VI » (acte inconnu de la liste officielle).
fn short_act_label(short: &str) -> Option<String> {
    let s = short.trim().to_lowercase();
    let (e, a) = s.strip_prefix('e')?.split_once('a')?;
    let (e, a): (u32, usize) = (e.parse().ok()?, a.parse().ok()?);
    let roman = ["", "I", "II", "III", "IV", "V", "VI", "VII", "VIII", "IX"].get(a)?;
    Some(format!("E{e} · ACTE {roman}"))
}

pub fn spawn(app: AppHandle, shared: Arc<Shared>, riot: Arc<Riot>) {
    tauri::async_runtime::spawn(async move {
        let mut t = Tracker::new(app, shared, riot);
        loop {
            let delay = t.tick().await;
            tokio::time::sleep(delay).await;
        }
    });
}

/// Joueur brut tel que renvoyé par core-game / pregame / parties.
struct RawPlayer {
    puuid: String,
    team: String,
    agent: Option<String>,
    agent_state: String,
    identity: Value,
}

#[derive(Clone)]
struct Act {
    name: String,
    start: String,
    end: String,
}

#[derive(Default)]
struct OwnParty {
    id: Option<String>,
    members: HashSet<String>,
}

struct Tracker {
    app: AppHandle,
    shared: Arc<Shared>,
    riot: Arc<Riot>,
    acts: HashMap<String, Act>,
    current_act: Option<String>,
    content_at: Option<Instant>,
    /// Liste des actes venue de Riot (sinon de valorant-api.com)
    content_riot: bool,
    content_retry: Option<Instant>,
    ranks: HashMap<String, (Instant, RankInfo)>,
    names: HashMap<String, (String, String)>,
    core_match: Option<(String, Value)>,
    /// Partie dont les groupes probables sont affichés
    hints_match: Option<String>,
    party_for: Option<String>,
    own_party: OwnParty,
    backoff_until: Option<Instant>,
    last: Snapshot,
    last_log: String,
}

impl Tracker {
    fn new(app: AppHandle, shared: Arc<Shared>, riot: Arc<Riot>) -> Self {
        Self {
            app,
            shared,
            riot,
            acts: HashMap::new(),
            current_act: None,
            content_at: None,
            content_riot: false,
            content_retry: None,
            ranks: HashMap::new(),
            names: HashMap::new(),
            core_match: None,
            hints_match: None,
            party_for: None,
            own_party: OwnParty::default(),
            backoff_until: None,
            last: Snapshot::default(),
            last_log: String::new(),
        }
    }

    /// Journal de diagnostic : %LOCALAPPDATA%r.valooverlay.applogsalo-overlay.log
    /// (une ligne par changement, le fichier repart à zéro au-delà de 256 Ko).
    fn log(&mut self, line: String) {
        if line == self.last_log {
            return;
        }
        self.last_log = line.clone();
        diag(&self.app, &line);
    }

    fn publish(&mut self, snap: Snapshot) {
        if snap == self.last {
            return;
        }
        // Pseudos visibles (incognito respecté) : suggestions de recherche
        if !snap.players.is_empty() {
            let people = self.app.state::<Arc<Directory>>();
            let region = self.riot.region();
            for p in snap.players.iter().filter(|p| p.name.is_some()) {
                people.remember(Known {
                    puuid: p.puuid.clone(),
                    name: p.name.clone().unwrap_or_default(),
                    tag: p.tag.clone().unwrap_or_default(),
                    card_id: p.card_id.clone().unwrap_or_default(),
                    tier: p.rank.as_ref().map_or(0, |r| r.tier),
                    level: p.level.unwrap_or(0),
                    region: region.clone(),
                    seen: 0,
                });
            }
        }
        self.last = snap.clone();
        *self.shared.snapshot.lock().unwrap() = snap.clone();
        let _ = self.app.emit("snapshot", &snap);
    }

    fn status(&mut self, phase: Phase, message: Option<String>) {
        self.core_match = None;
        self.party_for = None;
        self.publish(Snapshot { phase, message, ..Default::default() });
    }

    async fn tick(&mut self) -> Duration {
        let conn = self.riot.ensure().await;
        if self.riot.connected() {
            let people = self.app.state::<Arc<Directory>>();
            people.set_region(&self.riot.region());
        }
        self.ensure_content().await;
        match conn {
            Ok(Conn::Ready) => {}
            Ok(Conn::NoClient) => {
                self.status(Phase::Offline, None);
                return Duration::from_secs(5);
            }
            Ok(Conn::NotLoggedIn) => {
                self.status(Phase::Offline, Some("Connecte-toi au client Riot.".into()));
                return Duration::from_secs(5);
            }
            Ok(Conn::NoValorant) => {
                // Jeu fermé juste après une partie : son résultat reste disponible
                self.game_over();
                self.status(Phase::Waiting, None);
                return Duration::from_secs(4);
            }
            Err(e) => {
                self.status(Phase::Waiting, Some(format!("Connexion aux serveurs Riot… ({e})")));
                return Duration::from_secs(5);
            }
        }


        let me = self.riot.puuid();
        let presences = self.riot.presences().await.unwrap_or_default();
        let own = presences.iter().find(|(p, _)| *p == me).map(|(_, v)| v.clone());
        let state = own
            .as_ref()
            .and_then(|v| str_at(v, &["/matchPresenceData/sessionLoopState", "/sessionLoopState"]));
        let party_of: HashMap<String, String> = presences
            .iter()
            .filter_map(|(p, v)| Some((p.clone(), str_at(v, &["/partyPresenceData/partyId", "/partyId"])?)))
            .collect();

        let result = match state.as_deref() {
            Some("INGAME") => self.ingame(&party_of).await.map(|ok| (ok, 5)),
            Some("PREGAME") => self.pregame(&party_of).await.map(|ok| (ok, 2)),
            Some("MENUS") => self.menus(own.as_ref()).await.map(|_| (true, 6)),
            // Présence absente ou inconnue : on sonde directement les serveurs.
            _ => match self.ingame(&party_of).await {
                Ok(true) => Ok((true, 5)),
                _ => match self.pregame(&party_of).await {
                    Ok(true) => Ok((true, 2)),
                    _ => self.menus(own.as_ref()).await.map(|_| (true, 6)),
                },
            },
        };

        match result {
            // État annoncé par la présence mais pas encore côté serveur (transition) : on retente vite.
            Ok((false, _)) => Duration::from_secs(2),
            Ok((true, secs)) => Duration::from_secs(secs),
            Err(e) => {
                eprintln!("suivi : {e:#}");
                Duration::from_secs(4)
            }
        }
    }

    /// Liste des actes : Riot quand une session existe, sinon valorant-api.com (recherche et
    /// carrières sans le jeu). Rechargée toutes les heures, ou dès que Riot devient disponible.
    async fn ensure_content(&mut self) {
        let riot = self.riot.connected();
        let due = self.content_at.is_none_or(|t| t.elapsed() > CONTENT_TTL) || (riot && !self.content_riot);
        if !due || self.content_retry.is_some_and(|t| Instant::now() < t) {
            return;
        }
        let res = if riot { self.load_content().await } else { Err(anyhow::anyhow!("pas de session Riot")) };
        match res {
            Ok(()) => self.content_riot = true,
            Err(e) => {
                if riot {
                    eprintln!("content-service : {e:#}");
                }
                if self.acts.is_empty() || !riot {
                    match self.load_public_content().await {
                        Ok(()) => self.content_riot = false,
                        Err(e) => eprintln!("valorant-api seasons : {e:#}"),
                    }
                }
                self.content_retry = Some(Instant::now() + Duration::from_secs(60));
            }
        }
    }

    async fn load_public_content(&mut self) -> anyhow::Result<()> {
        let v = self.riot.public_json("https://valorant-api.com/v1/seasons").await?;
        let (acts, current) = public_acts(&v, &now_iso());
        if acts.is_empty() {
            anyhow::bail!("aucun acte");
        }
        self.acts = acts;
        self.current_act = current;
        self.content_at = Some(Instant::now());
        *self.shared.content.lock().unwrap() = Content { current_act: self.current_act.clone(), acts: self.acts.clone() };
        Ok(())
    }

    async fn load_content(&mut self) -> anyhow::Result<()> {
        let Some(v) = self.riot.shared("/content-service/v3/content").await? else {
            anyhow::bail!("contenu introuvable");
        };
        let seasons = v["Seasons"].as_array().cloned().unwrap_or_default();
        let mut episodes: Vec<(String, String)> = seasons
            .iter()
            .filter(|s| s["Type"].as_str().is_some_and(|t| t.eq_ignore_ascii_case("episode")))
            .map(|s| (s["StartTime"].as_str().unwrap_or("").to_string(), s["Name"].as_str().unwrap_or("").to_string()))
            .collect();
        episodes.sort();
        self.acts.clear();
        for s in seasons.iter().filter(|s| s["Type"].as_str().is_some_and(|t| t.eq_ignore_ascii_case("act"))) {
            let (Some(id), Some(start)) = (s["ID"].as_str(), s["StartTime"].as_str()) else { continue };
            let episode = episodes.iter().rev().find(|(st, _)| st.as_str() <= start).map(|(_, n)| n.as_str());
            let name = act_label(episode, s["Name"].as_str().unwrap_or(""));
            if s["IsActive"].as_bool().unwrap_or(false) {
                self.current_act = Some(id.to_string());
            }
            let end = s["EndTime"].as_str().unwrap_or("").to_string();
            self.acts.insert(id.to_string(), Act { name, start: start.to_string(), end });
        }
        self.content_at = Some(Instant::now());
        *self.shared.content.lock().unwrap() = Content { current_act: self.current_act.clone(), acts: self.acts.clone() };
        Ok(())
    }

    async fn ingame(&mut self, party_of: &HashMap<String, String>) -> anyhow::Result<bool> {
        let me = self.riot.puuid();
        let Some(p) = self.riot.glz(&format!("/core-game/v1/players/{me}")).await? else { return Ok(false) };
        let Some(match_id) = p["MatchID"].as_str().map(String::from) else { return Ok(false) };

        if self.core_match.as_ref().map(|(id, _)| id) != Some(&match_id) {
            let Some(m) = self.riot.glz(&format!("/core-game/v1/matches/{match_id}")).await? else {
                return Ok(false);
            };
            self.core_match = Some((match_id.clone(), m));
        }
        self.refresh_own_party(&match_id).await;

        let m = self.core_match.as_ref().map(|(_, m)| m.clone()).unwrap_or_default();
        let raws: Vec<RawPlayer> = m["Players"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|pl| {
                Some(RawPlayer {
                    puuid: pl["Subject"].as_str()?.to_string(),
                    team: pl["TeamID"].as_str().unwrap_or("").to_string(),
                    agent: pl["CharacterID"].as_str().filter(|s| !s.is_empty()).map(String::from),
                    agent_state: "locked".into(),
                    identity: pl["PlayerIdentity"].clone(),
                })
            })
            .collect();

        self.request_party_hints(&match_id, &raws, party_of);
        let snap = Snapshot {
            phase: Phase::Ingame,
            map_id: m["MapID"].as_str().map(String::from),
            queue_id: m["MatchmakingData"]["QueueID"].as_str().map(String::from),
            provisioning_flow: m["ProvisioningFlow"].as_str().map(String::from),
            ..Default::default()
        };
        self.hints_match = Some(match_id);
        self.enrich_and_publish(snap, raws, party_of, true).await;
        Ok(true)
    }

    /// Groupes que Riot ne montre pas (joueurs qui ne sont pas tes amis) : déduits en arrière-plan
    /// du dernier match de chaque joueur. Toi, ton groupe et tes amis sont déjà connus.
    fn request_party_hints(&mut self, match_id: &str, raws: &[RawPlayer], party_of: &HashMap<String, String>) {
        let me = self.riot.puuid();
        let unknown: Vec<String> = raws
            .iter()
            .map(|r| r.puuid.clone())
            .filter(|p| *p != me && !self.own_party.members.contains(p) && !party_of.contains_key(p))
            .collect();
        let todo = self.shared.claim_party_hints(match_id, unknown);
        if todo.is_empty() {
            return;
        }
        let cache = self.app.state::<Arc<crate::career::CareerCache>>().inner().clone();
        crate::career::spawn_party_hints(self.app.clone(), self.riot.clone(), cache, self.shared.clone(), match_id.to_string(), todo);
    }

    async fn pregame(&mut self, party_of: &HashMap<String, String>) -> anyhow::Result<bool> {
        let me = self.riot.puuid();
        let Some(p) = self.riot.glz(&format!("/pregame/v1/players/{me}")).await? else { return Ok(false) };
        let Some(match_id) = p["MatchID"].as_str().map(String::from) else { return Ok(false) };
        let Some(m) = self.riot.glz(&format!("/pregame/v1/matches/{match_id}")).await? else { return Ok(false) };
        self.game_over();
        self.refresh_own_party(&match_id).await;

        let team = &m["AllyTeam"];
        let team_id = team["TeamID"].as_str().unwrap_or("").to_string();
        self.hints_match = Some(match_id.clone());
        let raws: Vec<RawPlayer> = team["Players"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|pl| {
                Some(RawPlayer {
                    puuid: pl["Subject"].as_str()?.to_string(),
                    team: team_id.clone(),
                    agent: pl["CharacterID"].as_str().filter(|s| !s.is_empty()).map(String::from),
                    agent_state: pl["CharacterSelectionState"].as_str().unwrap_or("").to_string(),
                    identity: pl["PlayerIdentity"].clone(),
                })
            })
            .collect();

        self.request_party_hints(&match_id, &raws, party_of);
        let ends_at = m["PhaseTimeRemainingNS"].as_f64().map(|ns| now_ms() + (ns / 1e6) as u64);
        let snap = Snapshot {
            phase: Phase::Pregame,
            map_id: m["MapID"].as_str().map(String::from),
            queue_id: m["QueueID"].as_str().map(String::from),
            provisioning_flow: m["ProvisioningFlowID"].as_str().map(String::from),
            phase_ends_at: ends_at,
            ..Default::default()
        };
        self.enrich_and_publish(snap, raws, party_of, true).await;
        Ok(true)
    }

    /// On quitte une partie : l'écran de fin de partie se prépare en arrière-plan.
    fn game_over(&mut self) {
        let Some((match_id, m)) = self.core_match.take() else { return };
        if m["ProvisioningFlow"].as_str() == Some("ShootingRange") {
            return;
        }
        // Pseudos affichés pendant la partie (None = incognito, masqué aussi dans le résultat)
        let names = self
            .last
            .players
            .iter()
            .map(|p| (p.puuid.clone(), p.name.clone().map(|n| (n, p.tag.clone().unwrap_or_default()))))
            .collect();
        let cache = self.app.state::<Arc<crate::career::CareerCache>>().inner().clone();
        crate::career::spawn_result(self.app.clone(), self.riot.clone(), cache, self.shared.clone(), match_id, names);
    }

    async fn menus(&mut self, own: Option<&Value>) -> anyhow::Result<()> {
        self.game_over();
        self.party_for = None;
        let me = self.riot.puuid();
        let mut queue = None;
        let mut raws = Vec::new();

        if let Some(pp) = self.riot.glz(&format!("/parties/v1/players/{me}")).await? {
            if let Some(pid) = pp["CurrentPartyID"].as_str() {
                if let Some(party) = self.riot.glz(&format!("/parties/v1/parties/{pid}")).await? {
                    queue = party["MatchmakingData"]["QueueID"].as_str().map(String::from);
                    self.own_party = OwnParty { id: Some(pid.to_string()), members: HashSet::new() };
                    for m in party["Members"].as_array().into_iter().flatten() {
                        let Some(puuid) = m["Subject"].as_str() else { continue };
                        self.own_party.members.insert(puuid.to_string());
                        raws.push(RawPlayer {
                            puuid: puuid.to_string(),
                            team: String::new(),
                            agent: None,
                            agent_state: String::new(),
                            identity: m["PlayerIdentity"].clone(),
                        });
                    }
                }
            }
        }

        if raws.is_empty() {
            // Repli : seulement toi, avec les infos de ta présence.
            let pres = own.cloned().unwrap_or_default();
            raws.push(RawPlayer {
                puuid: me.clone(),
                team: String::new(),
                agent: None,
                agent_state: String::new(),
                identity: json!({
                    "PlayerCardID": str_at(&pres, &["/playerPresenceData/playerCardId", "/playerCardId"]),
                    "AccountLevel": pres.pointer("/playerPresenceData/accountLevel").or(pres.get("accountLevel")),
                }),
            });
        }
        // Toi en premier.
        raws.sort_by_key(|r| r.puuid != me);

        let snap = Snapshot { phase: Phase::Menus, queue_id: queue, ..Default::default() };
        self.enrich_and_publish(snap, raws, &HashMap::new(), false).await;
        Ok(())
    }

    /// Ton groupe actuel, utile pour marquer tes coéquipiers et révéler leurs pseudos.
    async fn refresh_own_party(&mut self, match_id: &str) {
        if self.party_for.as_deref() == Some(match_id) {
            return;
        }
        self.party_for = Some(match_id.to_string());
        let me = self.riot.puuid();
        let Ok(Some(pp)) = self.riot.glz(&format!("/parties/v1/players/{me}")).await else { return };
        let Some(pid) = pp["CurrentPartyID"].as_str().map(String::from) else { return };
        let Ok(Some(party)) = self.riot.glz(&format!("/parties/v1/parties/{pid}")).await else { return };
        let members = party["Members"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|m| m["Subject"].as_str().map(String::from))
            .collect();
        self.own_party = OwnParty { id: Some(pid), members };
    }

    async fn enrich_and_publish(
        &mut self,
        mut snap: Snapshot,
        mut raws: Vec<RawPlayer>,
        party_of: &HashMap<String, String>,
        show_parties: bool,
    ) {
        let me = self.riot.puuid();
        snap.act_name = self.current_act.as_ref().and_then(|id| self.acts.get(id)).map(|a| a.name.clone());
        let my_team = raws.iter().find(|r| r.puuid == me).map(|r| r.team.clone()).unwrap_or_default();

        // Ordre stable : ton équipe d'abord, toi en tête.
        raws.sort_by_key(|r| (r.team != my_team, r.puuid != me));

        // Pseudos manquants, en une seule requête.
        let missing: Vec<&str> =
            raws.iter().map(|r| r.puuid.as_str()).filter(|p| !self.names.contains_key(*p)).collect();
        if !missing.is_empty() {
            if let Ok(Some(v)) = self.riot.pd_put("/name-service/v2/players", json!(missing)).await {
                for e in v.as_array().into_iter().flatten() {
                    if let (Some(id), Some(n), Some(t)) =
                        (e["Subject"].as_str(), e["GameName"].as_str(), e["TagLine"].as_str())
                    {
                        if !n.is_empty() {
                            self.names.insert(id.to_string(), (n.to_string(), t.to_string()));
                        }
                    }
                }
            }
        }

        if !show_parties {
            self.hints_match = None;
        }
        let (groups, guessed) = if show_parties { self.party_groups(&raws, party_of) } else { Default::default() };
        if show_parties {
            let seen = raws.iter().filter(|r| party_of.contains_key(&r.puuid)).count();
            let mut sizes: HashMap<u32, u32> = HashMap::new();
            for g in groups.values() {
                *sizes.entry(*g).or_default() += 1;
            }
            let mut sizes: Vec<u32> = sizes.into_values().collect();
            sizes.sort_unstable_by(|a, b| b.cmp(a));
            self.log(format!(
                "{:?} : {} joueurs, présence connue pour {}, ton groupe {} membre(s), groupes détectés {:?} (dont {} joueur(s) d'après leur dernier match)",
                snap.phase,
                raws.len(),
                seen,
                self.own_party.members.len(),
                sizes,
                guessed.len()
            ));
        }
        snap.players = raws.iter().map(|r| self.view(r, &me, &my_team, &groups, &guessed)).collect();
        self.publish(snap.clone());

        // Rangs, 3 à la fois (au lieu d'un par un) ; l'interface se remplit au fur et à mesure.
        if self.backoff_until.is_some_and(|t| Instant::now() < t) {
            return;
        }
        let todo: Vec<String> = raws.iter().map(|r| r.puuid.clone()).filter(|p| self.rank_stale(p)).collect();
        let ctx = RankCtx { riot: self.riot.clone(), app: self.app.clone(), current: self.current_act.clone(), acts: Arc::new(self.acts.clone()) };
        let mut ranks = stream::iter(todo)
            .map(|puuid| {
                let ctx = ctx.clone();
                async move {
                    let r = ctx.fetch(&puuid).await;
                    (puuid, r)
                }
            })
            .buffer_unordered(3);
        while let Some((puuid, res)) = ranks.next().await {
            let rank = match res {
                Ok(r) => r,
                Err(e) if e.is::<RateLimited>() => {
                    self.backoff_until = Some(Instant::now() + Duration::from_secs(20));
                    break;
                }
                Err(e) => {
                    eprintln!("mmr {puuid} : {e:#}");
                    RankInfo { error: true, ..Default::default() }
                }
            };
            if rank.tier > 0 {
                self.shared.tiers.lock().unwrap().insert(puuid.clone(), rank.tier);
            }
            self.ranks.insert(puuid.clone(), (Instant::now(), rank.clone()));
            if let Some(p) = snap.players.iter_mut().find(|p| p.puuid == puuid) {
                p.rank = Some(rank);
            }
            self.publish(snap.clone());
        }
    }

    fn rank_stale(&self, puuid: &str) -> bool {
        match self.ranks.get(puuid) {
            None => true,
            Some((at, r)) => at.elapsed() > if r.error { RANK_ERROR_TTL } else { RANK_TTL },
        }
    }

    fn view(&self, r: &RawPlayer, me: &str, my_team: &str, groups: &HashMap<String, u32>, guessed: &HashSet<String>) -> PlayerView {
        let id = &r.identity;
        let is_me = r.puuid == me;
        let known = is_me || self.own_party.members.contains(&r.puuid);
        let incognito = id["Incognito"].as_bool().unwrap_or(false);
        // On respecte le mode incognito : pseudo masqué sauf pour toi et ton groupe.
        let (name, tag) = match self.names.get(&r.puuid) {
            Some((n, t)) if !incognito || known => (Some(n.clone()), Some(t.clone())),
            _ => (None, None),
        };
        let hide_level = id["HideAccountLevel"].as_bool().unwrap_or(false) && !known;
        PlayerView {
            puuid: r.puuid.clone(),
            name,
            tag,
            incognito,
            team: r.team.clone(),
            is_me,
            is_ally: r.team == my_team,
            agent_id: r.agent.clone(),
            agent_state: r.agent_state.clone(),
            level: id["AccountLevel"].as_u64().filter(|l| *l > 0 && !hide_level).map(|l| l as u32),
            card_id: id["PlayerCardID"].as_str().filter(|s| !s.is_empty()).map(String::from),
            party: groups.get(&r.puuid).copied(),
            party_guess: guessed.contains(&r.puuid),
            rank: self.ranks.get(&r.puuid).map(|(_, rk)| rk.clone()),
        }
    }

    /// Numérote les groupes (≥ 2 joueurs dans la partie). Ton groupe = 0.
    /// Groupes connus (ton groupe, tes amis) + groupes probables (même groupe au dernier match,
    /// même équipe maintenant). Renvoie aussi les joueurs dont le groupe est seulement probable.
    fn party_groups(&self, raws: &[RawPlayer], party_of: &HashMap<String, String>) -> (HashMap<String, u32>, HashSet<String>) {
        let known = |p: &str| -> Option<String> {
            if self.own_party.members.contains(p) {
                self.own_party.id.clone()
            } else {
                party_of.get(p).cloned()
            }
        };
        let players: Vec<(String, String, Option<String>)> = raws.iter().map(|r| (r.puuid.clone(), r.team.clone(), known(&r.puuid))).collect();
        let mates = self.hints_match.as_deref().map(|m| self.shared.party_mates(m)).unwrap_or_default();
        let (root, mut guessed) = merge_hints(&players, self.own_party.id.as_deref(), &mates);
        let pid = |p: &str| -> Option<String> { root.get(p).cloned() };
        let mut count: HashMap<String, u32> = HashMap::new();
        for r in raws {
            if let Some(id) = pid(&r.puuid) {
                *count.entry(id).or_default() += 1;
            }
        }
        let mut order: Vec<String> = Vec::new();
        if let Some(own) = &self.own_party.id {
            if count.get(own).copied().unwrap_or(0) >= 2 {
                order.push(own.clone());
            }
        }
        for r in raws {
            if let Some(id) = pid(&r.puuid) {
                if count[&id] >= 2 && !order.contains(&id) {
                    order.push(id);
                }
            }
        }
        let groups: HashMap<String, u32> = raws
            .iter()
            .filter_map(|r| {
                let id = pid(&r.puuid)?;
                let idx = order.iter().position(|o| *o == id)?;
                Some((r.puuid.clone(), idx as u32))
            })
            .collect();
        guessed.retain(|p| groups.contains_key(p));
        (groups, guessed)
    }

}

/// De quoi lire des rangs en parallèle, sans bloquer la boucle de suivi.
#[derive(Clone)]
struct RankCtx {
    riot: Arc<Riot>,
    app: AppHandle,
    current: Option<String>,
    acts: Arc<HashMap<String, Act>>,
}

impl RankCtx {
    async fn fetch(&self, puuid: &str) -> anyhow::Result<RankInfo> {
        match self.riot.pd(&format!("/mmr/v1/players/{puuid}")).await {
            Ok(Some(v)) => Ok(parse_mmr(&v, self.current.as_deref(), &self.acts)),
            Ok(None) => Err(anyhow::anyhow!("profil compétitif introuvable")),
            // Riot limite : même rang via le relais HenrikDev (sans attendre son quota)
            Err(e) if e.is::<RateLimited>() => self.relay(puuid).await.ok_or(e),
            Err(e) => Err(e),
        }
    }

    async fn relay(&self, puuid: &str) -> Option<RankInfo> {
        let henrik = self.app.state::<Arc<Henrik>>();
        if !henrik.available() {
            return None;
        }
        let region = self.app.state::<Arc<Directory>>().region_for(puuid, &self.riot.region());
        let v = henrik.try_get(&format!("valorant/v3/by-puuid/mmr/{region}/pc/{puuid}")).await?;
        Some(henrik_rank(&v["data"], self.current.as_deref(), &self.acts).rank)
    }
}

/// Clé de groupe de chaque joueur : groupe connu (ton groupe, tes amis), sinon lui-même, puis
/// fusion des joueurs d'une même équipe qui étaient ensemble à leur dernier match. Ton groupe est
/// connu exactement (personne n'y est ajouté) et deux groupes connus ne sont jamais fusionnés.
/// Renvoie aussi les joueurs rattachés seulement par déduction.
fn merge_hints(players: &[(String, String, Option<String>)], own: Option<&str>, mates: &HashMap<String, Vec<String>>) -> (HashMap<String, String>, HashSet<String>) {
    let mut root: HashMap<String, String> = players.iter().map(|(p, _, k)| (p.clone(), k.clone().unwrap_or_else(|| p.clone()))).collect();
    let team: HashMap<&str, &str> = players.iter().map(|(p, t, _)| (p.as_str(), t.as_str())).collect();
    let known: HashSet<&str> = players.iter().filter_map(|(_, _, k)| k.as_deref()).collect();
    let mut guessed = HashSet::new();
    // Ordre fixe : même résultat à chaque passage de la boucle de suivi
    let mut list: Vec<(&String, &Vec<String>)> = mates.iter().collect();
    list.sort();
    for (p, qs) in list {
        for q in qs.iter().filter(|q| team.contains_key(q.as_str()) && team.get(q.as_str()) == team.get(p.as_str())) {
            let (Some(a), Some(b)) = (root.get(p).cloned(), root.get(q).cloned()) else { continue };
            let (ka, kb) = (known.contains(a.as_str()), known.contains(b.as_str()));
            if a == b || (ka && kb) || own.is_some_and(|o| a == o || b == o) {
                continue;
            }
            let (from, to) = if kb { (a, b) } else { (b, a) };
            for v in root.values_mut().filter(|v| **v == from) {
                *v = to.clone();
            }
            guessed.insert(p.clone());
            guessed.insert(q.clone());
        }
    }
    guessed.retain(|p| players.iter().any(|(id, _, k)| id == p && k.is_none()));
    (root, guessed)
}

fn parse_mmr(v: &Value, current: Option<&str>, acts: &HashMap<String, Act>) -> RankInfo {
    let mut r = RankInfo::default();
    let Some(seasons) = v.pointer("/QueueSkills/competitive/SeasonalInfoBySeasonID").and_then(Value::as_object) else {
        return r;
    };
    let num = |x: &Value| x.as_u64().unwrap_or(0) as u32;

    if let Some(cur) = current.and_then(|c| seasons.get(c)) {
        r.tier = num(&cur["CompetitiveTier"]);
        r.rr = num(&cur["RankedRating"]);
        r.wins = num(&cur["NumberOfWinsWithPlacements"]).max(num(&cur["NumberOfWins"]));
        r.games = num(&cur["NumberOfGames"]);
        r.leaderboard = Some(num(&cur["LeaderboardRank"])).filter(|l| *l > 0);
        r.act_peak_tier = cur["WinsByTier"]
            .as_object()
            .into_iter()
            .flat_map(|m| m.keys())
            .filter_map(|k| k.parse::<u32>().ok())
            .fold(r.tier, u32::max);
    }

    let mut prev_start = "";
    for (id, s) in seasons {
        let act = acts.get(id);
        let start = act.map_or("", |a| a.start.as_str());
        let fix = |t: u32| if !start.is_empty() && start < EP5_START && t >= 21 { t + 3 } else { t };

        let mut best = fix(num(&s["CompetitiveTier"]));
        for k in s["WinsByTier"].as_object().into_iter().flat_map(|m| m.keys()) {
            if let Ok(t) = k.parse::<u32>() {
                best = best.max(fix(t));
            }
        }
        if best > r.peak_tier {
            r.peak_tier = best;
            r.peak_act = act.map(|a| a.name.clone());
        }

        let final_tier = fix(num(&s["CompetitiveTier"]));
        if Some(id.as_str()) != current && final_tier > 0 && start >= prev_start {
            prev_start = start;
            r.prev_tier = final_tier;
            r.prev_act = act.map(|a| a.name.clone());
        }
    }
    r
}

/// Bilan de chaque acte joué en compétition, du plus ancien au plus récent.
fn parse_history(v: &Value, current: Option<&str>, acts: &HashMap<String, Act>) -> Vec<ActRank> {
    let Some(seasons) = v.pointer("/QueueSkills/competitive/SeasonalInfoBySeasonID").and_then(Value::as_object) else {
        return Vec::new();
    };
    let num = |x: &Value| x.as_u64().unwrap_or(0) as u32;
    let mut out: Vec<ActRank> = seasons
        .iter()
        .filter_map(|(id, s)| {
            let games = num(&s["NumberOfGames"]);
            if games == 0 {
                return None;
            }
            let act = acts.get(id);
            let start = act.map_or(String::new(), |a| a.start.clone());
            let fix = |t: u32| if !start.is_empty() && start.as_str() < EP5_START && t >= 21 { t + 3 } else { t };
            let tier = fix(num(&s["CompetitiveTier"]));
            let mut wins_by_tier: Vec<[u32; 2]> = s["WinsByTier"]
                .as_object()
                .into_iter()
                .flatten()
                .filter_map(|(k, v)| Some([fix(k.parse::<u32>().ok()?), v.as_u64().unwrap_or(0) as u32]))
                .filter(|[t, w]| *t > 0 && *w > 0)
                .collect();
            wins_by_tier.sort_unstable();
            let peak = wins_by_tier.iter().map(|[t, _]| *t).fold(tier, u32::max);
            Some(ActRank {
                id: id.clone(),
                name: act.map(|a| a.name.clone()),
                start,
                tier,
                peak,
                games,
                wins: num(&s["NumberOfWinsWithPlacements"]).max(num(&s["NumberOfWins"])),
                current: Some(id.as_str()) == current,
                wins_by_tier,
            })
        })
        .collect();
    out.sort_by(|a, b| a.start.cmp(&b.start));
    out
}

/// Actes de valorant-api.com/v1/seasons (mêmes identifiants que Riot), et l'acte en cours à `now`.
fn public_acts(v: &Value, now: &str) -> (HashMap<String, Act>, Option<String>) {
    let seasons = v["data"].as_array().cloned().unwrap_or_default();
    let episode_name: HashMap<&str, &str> = seasons
        .iter()
        .filter(|s| s["parentUuid"].is_null())
        .filter_map(|s| Some((s["uuid"].as_str()?, s["displayName"].as_str()?)))
        .collect();
    let mut acts = HashMap::new();
    let mut current = None;
    for s in seasons.iter().filter(|s| s["type"].as_str().is_some_and(|t| t.ends_with("::Act"))) {
        let (Some(id), Some(start)) = (s["uuid"].as_str(), s["startTime"].as_str()) else { continue };
        let end = s["endTime"].as_str().unwrap_or("");
        let episode = s["parentUuid"].as_str().and_then(|p| episode_name.get(p).copied());
        if start <= now && (end.is_empty() || now < end) {
            current = Some(id.to_string());
        }
        let name = act_label(episode, s["displayName"].as_str().unwrap_or(""));
        acts.insert(id.to_string(), Act { name, start: start.to_string(), end: end.to_string() });
    }
    (acts, current)
}

/// Date actuelle au format ISO (comparable aux dates des actes).
fn now_iso() -> String {
    let secs = now_ms() / 1000;
    let days = (secs / 86_400) as i64;
    // Inverse de « days from civil »
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + (m <= 2) as i64;
    let rem = secs % 86_400;
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem / 60 % 60, rem % 60)
}

/// "EPISODE 9" + "ACT III" → "E9 · ACTE III" ; les nouveaux noms ("V25") restent tels quels.
fn act_label(episode: Option<&str>, act: &str) -> String {
    let act = act.trim().to_uppercase();
    let act = act.strip_prefix("ACT ").map_or(act.clone(), |r| format!("ACTE {r}"));
    match episode.map(|e| e.trim().to_uppercase()) {
        Some(e) => {
            let e = e.strip_prefix("EPISODE ").map_or(e.clone(), |n| format!("E{n}"));
            format!("{e} · {act}")
        }
        None => act,
    }
}

/// Ajoute une ligne au journal de diagnostic
/// (%LOCALAPPDATA%\fr.valooverlay.app\logs\valo-overlay.log, remis à zéro au-delà de 256 Ko).
pub fn diag(app: &AppHandle, line: &str) {
    let Ok(dir) = app.path().app_log_dir() else { return };
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("valo-overlay.log");
    let too_big = std::fs::metadata(&path).is_ok_and(|m| m.len() > 256 * 1024);
    let file = std::fs::OpenOptions::new().create(true).append(!too_big).write(true).truncate(too_big).open(&path);
    if let Ok(mut f) = file {
        let _ = writeln!(f, "[{}] {line}", now_ms() / 1000);
    }
}

/// "2025-06-24T12:00:00Z" → millisecondes depuis 1970 (UTC).
pub fn iso_ms(s: &str) -> Option<u64> {
    let (date, time) = s.trim().split_once('T')?;
    let mut d = date.split('-').map(|x| x.parse::<i64>().ok());
    let (y, m, day) = (d.next()??, d.next()??, d.next()??);
    let time = time.trim_end_matches('Z');
    let time = time.split(['+']).next().unwrap_or(time);
    let mut t = time.split(':');
    let h: i64 = t.next()?.parse().ok()?;
    let mi: i64 = t.next().unwrap_or("0").parse().ok()?;
    let sec: f64 = t.next().unwrap_or("0").parse().ok()?;
    // Jours depuis 1970 (algorithme « days from civil »)
    let y2 = if m <= 2 { y - 1 } else { y };
    let era = if y2 >= 0 { y2 } else { y2 - 399 } / 400;
    let yoe = y2 - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let ms = (days * 86_400 + h * 3600 + mi * 60) as f64 * 1000.0 + sec * 1000.0;
    (ms >= 0.0).then_some(ms as u64)
}

fn str_at(v: &Value, pointers: &[&str]) -> Option<String> {
    pointers
        .iter()
        .find_map(|p| v.pointer(p).and_then(Value::as_str))
        .filter(|s| !s.is_empty())
        .map(String::from)
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mmr_current_peak_and_old_episode_fix() {
        let mut acts = HashMap::new();
        acts.insert("old".to_string(), Act { name: "E4 · ACTE III".into(), start: "2022-03-01T00:00:00Z".into(), end: String::new() });
        acts.insert("cur".to_string(), Act { name: "V26 · ACTE II".into(), start: "2026-08-01T00:00:00Z".into(), end: String::new() });
        let v = json!({ "QueueSkills": { "competitive": { "SeasonalInfoBySeasonID": {
            "old": { "CompetitiveTier": 21, "NumberOfGames": 20, "WinsByTier": { "20": 3, "21": 5 } },
            "cur": { "CompetitiveTier": 19, "RankedRating": 42, "NumberOfWinsWithPlacements": 7, "NumberOfGames": 12, "WinsByTier": { "19": 7 } }
        }}}});
        let r = parse_mmr(&v, Some("cur"), &acts);
        assert_eq!((r.tier, r.rr, r.wins, r.games, r.act_peak_tier), (19, 42, 7, 12, 19));
        // Immortel 1 de l'ancien système = tier 24 aujourd'hui.
        assert_eq!(r.peak_tier, 24);
        assert_eq!(r.peak_act.as_deref(), Some("E4 · ACTE III"));
        assert_eq!(r.prev_tier, 24);

        let h = parse_history(&v, Some("cur"), &acts);
        assert_eq!(h.len(), 2);
        assert_eq!((h[0].tier, h[0].peak, h[0].current), (24, 24, false));
        // Ancien système : 20 → 20, 21 (Immortel 1) → 24
        assert_eq!(h[0].wins_by_tier, vec![[20, 3], [24, 5]]);
        assert_eq!((h[1].tier, h[1].games, h[1].wins, h[1].current), (19, 12, 7, true));
    }

    #[test]
    fn party_hints_merge() {
        let s = |x: &str| x.to_string();
        let p = |id: &str, team: &str, known: Option<&str>| (s(id), s(team), known.map(s));
        let players = vec![
            p("me", "Blue", Some("own")),
            p("mate", "Blue", Some("own")),
            p("a1", "Blue", None),
            p("a2", "Blue", None),
            p("a3", "Blue", None),
            p("e1", "Red", None),
            p("e2", "Red", None),
            p("e3", "Red", Some("friend")),
            p("e4", "Red", None),
            p("e5", "Red", None),
        ];
        let mates: HashMap<String, Vec<String>> = [
            // duo d'alliés
            ("a1", vec!["a2", "x"]),
            // a3 a joué avec ton coéquipier : ton groupe est connu exactement, rien n'est ajouté
            ("a3", vec!["mate"]),
            // trio adverse, relié dans les deux sens
            ("e1", vec!["e2", "e4"]),
            ("e2", vec!["e1"]),
            // e5 a joué avec ton ami (groupe connu) : rattaché à lui
            ("e5", vec!["e3"]),
            // même groupe au dernier match mais équipes différentes maintenant : ignoré
            ("e4", vec!["a3"]),
        ]
        .into_iter()
        .map(|(k, v)| (s(k), v.into_iter().map(s).collect()))
        .collect();
        let (root, guessed) = merge_hints(&players, Some("own"), &mates);
        let same = |a: &str, b: &str| root[a] == root[b];
        assert!(same("me", "mate") && root["me"] == "own");
        assert!(same("a1", "a2") && !same("a1", "a3") && !same("a3", "mate"));
        assert!(same("e1", "e2") && same("e1", "e4") && !same("e1", "e5"));
        assert!(same("e5", "e3") && root["e3"] == "friend");
        assert!(!same("e4", "a3"));
        let mut g: Vec<&str> = guessed.iter().map(|x| x.as_str()).collect();
        g.sort_unstable();
        assert_eq!(g, ["a1", "a2", "e1", "e2", "e4", "e5"]);
    }

    #[test]
    fn iso_dates() {
        assert_eq!(iso_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(iso_ms("2022-06-22T00:00:00Z"), Some(1_655_856_000_000));
        assert_eq!(iso_ms("2026-09-25T12:30:15.500Z"), Some(1_790_339_415_500));
        assert_eq!(iso_ms(""), None);
    }

    #[test]
    fn henrik_rank_mapping() {
        let mut acts = HashMap::new();
        acts.insert("cur".to_string(), Act { name: "V26 · ACTE V".into(), start: "2026-08-19T00:00:00Z".into(), end: String::new() });
        let d = json!({
            "current": { "tier": { "id": 21 }, "rr": 42, "leaderboard_placement": null },
            "peak": { "season": { "id": "old", "short": "e10a6" }, "tier": { "id": 25 } },
            "seasonal": [
                { "season": { "id": "old", "short": "e10a6" }, "wins": 30, "games": 55, "end_tier": { "id": 24 },
                  "act_wins": [ { "id": 23 }, { "id": 25 }, { "id": 25 }, { "id": 0 } ] },
                { "season": { "id": "cur", "short": "e11a5" }, "wins": 3, "games": 5, "end_tier": { "id": 21 },
                  "act_wins": [ { "id": 20 }, { "id": 22 }, { "id": 21 } ] }
            ]
        });
        let r = henrik_rank(&d, Some("cur"), &acts);
        assert_eq!((r.rank.tier, r.rank.rr, r.rank.wins, r.rank.games, r.rank.act_peak_tier), (21, 42, 3, 5, 22));
        assert_eq!((r.rank.peak_tier, r.rank.peak_act.as_deref()), (25, Some("E10 · ACTE VI")));
        assert_eq!((r.rank.prev_tier, r.rank.prev_act.as_deref()), (24, Some("E10 · ACTE VI")));
        assert_eq!(r.history.len(), 2);
        assert_eq!(r.history[0].wins_by_tier, vec![[23, 1], [25, 2]]);
        assert!(r.history[1].current);
        assert_eq!((r.total_games, r.total_wins), (60, 33));
    }

    #[test]
    fn public_seasons() {
        let v = json!({ "data": [
            { "uuid": "ep", "displayName": "V26", "type": null, "parentUuid": null },
            { "uuid": "a4", "displayName": "ACT IV", "type": "EAresSeasonType::Act", "startTime": "2026-06-24T00:00:00Z", "endTime": "2026-08-19T00:00:00Z", "parentUuid": "ep" },
            { "uuid": "a5", "displayName": "ACT V", "type": "EAresSeasonType::Act", "startTime": "2026-08-19T00:00:00Z", "endTime": "2026-10-14T00:00:00Z", "parentUuid": "ep" }
        ]});
        let (acts, current) = public_acts(&v, "2026-09-25T10:00:00Z");
        assert_eq!(current.as_deref(), Some("a5"));
        assert_eq!(acts["a4"].name, "V26 · ACTE IV");
        assert!(now_iso().starts_with("20") && now_iso().ends_with('Z'));
        assert_eq!(iso_ms(&now_iso()).map(|t| t / 1000), Some(now_ms() / 1000));
    }

    #[test]
    fn labels() {
        assert_eq!(short_act_label("e10a6").as_deref(), Some("E10 · ACTE VI"));
        assert_eq!(act_label(Some("EPISODE 9"), "ACT III"), "E9 · ACTE III");
        assert_eq!(act_label(Some("V26"), "ACT II"), "V26 · ACTE II");
    }
}

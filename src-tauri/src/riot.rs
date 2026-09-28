//! Accès au client Riot local (lockfile) puis aux serveurs Valorant (glz / pd / shared)
//! avec les jetons du client déjà connecté. Aucune clé d'API tierce.

use anyhow::{anyhow, bail, Context, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use reqwest::{Client, Method, StatusCode};
use serde_json::{json, Value};
use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

/// Les jetons Riot durent ~1 h ; on les relit régulièrement depuis le client local.
const TOKEN_TTL: Duration = Duration::from_secs(240);

#[derive(Debug)]
pub struct RateLimited;
impl fmt::Display for RateLimited {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("limite de requêtes Riot atteinte")
    }
}
impl std::error::Error for RateLimited {}

#[derive(PartialEq, Clone)]
struct Lockfile {
    port: u16,
    password: String,
}

pub struct Session {
    pub puuid: String,
    access_token: String,
    entitlement: String,
    region: String,
    shard: String,
    client_version: String,
    refreshed: Instant,
}

pub enum Conn {
    NoClient,
    NotLoggedIn,
    NoValorant,
    Ready,
}

/// Partagé entre la boucle de suivi et les commandes de l'interface (carrière) :
/// l'état interne est protégé, toutes les méthodes prennent `&self`.
pub struct Riot {
    local: Client,
    remote: Client,
    lock: Mutex<Option<Lockfile>>,
    session: RwLock<Option<Arc<Session>>>,
    stale: AtomicBool,
}

impl Riot {
    pub fn new() -> Self {
        let local = Client::builder()
            // Le client Riot local utilise un certificat auto-signé.
            .danger_accept_invalid_certs(true)
            .danger_accept_invalid_hostnames(true)
            .timeout(Duration::from_secs(3))
            .build()
            .expect("client HTTP local");
        let remote = Client::builder()
            .timeout(Duration::from_secs(8))
            .build()
            .expect("client HTTP distant");
        Self {
            local,
            remote,
            lock: Mutex::new(None),
            session: RwLock::new(None),
            stale: AtomicBool::new(false),
        }
    }

    fn session(&self) -> Option<Arc<Session>> {
        self.session.read().unwrap().clone()
    }

    fn set_session(&self, s: Option<Session>) {
        *self.session.write().unwrap() = s.map(Arc::new);
    }

    pub fn region(&self) -> String {
        self.session().map(|s| s.region.clone()).unwrap_or_default()
    }

    pub fn puuid(&self) -> String {
        self.session().map(|s| s.puuid.clone()).unwrap_or_default()
    }

    /// Jetons Riot disponibles (client Riot connecté, jeu lancé ou non).
    pub fn connected(&self) -> bool {
        self.session().is_some()
    }

    /// Vérifie que le client Riot tourne et rafraîchit les jetons. Sans le jeu lancé, la session
    /// reste utilisable (carrières, rangs, recherche) : seul le suivi de partie attend le jeu.
    pub async fn ensure(&self) -> Result<Conn> {
        let Some(lock) = read_lockfile() else {
            *self.lock.lock().unwrap() = None;
            self.set_session(None);
            return Ok(Conn::NoClient);
        };
        {
            let mut current = self.lock.lock().unwrap();
            if current.as_ref() != Some(&lock) {
                self.set_session(None);
            }
            *current = Some(lock);
        }

        let Ok(sessions) = self.local_get("/product-session/v1/external-sessions").await else {
            return Ok(Conn::NotLoggedIn);
        };
        let valorant = sessions
            .as_object()
            .and_then(|m| m.values().find(|s| s["productId"] == "valorant"))
            .cloned();
        let state = if valorant.is_some() { Conn::Ready } else { Conn::NoValorant };
        let valorant = valorant.unwrap_or(Value::Null);

        let previous = self.session();
        if let Some(s) = &previous {
            if !self.stale.load(Ordering::Relaxed) && s.refreshed.elapsed() < TOKEN_TTL {
                return Ok(state);
            }
        }

        let tok = match self.local_get("/entitlements/v1/token").await {
            Ok(t) => t,
            Err(_) => return Ok(Conn::NotLoggedIn),
        };
        let (Some(access), Some(ent), Some(puuid)) =
            (tok["accessToken"].as_str(), tok["token"].as_str(), tok["subject"].as_str())
        else {
            return Ok(Conn::NotLoggedIn);
        };
        // Autre compte connecté : région et version redétectées
        let previous = previous.filter(|s| s.puuid == puuid);

        // Région / version : on garde celles déjà trouvées, sinon on les détecte.
        let (region, shard, version) = match previous {
            Some(s) => (s.region.clone(), s.shard.clone(), s.client_version.clone()),
            None => {
                let log = read_game_log();
                let (region, shard) = match log.as_deref().and_then(region_from_log) {
                    Some(rs) => rs,
                    None => self.region_fallback(&valorant).await.context("région introuvable")?,
                };
                let version = match log.as_deref().and_then(version_from_log) {
                    Some(v) => v,
                    None => self.version_fallback().await.unwrap_or_default(),
                };
                (region, shard, version)
            }
        };

        self.set_session(Some(Session {
            puuid: puuid.to_string(),
            access_token: access.to_string(),
            entitlement: ent.to_string(),
            region,
            shard,
            client_version: version,
            refreshed: Instant::now(),
        }));
        self.stale.store(false, Ordering::Relaxed);
        Ok(state)
    }

    async fn region_fallback(&self, valorant: &Value) -> Option<(String, String)> {
        let from_args = valorant["launchConfiguration"]["arguments"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .find_map(|a| a.strip_prefix("-ares-deployment="))
            .map(str::to_lowercase);
        let region = match from_args {
            Some(r) => r,
            None => game_region(self.local_get("/riotclient/region-locale").await.ok()?["region"].as_str()?),
        };
        let shard = match region.as_str() {
            "latam" | "br" => "na".to_string(),
            r => r.to_string(),
        };
        Some((region, shard))
    }

    /// JSON public (valorant-api.com…), avec le client HTTP distant.
    pub async fn public_json(&self, url: &str) -> Result<Value> {
        let res = self.remote.get(url).send().await?;
        if !res.status().is_success() {
            bail!("{} sur {url}", res.status());
        }
        Ok(res.json().await?)
    }

    async fn version_fallback(&self) -> Option<String> {
        let v: Value = self
            .remote
            .get("https://valorant-api.com/v1/version")
            .send()
            .await
            .ok()?
            .json()
            .await
            .ok()?;
        v["data"]["riotClientVersion"].as_str().map(String::from)
    }

    async fn local_get(&self, path: &str) -> Result<Value> {
        let lock = self.lock.lock().unwrap().clone().ok_or_else(|| anyhow!("lockfile absent"))?;
        let res = self
            .local
            .get(format!("https://127.0.0.1:{}{}", lock.port, path))
            .basic_auth("riot", Some(&lock.password))
            .send()
            .await?;
        if !res.status().is_success() {
            bail!("client local {path} : {}", res.status());
        }
        Ok(res.json().await?)
    }

    /// Présences Valorant visibles (toi + tes amis), `private` décodé.
    pub async fn presences(&self) -> Result<Vec<(String, Value)>> {
        let v = self.local_get("/chat/v4/presences").await?;
        Ok(v["presences"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|p| p["product"] == "valorant")
            .filter_map(|p| {
                let puuid = p["puuid"].as_str()?.to_string();
                let raw = B64.decode(p["private"].as_str()?).ok()?;
                Some((puuid, serde_json::from_slice(&raw).ok()?))
            })
            .collect())
    }

    fn url(&self, kind: &str, path: &str) -> Result<(Arc<Session>, String)> {
        let s = self.session().ok_or_else(|| anyhow!("Valorant n'est pas connecté"))?;
        let base = match kind {
            "glz" => format!("https://glz-{}-1.{}.a.pvp.net", s.region, s.shard),
            "pd" => format!("https://pd.{}.a.pvp.net", s.shard),
            _ => format!("https://shared.{}.a.pvp.net", s.shard),
        };
        Ok((s, base + path))
    }

    pub async fn glz(&self, path: &str) -> Result<Option<Value>> {
        let (s, url) = self.url("glz", path)?;
        self.remote(&s, Method::GET, url, None).await
    }

    pub async fn pd(&self, path: &str) -> Result<Option<Value>> {
        let (s, url) = self.url("pd", path)?;
        self.remote(&s, Method::GET, url, None).await
    }

    /// Comme `pd`, mais patiente et réessaie si Riot limite les requêtes ou si le jeton vient
    /// d'expirer (la boucle de suivi le renouvelle), jusqu'à `max` au total.
    pub async fn pd_patient(&self, path: &str, max: Duration) -> Result<Option<Value>> {
        let started = Instant::now();
        let mut delay = 2;
        loop {
            match self.pd(path).await {
                Err(e) if (e.is::<RateLimited>() || self.stale.load(Ordering::Relaxed)) && started.elapsed() < max => {
                    // Jamais au-delà de `max` : dernier essai pile à l'échéance
                    tokio::time::sleep(Duration::from_secs(delay).min(max.saturating_sub(started.elapsed()))).await;
                    delay = (delay * 2).min(30);
                }
                r => return r,
            }
        }
    }

    pub async fn pd_put(&self, path: &str, body: Value) -> Result<Option<Value>> {
        let (s, url) = self.url("pd", path)?;
        self.remote(&s, Method::PUT, url, Some(body)).await
    }

    pub async fn shared(&self, path: &str) -> Result<Option<Value>> {
        let (s, url) = self.url("shared", path)?;
        self.remote(&s, Method::GET, url, None).await
    }

    /// Requête authentifiée. `Ok(None)` = 404 (pas en partie, etc.).
    async fn remote(&self, s: &Session, method: Method, url: String, body: Option<Value>) -> Result<Option<Value>> {
        let mut req = self
            .remote
            .request(method, &url)
            .bearer_auth(&s.access_token)
            .header("X-Riot-Entitlements-JWT", &s.entitlement)
            .header("X-Riot-ClientVersion", &s.client_version)
            .header("X-Riot-ClientPlatform", client_platform());
        if let Some(b) = body {
            req = req.json(&b);
        }
        let res = req.send().await?;
        match res.status() {
            st if st.is_success() => Ok(Some(res.json().await?)),
            StatusCode::NOT_FOUND => Ok(None),
            StatusCode::TOO_MANY_REQUESTS => Err(RateLimited.into()),
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                self.stale.store(true, Ordering::Relaxed);
                bail!("jeton refusé ({})", res.status())
            }
            // Riot répond parfois 400 quand le joueur n'est pas (encore) en partie.
            StatusCode::BAD_REQUEST if url.contains("/players/") => Ok(None),
            st => bail!("{st} sur {url}"),
        }
    }
}

/// Région du client Riot (« EUW », « NA », « KR »…) → région des serveurs Valorant.
fn game_region(r: &str) -> String {
    let r = r.trim().to_lowercase();
    match r.as_str() {
        "na" | "latam" | "br" | "eu" | "ap" | "kr" | "pbe" => r,
        "euw" | "eune" | "tr" | "ru" | "me" | "mena" => "eu".into(),
        "la1" | "la2" | "las" | "lan" => "latam".into(),
        "br1" => "br".into(),
        "jp" | "jp1" | "oc" | "oc1" | "oce" | "sea" | "ph" | "sg" | "th" | "tw" | "vn" => "ap".into(),
        "kr1" => "kr".into(),
        "na1" => "na".into(),
        _ => r,
    }
}

fn client_platform() -> String {
    let platform = json!({
        "platformType": "PC",
        "platformOS": "Windows",
        "platformOSVersion": "10.0.19042.1.256.64bit",
        "platformChipset": "Unknown"
    });
    B64.encode(platform.to_string())
}

fn local_app_data() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
}

fn read_lockfile() -> Option<Lockfile> {
    let path = local_app_data()?.join(r"Riot Games\Riot Client\Config\lockfile");
    let raw = std::fs::read_to_string(path).ok()?;
    // nom:pid:port:motdepasse:protocole
    let parts: Vec<&str> = raw.trim().split(':').collect();
    if parts.len() < 5 {
        return None;
    }
    Some(Lockfile { port: parts[2].parse().ok()?, password: parts[3].to_string() })
}

fn read_game_log() -> Option<String> {
    let path = local_app_data()?.join(r"VALORANT\Saved\Logs\ShooterGame.log");
    let bytes = std::fs::read(path).ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// `https://glz-eu-1.eu.a.pvp.net` → ("eu", "eu")
fn region_from_log(log: &str) -> Option<(String, String)> {
    const START: &str = "https://glz-";
    let rest = &log[log.rfind(START)? + START.len()..];
    let host = &rest[..rest.find(".a.pvp.net")?];
    let (left, shard) = host.split_once('.')?;
    let region = left.rsplit_once('-').map_or(left, |(r, _)| r);
    Some((region.to_string(), shard.to_string()))
}

/// Construit `release-XX.YY-shipping-B-CL` depuis le log du jeu.
fn version_from_log(log: &str) -> Option<String> {
    let after = |key: &str| -> Option<&str> {
        let i = log.find(key)? + key.len();
        log[i..].lines().next().map(str::trim)
    };
    if let Some(v) = after("CI server version: ") {
        if v.starts_with("release-") && v.contains("-shipping-") {
            return Some(v.to_string());
        }
    }
    let branch = after("Branch: ")?;
    let changelist = after("Changelist: ")?;
    let build = after("Build version: ")?;
    Some(format!("{branch}-shipping-{build}-{changelist}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_region_and_version() {
        let log = "LogShooter: Branch: release-13.06\nChangelist: 5435758\nBuild version: 13\n\
                   GET https://glz-eu-1.eu.a.pvp.net/session/v1 ...\n";
        assert_eq!(region_from_log(log), Some(("eu".into(), "eu".into())));
        assert_eq!(version_from_log(log).as_deref(), Some("release-13.06-shipping-13-5435758"));
        let latam = "x https://glz-latam-1.na.a.pvp.net/y";
        assert_eq!(region_from_log(latam), Some(("latam".into(), "na".into())));
        assert_eq!(game_region("EUW"), "eu");
        assert_eq!(game_region("JP"), "ap");
        assert_eq!(game_region("NA"), "na");
    }
}

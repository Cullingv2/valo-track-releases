//! API HenrikDev (données Valorant publiques, sans le jeu) : via le serveur Valo Overlay quand il
//! est configuré (il garde la clé, met les réponses en cache et partage le quota entre tous les
//! joueurs), sinon directement avec la clé personnelle de config.json.

use crate::config::Config;
use reqwest::Client;
use serde_json::Value;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const DIRECT: &str = "https://api.henrikdev.xyz";

/// Réponse d'une source : `Err` = source injoignable (on essaie la suivante).
type Fetched = Result<Option<Value>, ()>;

pub struct Henrik {
    key: String,
    server: String,
    token: String,
    client: Client,
    /// Quota restant et fin de la minute en cours : [serveur, direct]
    quota: [Mutex<Option<(i64, Instant)>>; 2],
}

impl Henrik {
    pub fn new(cfg: &Config) -> Self {
        Self {
            key: cfg.henrik_api_key.trim().to_string(),
            server: cfg.api_server.trim().trim_end_matches('/').to_string(),
            token: cfg.api_token.trim().to_string(),
            client: Client::builder().timeout(Duration::from_secs(20)).build().unwrap_or_default(),
            quota: [Mutex::new(None), Mutex::new(None)],
        }
    }

    /// Aucune source configurée (tests, ou ni clé ni serveur).
    #[cfg(test)]
    pub fn disabled() -> Self {
        Self::new(&Config { henrik_api_key: String::new(), api_server: String::new(), ..Config::default() })
    }

    pub fn available(&self) -> bool {
        !self.key.is_empty() || !self.server.is_empty()
    }

    /// `path` : « valorant/v3/by-puuid/mmr/eu/pc/… » (requête comprise, segments déjà encodés).
    /// `None` = introuvable ou service indisponible.
    pub async fn get(&self, path: &str) -> Option<Value> {
        self.get_with(path, true).await
    }

    /// Comme `get`, mais sans jamais patienter pour le quota : `None` tout de suite s'il est épuisé
    /// (l'appelant a une autre source, Riot, sur laquelle se rabattre).
    pub async fn try_get(&self, path: &str) -> Option<Value> {
        self.get_with(path, false).await
    }

    async fn get_with(&self, path: &str, wait: bool) -> Option<Value> {
        let path = path.trim_start_matches('/');
        if !self.server.is_empty() {
            if let Ok(v) = self.fetch(0, &format!("{}/v1/henrik/{path}", self.server), wait).await {
                return v;
            }
        }
        if !self.key.is_empty() {
            return self.fetch(1, &format!("{DIRECT}/{path}"), wait).await.ok().flatten();
        }
        None
    }

    /// Requête avec respect du quota (lu dans les en-têtes x-ratelimit-* ou retry-after) :
    /// pleine vitesse tant qu'il en reste, pause jusqu'à la minute suivante sinon.
    async fn fetch(&self, source: usize, url: &str, patient: bool) -> Fetched {
        let quota = &self.quota[source];
        for _ in 0..if patient { 8 } else { 1 } {
            let wait = match *quota.lock().unwrap() {
                Some((remaining, reset_at)) if remaining <= 1 => reset_at.saturating_duration_since(Instant::now()),
                _ => Duration::ZERO,
            };
            if !wait.is_zero() {
                if !patient {
                    return Ok(None);
                }
                tokio::time::sleep(wait + Duration::from_millis(300)).await;
                *quota.lock().unwrap() = None;
            }
            let mut req = self.client.get(url);
            if !patient {
                // Une autre source attend derrière : pas plus de quelques secondes ici.
                req = req.timeout(Duration::from_secs(6));
            }
            req = if source == 0 { req.header("X-App-Token", &self.token) } else { req.header("Authorization", &self.key) };
            let res = req.send().await.map_err(|_| ())?;
            let header = |name: &str| res.headers().get(name).and_then(|v| v.to_str().ok()).and_then(|v| v.trim().parse::<i64>().ok());
            let reset = header("x-ratelimit-reset").or(header("retry-after")).unwrap_or(60).clamp(1, 120) as u64;
            let reset_at = Instant::now() + Duration::from_secs(reset);
            if let Some(remaining) = header("x-ratelimit-remaining") {
                *quota.lock().unwrap() = Some((remaining, reset_at));
            }
            match res.status().as_u16() {
                200..=299 => return Ok(res.json().await.ok()),
                400 | 404 if source == 0 => {
                    // Serveur sans le relais (pas encore installé) : on passe à la source suivante.
                    let body = res.text().await.unwrap_or_default();
                    return if body.contains("\"error\":\"route\"") || body.contains("\"error\":\"not_found\"") { Err(()) } else { Ok(None) };
                }
                400 | 404 => return Ok(None),
                429 => *quota.lock().unwrap() = Some((0, reset_at)),
                _ => return Err(()),
            }
        }
        Err(())
    }
}

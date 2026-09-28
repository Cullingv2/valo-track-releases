//! Joueurs connus (pseudos vus en partie ou dans un match détaillé) : sert à retrouver la région
//! d'un joueur, même sans le jeu. Fichier `known-players.json` du dossier de cache.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// Au-delà, les joueurs vus il y a le plus longtemps sont oubliés.
const MAX_KNOWN: usize = 20_000;

#[derive(Serialize, Deserialize, Clone, Default, Debug)]
#[serde(default, rename_all = "camelCase")]
pub struct Known {
    pub puuid: String,
    pub name: String,
    pub tag: String,
    pub card_id: String,
    pub tier: u32,
    pub level: u32,
    pub region: String,
    /// Dernière fois vu (ms)
    pub seen: u64,
}

#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
struct Saved {
    region: String,
    players: Vec<Known>,
}

pub struct Directory {
    players: Mutex<HashMap<String, Known>>,
    /// Dernière région connue de ton compte (recherches sans le client Riot)
    region: Mutex<String>,
    path: Option<PathBuf>,
    dirty: AtomicBool,
}

impl Directory {
    pub fn new(dir: Option<PathBuf>) -> Self {
        let path = dir.map(|d| d.join("known-players.json"));
        let saved: Saved = path
            .as_ref()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Self {
            players: Mutex::new(saved.players.into_iter().map(|k| (k.puuid.clone(), k)).collect()),
            region: Mutex::new(saved.region),
            path,
            dirty: AtomicBool::new(false),
        }
    }

    /// Ajoute ou complète un joueur (les champs vides ne remplacent rien).
    pub fn remember(&self, k: Known) {
        if k.puuid.is_empty() || k.name.is_empty() {
            return;
        }
        let now = now_ms();
        let mut map = self.players.lock().unwrap();
        let e = map.entry(k.puuid.clone()).or_insert_with(|| Known { puuid: k.puuid.clone(), ..Default::default() });
        let before = (e.name.clone(), e.tag.clone(), e.card_id.clone(), e.tier, e.level, e.region.clone());
        e.name = k.name;
        if !k.tag.is_empty() {
            e.tag = k.tag;
        }
        if !k.card_id.is_empty() {
            e.card_id = k.card_id;
        }
        if k.tier > 0 {
            e.tier = k.tier;
        }
        if k.level > 0 {
            e.level = k.level;
        }
        if !k.region.is_empty() {
            e.region = k.region;
        }
        let changed = before != (e.name.clone(), e.tag.clone(), e.card_id.clone(), e.tier, e.level, e.region.clone());
        // La date n'est réécrite qu'une fois par heure (pas d'écriture disque à chaque partie).
        if changed || now.saturating_sub(e.seen) > 3_600_000 {
            e.seen = now;
            self.dirty.store(true, Ordering::Relaxed);
        }
    }

    pub fn get(&self, puuid: &str) -> Option<Known> {
        self.players.lock().unwrap().get(puuid).cloned()
    }

    pub fn set_region(&self, region: &str) {
        let mut r = self.region.lock().unwrap();
        if !region.is_empty() && *r != region {
            *r = region.to_string();
            self.dirty.store(true, Ordering::Relaxed);
        }
    }

    /// Région d'un joueur : la sienne si connue, sinon `fallback` (ta session), sinon la dernière
    /// région de ton compte, sinon l'Europe.
    pub fn region_for(&self, puuid: &str, fallback: &str) -> String {
        if let Some(r) = self.get(puuid).map(|k| k.region).filter(|r| !r.is_empty()) {
            return r;
        }
        if !fallback.is_empty() {
            return fallback.to_string();
        }
        let saved = self.region.lock().unwrap().clone();
        if saved.is_empty() { "eu".into() } else { saved }
    }

    /// Écrit le fichier si quelque chose a changé.
    pub fn save(&self) {
        if !self.dirty.swap(false, Ordering::Relaxed) {
            return;
        }
        let Some(path) = &self.path else { return };
        let mut players: Vec<Known> = self.players.lock().unwrap().values().cloned().collect();
        if players.len() > MAX_KNOWN {
            players.sort_by(|a, b| b.seen.cmp(&a.seen));
            players.truncate(MAX_KNOWN);
            let keep: HashMap<String, Known> = players.iter().map(|k| (k.puuid.clone(), k.clone())).collect();
            *self.players.lock().unwrap() = keep;
        }
        let saved = Saved { region: self.region.lock().unwrap().clone(), players };
        if let Ok(json) = serde_json::to_vec(&saved) {
            let tmp = path.with_extension("tmp");
            if std::fs::write(&tmp, json).is_ok() {
                let _ = std::fs::rename(tmp, path);
            }
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(puuid: &str, name: &str, tag: &str) -> Known {
        Known { puuid: puuid.into(), name: name.into(), tag: tag.into(), ..Default::default() }
    }

    #[test]
    fn regions() {
        let d = Directory::new(None);
        assert_eq!(d.region_for("x", ""), "eu");
        d.set_region("na");
        assert_eq!(d.region_for("x", ""), "na");
        assert_eq!(d.region_for("x", "ap"), "ap");
        d.remember(Known { region: "kr".into(), ..k("x", "Faker", "KR1") });
        assert_eq!(d.region_for("x", "ap"), "kr");
    }
}

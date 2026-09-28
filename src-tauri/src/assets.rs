//! Métadonnées des assets officiels (agents, rangs, cartes) depuis valorant-api.com,
//! mises en cache sur disque. Les images elles-mêmes sont servies par media.valorant-api.com.

use anyhow::Result;
use serde_json::{json, Map, Value};
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Manager, State};
use tokio::sync::Mutex;

const MAX_AGE: Duration = Duration::from_secs(3 * 24 * 3600);
const CACHE_FILE: &str = "assets-v2.json";

pub struct AssetStore {
    language: String,
    data: Mutex<Option<Value>>,
}

impl AssetStore {
    pub fn new(language: String) -> Self {
        Self { language, data: Mutex::new(None) }
    }
}

#[tauri::command]
pub async fn get_assets(app: AppHandle, store: State<'_, AssetStore>) -> Result<Value, String> {
    let mut data = store.data.lock().await;
    if let Some(v) = data.as_ref() {
        return Ok(v.clone());
    }
    let path = app.path().app_cache_dir().ok().map(|d| d.join(CACHE_FILE));
    let cached = path.as_ref().and_then(read_cache);

    let fresh = cached.as_ref().is_some_and(|c| {
        c["language"] == store.language.as_str() && now_secs().saturating_sub(c["fetchedAt"].as_u64().unwrap_or(0)) < MAX_AGE.as_secs()
    });
    let value = if fresh {
        cached.unwrap()
    } else {
        match fetch_all(&store.language).await {
            Ok(v) => {
                if let Some(p) = &path {
                    let _ = std::fs::create_dir_all(p.parent().unwrap());
                    let _ = std::fs::write(p, v.to_string());
                }
                v
            }
            // Hors ligne : on garde l'ancien cache s'il existe.
            Err(e) => cached.ok_or_else(|| format!("assets indisponibles : {e:#}"))?,
        }
    };
    *data = Some(value.clone());
    Ok(value)
}

fn read_cache(path: &PathBuf) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

async fn fetch_all(lang: &str) -> Result<Value> {
    let client = reqwest::Client::builder().timeout(Duration::from_secs(20)).build()?;
    let get = |url: String| {
        let client = client.clone();
        async move {
            let v: Value = client.get(&url).send().await?.error_for_status()?.json().await?;
            Ok::<Value, anyhow::Error>(v["data"].clone())
        }
    };
    let base = "https://valorant-api.com/v1";
    let (agents, tiers, maps, cards, borders) = tokio::try_join!(
        get(format!("{base}/agents?isPlayableCharacter=true&language={lang}")),
        get(format!("{base}/competitivetiers?language={lang}")),
        get(format!("{base}/maps?language={lang}")),
        get(format!("{base}/playercards")),
        get(format!("{base}/levelborders")),
    )?;

    let list = |v: &Value| v.as_array().cloned().unwrap_or_default();
    let agents: Vec<Value> = list(&agents)
        .iter()
        .map(|a| {
            let mut o = pick(a, &["uuid", "displayName", "displayIcon", "fullPortrait", "backgroundGradientColors"]);
            o["role"] = json!({ "displayName": a["role"]["displayName"], "displayIcon": a["role"]["displayIcon"] });
            o
        })
        .collect();
    // Dernière table de rangs = épisode en cours.
    let tiers: Vec<Value> = list(&tiers)
        .last()
        .map(|t| list(&t["tiers"]))
        .unwrap_or_default()
        .iter()
        .map(|t| pick(t, &["tier", "tierName", "color", "backgroundColor", "largeIcon", "smallIcon"]))
        .collect();
    let maps: Vec<Value> =
        list(&maps).iter().map(|m| pick(m, &["uuid", "displayName", "mapUrl", "splash", "listViewIcon"])).collect();
    // Quelques cartes de joueur pour le mode démo.
    let cards: Vec<Value> = list(&cards)
        .iter()
        .filter(|c| c["wideArt"].is_string() && c["largeArt"].is_string())
        .step_by(37)
        .take(24)
        .map(|c| c["uuid"].clone())
        .collect();

    Ok(json!({
        "language": lang,
        "fetchedAt": now_secs(),
        "agents": agents,
        "tiers": tiers,
        "maps": maps,
        "cards": cards,
        // Cadres du niveau de compte, comme en jeu (un par palier de 20 niveaux)
        "levelBorders": list(&borders)
            .iter()
            .map(|b| pick(b, &["startingLevel", "levelNumberAppearance"]))
            .collect::<Vec<Value>>(),
    }))
}

fn pick(v: &Value, keys: &[&str]) -> Value {
    let mut m = Map::new();
    for k in keys {
        if let Some(x) = v.get(*k) {
            m.insert(k.to_string(), x.clone());
        }
    }
    Value::Object(m)
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

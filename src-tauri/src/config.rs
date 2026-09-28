//! Configuration utilisateur : %APPDATA%\fr.valooverlay.app\config.json

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, State};

#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct Config {
    /// Raccourci global, ex. "Alt+Z", "Ctrl+Shift+V", "F10".
    pub hotkey: String,
    /// "toggle" = un appui affiche / masque ; "hold" = visible tant que la touche est enfoncée.
    pub mode: String,
    /// Langue des noms d'agents, cartes et rangs.
    pub language: String,
    /// Intro animée : "launch" (au démarrage de l'application), "daily" (première ouverture
    /// du jour), "always" (à chaque ouverture) ou "never".
    pub intro: String,
    /// false = aucune animation (PC très modestes).
    pub animations: bool,
    /// Clé de l'API HenrikDev (optionnelle) : retrouve les parties d'un acte que Riot ne liste plus.
    #[serde(rename = "henrikApiKey", skip_serializing)]
    pub henrik_api_key: String,
    /// Serveur Valo Overlay (optionnel, ex. "https://valo.mondomaine.fr") : garde la clé HenrikDev
    /// côté serveur et partage un cache entre tous les joueurs.
    #[serde(rename = "apiServer")]
    pub api_server: String,
    /// Jeton de l'application attendu par ce serveur.
    #[serde(rename = "apiToken", skip_serializing)]
    pub api_token: String,
}

/// Serveur relais par défaut et son jeton, fournis à la compilation (jamais dans le code source) :
/// variables `VALO_API_SERVER` / `VALO_API_TOKEN`, par exemple dans `src-tauri/.cargo/config.toml`
/// (non versionné, voir `config.toml.example`). Sans elles, l'application fonctionne sans relais
/// (données Riot, ou clé HenrikDev personnelle dans config.json).
const DEFAULT_SERVER: &str = match option_env!("VALO_API_SERVER") {
    Some(s) => s,
    None => "",
};
const DEFAULT_TOKEN: &str = match option_env!("VALO_API_TOKEN") {
    Some(s) => s,
    None => "",
};

impl Default for Config {
    fn default() -> Self {
        Self {
            hotkey: "Alt+Z".into(),
            mode: "toggle".into(),
            language: "fr-FR".into(),
            intro: "launch".into(),
            animations: true,
            henrik_api_key: String::new(),
            api_server: DEFAULT_SERVER.into(),
            api_token: DEFAULT_TOKEN.into(),
        }
    }
}

pub fn load(app: &AppHandle) -> Config {
    let Ok(dir) = app.path().app_config_dir() else { return Config::default() };
    let path = dir.join("config.json");
    match std::fs::read_to_string(&path) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_else(|e| {
            eprintln!("config.json invalide ({e}), valeurs par défaut utilisées");
            Config::default()
        }),
        Err(_) => {
            let cfg = Config::default();
            let _ = std::fs::create_dir_all(&dir);
            let _ = std::fs::write(&path, serde_json::to_string_pretty(&cfg).unwrap_or_default());
            cfg
        }
    }
}

/// Réglages utiles à l'interface (les clés et jetons restent côté Rust).
#[tauri::command]
pub fn get_config(cfg: State<'_, Config>) -> Config {
    cfg.inner().clone()
}

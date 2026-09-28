#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod assets;
mod career;
mod config;
mod directory;
mod henrik;
mod overlay;
mod riot;
mod tracker;

use std::sync::Arc;
use tauri::Manager;

fn main() {
    tauri::Builder::default()
        // Relancer l'application ouvre simplement l'overlay de l'instance déjà lancée.
        .plugin(tauri_plugin_single_instance::init(|app, _, _| overlay::set_visible(app, true)))
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .setup(|app| {
            let cfg = config::load(app.handle());
            let shared = Arc::new(tracker::Shared::default());
            let riot = Arc::new(riot::Riot::new());
            app.manage(shared.clone());
            app.manage(riot.clone());
            let cache_dir = app.path().app_cache_dir().ok();
            let henrik = Arc::new(henrik::Henrik::new(&cfg));
            let people = Arc::new(directory::Directory::new(cache_dir.clone()));
            app.manage(henrik.clone());
            app.manage(people.clone());
            // Joueurs connus : écrits sur disque régulièrement (suggestions de recherche)
            tauri::async_runtime::spawn(async move {
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                    people.save();
                }
            });
            let people = app.state::<Arc<directory::Directory>>().inner().clone();
            let cache = Arc::new(career::CareerCache::new(cache_dir, henrik, people));
            app.manage(cache.clone());
            career::spawn_archiver(riot.clone(), cache, shared.clone());
            app.manage(assets::AssetStore::new(cfg.language.clone()));
            overlay::setup(app, &cfg)?;
            app.manage(cfg);
            tracker::spawn(app.handle().clone(), shared, riot);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            tracker::get_snapshot,
            assets::get_assets,
            config::get_config,
            career::get_career,
            career::get_match,
            career::prefetch_players,
            career::get_last_result,
            tracker::get_rank,
            overlay::show_overlay,
            overlay::hide_overlay,
            overlay::toggle_compact,
            overlay::is_compact
        ])
        .run(tauri::generate_context!())
        .expect("impossible de lancer Valo Overlay");
}

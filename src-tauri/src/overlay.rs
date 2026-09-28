//! Fenêtre de l'overlay (sans bordure, coins arrondis), raccourci global modifiable et icône
//! dans la zone de notification. C'est une vraie fenêtre : visible dans la barre des tâches et
//! dans Alt+Tab. Ouverte par le raccourci, elle passe au premier plan (par-dessus le jeu) ; si on
//! va sur une autre fenêtre, elle redevient normale. Fermer = réduire (l'application continue),
//! et le focus est rendu à la fenêtre précédente (le jeu).

use crate::config::Config;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{App, AppHandle, Emitter, Manager, PhysicalSize, WebviewWindow, WindowEvent, Wry};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutState};

const LABEL: &str = "overlay";

/// Fenêtre active avant l'ouverture de l'overlay, à qui rendre le focus.
static PREVIOUS: AtomicIsize = AtomicIsize::new(0);
/// Taille choisie par l'utilisateur, en fraction de l'écran (largeur, hauteur) : elle suit
/// les changements de résolution (passage en 4:3, écran étiré…). `None` = taille par défaut.
static SIZE: Mutex<Option<(f64, f64)>> = Mutex::new(None);
/// Écran (taille en pixels) lors du dernier placement : s'il change, la fenêtre est recentrée.
static LAST_MONITOR: Mutex<Option<(u32, u32)>> = Mutex::new(None);
/// Dernier redimensionnement fait par l'overlay lui-même (à ne pas prendre pour un choix de l'utilisateur)
static PROGRAMMATIC: Mutex<Option<Instant>> = Mutex::new(None);
/// Enregistrement différé de la taille (une seule écriture à la fin d'un redimensionnement)
static SAVE_GEN: AtomicU64 = AtomicU64::new(0);
/// Fichier où la taille choisie est retenue
static SIZE_FILE: Mutex<Option<PathBuf>> = Mutex::new(None);
/// Format compact : part de la taille par défaut
const COMPACT: f64 = 0.64;
/// Fenêtre affichée (ni réduite ni cachée), selon l'overlay
static SHOWN: AtomicBool = AtomicBool::new(false);
/// Raccourci actuel et mode « maintenir » (visible tant que la touche est enfoncée)
static HOTKEY: Mutex<String> = Mutex::new(String::new());
static HOLD: AtomicBool = AtomicBool::new(false);
/// Entrée « Afficher / masquer » du menu de l'icône (son libellé montre le raccourci)
static TOGGLE_ITEM: Mutex<Option<MenuItem<Wry>>> = Mutex::new(None);

pub fn setup(app: &mut App, cfg: &Config) -> Result<(), Box<dyn std::error::Error>> {
    let win = app.get_webview_window(LABEL).ok_or("fenêtre overlay absente")?;
    native::round_corners(&win);

    // Taille retenue d'une session à l'autre
    let file = app.path().app_config_dir().ok().map(|d| d.join("window.json"));
    if let Some(saved) = file.as_ref().and_then(|f| std::fs::read(f).ok()).and_then(|b| serde_json::from_slice::<(f64, f64)>(&b).ok()) {
        if saved.0 > 0.2 && saved.0 <= 1.0 && saved.1 > 0.2 && saved.1 <= 1.0 {
            *SIZE.lock().unwrap() = Some(saved);
        }
    }
    *SIZE_FILE.lock().unwrap() = file;

    // Fermer (Alt+F4, bouton ×) = masquer ; l'application reste dans la zone de notification.
    let handle = app.handle().clone();
    let w2 = win.clone();
    win.on_window_event(move |e| match e {
        WindowEvent::CloseRequested { api, .. } => {
            api.prevent_close();
            set_visible(&handle, false);
        }
        // Rouverte par la barre des tâches ou Alt+Tab : l'interface se remet à jour
        WindowEvent::Focused(true) => {
            if !SHOWN.swap(true, Ordering::SeqCst) {
                let _ = handle.emit("overlay-visibility", true);
            }
        }
        // Une autre fenêtre passe devant : l'overlay redevient une fenêtre normale ; réduit par
        // la barre des tâches : l'interface cesse de se dessiner
        WindowEvent::Focused(false) => {
            let _ = w2.set_always_on_top(false);
            if w2.is_minimized().unwrap_or(false) && SHOWN.swap(false, Ordering::SeqCst) {
                let _ = handle.emit("overlay-visibility", false);
            }
        }
        // Redimensionnée à la main : la nouvelle taille est retenue (en part de l'écran)
        WindowEvent::Resized(size) => {
            let ours = PROGRAMMATIC.lock().unwrap().is_some_and(|t| t.elapsed() < Duration::from_millis(900));
            if ours || !w2.is_visible().unwrap_or(false) || size.width == 0 {
                return;
            }
            if let Some((mw, mh)) = monitor_size(&w2) {
                remember(size.width as f64 / mw as f64, size.height as f64 / mh as f64);
            }
        }
        _ => {}
    });

    HOLD.store(cfg.mode.eq_ignore_ascii_case("hold"), Ordering::Relaxed);
    let hotkey = if cfg.hotkey.parse::<Shortcut>().is_ok() { cfg.hotkey.clone() } else { "Alt+Z".to_string() };
    *HOTKEY.lock().unwrap() = hotkey.clone();
    let hotkey_label = match register(app.handle(), &hotkey) {
        Ok(()) => hotkey.clone(),
        Err(e) => {
            eprintln!("raccourci {hotkey} indisponible : {e}");
            "raccourci indisponible".into()
        }
    };

    let toggle_item = MenuItem::with_id(app, "toggle", format!("Afficher / masquer  ({hotkey_label})"), true, None::<&str>)?;
    *TOGGLE_ITEM.lock().unwrap() = Some(toggle_item.clone());
    let demo_item = CheckMenuItem::with_id(app, "demo", "Mode démo", true, false, None::<&str>)?;
    let quit_item = MenuItem::with_id(app, "quit", "Quitter", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&toggle_item, &demo_item, &PredefinedMenuItem::separator(app)?, &quit_item])?;

    let demo = demo_item.clone();
    TrayIconBuilder::with_id("main")
        .icon(app.default_window_icon().cloned().ok_or("icône absente")?)
        .tooltip(format!("Valo Overlay — {hotkey_label}"))
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(move |app, e| match e.id.as_ref() {
            "toggle" => toggle(app),
            "demo" => {
                let on = demo.is_checked().unwrap_or(false);
                let _ = app.emit("demo-mode", on);
                if on {
                    set_visible(app, true);
                }
            }
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, e| {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = e {
                toggle(tray.app_handle());
            }
        })
        .build(app)?;
    Ok(())
}

/// Appelée par l'interface au démarrage de l'application, pour jouer l'intro.
#[tauri::command]
pub fn show_overlay(app: AppHandle) {
    set_visible(&app, true);
}

#[tauri::command]
pub fn hide_overlay(app: AppHandle) {
    set_visible(&app, false);
}

/// Raccourci : ouverte et au premier plan → réduite ; sinon (réduite, cachée ou derrière une
/// autre fenêtre) → ramenée devant.
pub fn toggle(app: &AppHandle) {
    if let Some(w) = app.get_webview_window(LABEL) {
        let front = is_shown(&w) && w.is_focused().unwrap_or(false);
        set_visible(app, !front);
    }
}

fn is_shown(w: &WebviewWindow) -> bool {
    w.is_visible().unwrap_or(false) && !w.is_minimized().unwrap_or(false)
}

pub fn set_visible(app: &AppHandle, visible: bool) {
    let Some(w) = app.get_webview_window(LABEL) else { return };
    let shown = is_shown(&w);
    if visible {
        let was_shown = SHOWN.swap(true, Ordering::SeqCst) && shown;
        let previous = native::foreground();
        if previous != native::handle(&w) {
            PREVIOUS.store(previous, Ordering::Relaxed);
        }
        if w.is_minimized().unwrap_or(false) {
            let _ = w.unminimize();
        }
        place(&w, false);
        let _ = w.show();
        let _ = w.set_always_on_top(true);
        let _ = w.set_focus();
        if !was_shown {
            let _ = app.emit("overlay-visibility", true);
        }
    } else {
        if !shown {
            return;
        }
        SHOWN.store(false, Ordering::SeqCst);
        // Réduite (et non cachée) : elle reste dans la barre des tâches et dans Alt+Tab
        let _ = w.set_always_on_top(false);
        let _ = w.minimize();
        native::focus(PREVIOUS.swap(0, Ordering::Relaxed));
        let _ = app.emit("overlay-visibility", false);
    }
}

/// Enregistre le raccourci global (le mode « maintenir » est lu à chaque appui).
fn register(app: &AppHandle, hotkey: &str) -> Result<(), String> {
    let shortcut: Shortcut = hotkey.parse().map_err(|_| format!("« {hotkey} » n'est pas un raccourci valide"))?;
    app.global_shortcut()
        .on_shortcut(shortcut, |app, _, event| match (HOLD.load(Ordering::Relaxed), event.state()) {
            (false, ShortcutState::Pressed) => toggle(app),
            (true, ShortcutState::Pressed) => set_visible(app, true),
            (true, ShortcutState::Released) => set_visible(app, false),
            _ => {}
        })
        .map_err(|_| "ce raccourci est déjà utilisé par une autre application".to_string())
}

/// Écrit un réglage dans config.json sans toucher aux autres (clé HenrikDev, serveur…).
fn save_setting(app: &AppHandle, key: &str, value: serde_json::Value) {
    let Ok(dir) = app.path().app_config_dir() else { return };
    let path = dir.join("config.json");
    let mut cfg: serde_json::Value = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_else(|| serde_json::json!({}));
    if let Some(obj) = cfg.as_object_mut() {
        obj.insert(key.to_string(), value);
    }
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(&path, serde_json::to_string_pretty(&cfg).unwrap_or_default());
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HotkeySettings {
    hotkey: String,
    hold: bool,
}

#[tauri::command]
pub fn get_hotkey() -> HotkeySettings {
    HotkeySettings { hotkey: HOTKEY.lock().unwrap().clone(), hold: HOLD.load(Ordering::Relaxed) }
}

/// Pendant qu'on choisit une nouvelle touche : le raccourci actuel ne doit pas fermer la fenêtre.
#[tauri::command]
pub fn pause_hotkey(app: AppHandle) {
    let _ = app.global_shortcut().unregister_all();
}

/// Nouveau raccourci, actif tout de suite et retenu dans config.json. S'il est refusé (déjà pris
/// par une autre application), l'ancien est remis.
#[tauri::command]
pub fn set_hotkey(app: AppHandle, hotkey: String) -> Result<String, String> {
    let _ = app.global_shortcut().unregister_all();
    let old = HOTKEY.lock().unwrap().clone();
    if let Err(e) = register(&app, &hotkey) {
        let _ = register(&app, &old);
        return Err(e);
    }
    *HOTKEY.lock().unwrap() = hotkey.clone();
    save_setting(&app, "hotkey", serde_json::Value::String(hotkey.clone()));
    if let Some(item) = TOGGLE_ITEM.lock().unwrap().as_ref() {
        let _ = item.set_text(format!("Afficher / masquer  ({hotkey})"));
    }
    if let Some(tray) = app.tray_by_id("main") {
        let _ = tray.set_tooltip(Some(format!("Valo Overlay — {hotkey}")));
    }
    Ok(hotkey)
}

/// « Maintenir » : visible seulement tant que la touche est enfoncée.
#[tauri::command]
pub fn set_hold(app: AppHandle, hold: bool) {
    HOLD.store(hold, Ordering::Relaxed);
    save_setting(&app, "mode", serde_json::Value::String(if hold { "hold" } else { "toggle" }.into()));
}

/// Taille par défaut en part de l'écran : 90 % de la hauteur, 92 % de la largeur au plus
/// (en 16:9 ≈ 1728×972 en 1080p ; en 4:3 la fenêtre prend toute la hauteur utile ; sur un écran
/// ultra-large, elle reste en 16:9).
fn default_frac(mw: f64, mh: f64) -> (f64, f64) {
    let height = mh * 0.9;
    let width = (mw * 0.92).min(height * 16.0 / 9.0);
    (width / mw, height / mh)
}

fn monitor_size(w: &WebviewWindow) -> Option<(u32, u32)> {
    let m = w.current_monitor().ok().flatten().or_else(|| w.primary_monitor().ok().flatten())?;
    Some((m.size().width, m.size().height))
}

/// Dimensionne la fenêtre pour l'écran actuel (taille choisie, sinon par défaut) et la recentre
/// si l'écran ou sa résolution a changé depuis la dernière ouverture.
fn place(w: &WebviewWindow, force_center: bool) {
    let Some((mw, mh)) = monitor_size(w) else { return };
    let (fw, fh) = SIZE.lock().unwrap().unwrap_or_else(|| default_frac(mw as f64, mh as f64));
    let width = (fw * mw as f64).round().max(640.0) as u32;
    let height = (fh * mh as f64).round().max(360.0) as u32;
    let changed = LAST_MONITOR.lock().unwrap().replace((mw, mh)) != Some((mw, mh));
    let current = w.outer_size().ok();
    if current.is_none_or(|s| s.width.abs_diff(width) > 2 || s.height.abs_diff(height) > 2) {
        *PROGRAMMATIC.lock().unwrap() = Some(Instant::now());
        let _ = w.set_size(PhysicalSize::new(width, height));
    }
    // Hors de l'écran (changement de résolution) : recentrée
    let outside = w.outer_position().ok().is_none_or(|p| p.x < -50 || p.y < -50 || p.x as i64 + 100 > mw as i64 || p.y as i64 + 60 > mh as i64);
    if force_center || changed || outside {
        let _ = w.center();
    }
}

/// Retient la taille choisie (écrite sur disque une fois le redimensionnement terminé).
fn remember(fw: f64, fh: f64) {
    let v = (fw.clamp(0.2, 1.0), fh.clamp(0.2, 1.0));
    *SIZE.lock().unwrap() = Some(v);
    let gen = SAVE_GEN.fetch_add(1, Ordering::Relaxed) + 1;
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(600));
        if SAVE_GEN.load(Ordering::Relaxed) != gen {
            return;
        }
        if let Some(f) = SIZE_FILE.lock().unwrap().clone() {
            let _ = std::fs::write(f, serde_json::to_vec(&v).unwrap_or_default());
        }
    });
}

/// Bouton « réduire / agrandir » : format compact ↔ taille par défaut. Renvoie vrai si compact.
#[tauri::command]
pub fn toggle_compact(app: AppHandle) -> bool {
    let Some(w) = app.get_webview_window(LABEL) else { return false };
    let Some((mw, mh)) = monitor_size(&w) else { return false };
    let (dw, dh) = default_frac(mw as f64, mh as f64);
    let (cw, _) = SIZE.lock().unwrap().unwrap_or((dw, dh));
    let compact = cw > dw * (COMPACT + 1.0) / 2.0;
    let (fw, fh) = if compact { (dw * COMPACT, dh * COMPACT) } else { (dw, dh) };
    remember(fw, fh);
    place(&w, true);
    compact
}

/// Format actuel (pour l'icône du bouton au démarrage).
#[tauri::command]
pub fn is_compact(app: AppHandle) -> bool {
    let Some(w) = app.get_webview_window(LABEL) else { return false };
    let Some((mw, mh)) = monitor_size(&w) else { return false };
    let (dw, _) = default_frac(mw as f64, mh as f64);
    SIZE.lock().unwrap().is_some_and(|(cw, _)| cw <= dw * (COMPACT + 1.0) / 2.0)
}

#[cfg(windows)]
mod native {
    use tauri::WebviewWindow;
    use windows_sys::Win32::Foundation::HWND;
    use windows_sys::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND};
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, IsWindow, SetForegroundWindow};

    pub fn handle(w: &WebviewWindow) -> isize {
        w.hwnd().map_or(0, |h| h.0 as isize)
    }

    pub fn foreground() -> isize {
        unsafe { GetForegroundWindow() as isize }
    }

    pub fn focus(hwnd: isize) {
        if hwnd == 0 {
            return;
        }
        let h = hwnd as HWND;
        unsafe {
            if IsWindow(h) != 0 {
                SetForegroundWindow(h);
            }
        }
    }

    /// Coins arrondis natifs de Windows 11 (sans effet sur Windows 10).
    pub fn round_corners(w: &WebviewWindow) {
        let h = handle(w) as HWND;
        if h.is_null() {
            return;
        }
        let pref = DWMWCP_ROUND;
        unsafe {
            DwmSetWindowAttribute(
                h,
                DWMWA_WINDOW_CORNER_PREFERENCE as u32,
                &pref as *const _ as *const _,
                std::mem::size_of_val(&pref) as u32,
            );
        }
    }
}

#[cfg(not(windows))]
mod native {
    use tauri::WebviewWindow;

    pub fn handle(_: &WebviewWindow) -> isize {
        0
    }
    pub fn foreground() -> isize {
        0
    }
    pub fn focus(_: isize) {}
    pub fn round_corners(_: &WebviewWindow) {}
}

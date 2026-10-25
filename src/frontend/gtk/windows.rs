//! Windows desktop integration. All GTK and tray operations stay on the GTK thread.
use super::window::Window;
use adw::prelude::*;
use gtk::{gio, glib, glib::clone, subclass::prelude::ObjectSubclassIsExt};
use serde::{Deserialize, Serialize};
use std::{cell::RefCell, path::PathBuf, rc::Rc, time::Duration};
use tray_icon::{
    menu::{Menu, MenuEvent, MenuItem},
    Icon, TrayIconBuilder, TrayIconEvent,
};
use winreg::{
    enums::{HKEY_CURRENT_USER, KEY_READ},
    RegKey,
};

const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const RUN_NAME: &str = "MTRLANCopyPortable";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
struct Settings {
    close_to_tray: bool,
    start_minimized: bool,
    font_size: u32,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            close_to_tray: true,
            start_minimized: false,
            font_size: 18,
        }
    }
}

fn config_path() -> PathBuf {
    std::env::var_os("LAN_MOUSE_CONFIG")
        .map(PathBuf::from)
        .or_else(|| crate::config::default_config_path().ok())
        .expect("config path initialized")
}
fn settings_path() -> PathBuf {
    config_path().with_file_name("desktop-settings.toml")
}
fn save(settings: &Settings) -> anyhow::Result<()> {
    crate::config::atomic_write(
        &settings_path(),
        toml::to_string_pretty(settings)?.as_bytes(),
    )
}
fn startup_command() -> anyhow::Result<String> {
    let executable = std::env::current_exe()?;
    let config = std::path::absolute(config_path())?;
    Ok(format!(
        "\"{}\" --minimized --config \"{}\"",
        executable.display(),
        config.display()
    ))
}
fn startup_enabled() -> bool {
    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey_with_flags(RUN_KEY, KEY_READ)
        .and_then(|key| key.get_value::<String, _>(RUN_NAME))
        .is_ok()
}
fn set_startup(enabled: bool) -> anyhow::Result<()> {
    let (key, _) = RegKey::predef(HKEY_CURRENT_USER).create_subkey(RUN_KEY)?;
    if enabled {
        key.set_value(RUN_NAME, &startup_command()?)?;
    } else {
        match key.delete_value(RUN_NAME) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

fn open_path(path: PathBuf, window: &Window) {
    match gio::File::for_path(path).uri().as_str() {
        uri => {
            if let Err(e) =
                gio::AppInfo::launch_default_for_uri(uri, None::<&gio::AppLaunchContext>)
            {
                window.show_toast(&format!("Could not open location: {e}"));
            }
        }
    }
}

pub fn setup(app: &adw::Application, window: &Window) {
    let settings = match std::fs::read_to_string(settings_path()) {
        Ok(text) => match toml::from_str::<Settings>(&text) {
            Ok(settings) => settings,
            Err(e) => {
                window.show_toast(&format!("Invalid desktop settings: {e}"));
                Settings::default()
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Settings::default(),
        Err(e) => {
            window.show_toast(&format!("Cannot read desktop settings: {e}"));
            Settings::default()
        }
    };
    let settings = Rc::new(RefCell::new(settings));
    let font_provider = gtk::CssProvider::new();
    let font_size = match settings.borrow().font_size {
        16 => 16,
        20 => 20,
        _ => 18,
    };
    font_provider.load_from_data(&format!(".lan-mouse {{ font-size: {font_size}px; }}"));
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &font_provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 1,
        );
    }
    let appearance = adw::PreferencesGroup::builder().title("Appearance").build();
    let text_size =
        gtk::DropDown::from_strings(&["Standard · 16 px", "Large · 18 px", "Extra large · 20 px"]);
    text_size.set_valign(gtk::Align::Center);
    text_size.set_selected(match font_size {
        18 => 1,
        20 => 2,
        _ => 0,
    });
    let text_row = adw::ActionRow::builder()
        .title("Text size")
        .subtitle("Applies immediately across the app")
        .subtitle_lines(0)
        .build();
    text_row.add_suffix(&text_size);
    appearance.add(&text_row);
    text_size.connect_selected_notify(clone!(@weak window, @strong settings, @strong font_provider => move |dropdown| {
        let font_size = match dropdown.selected() { 1 => 18, 2 => 20, _ => 16 };
        let mut updated = settings.borrow().clone(); updated.font_size = font_size;
        match save(&updated) {
            Ok(()) => {
                *settings.borrow_mut() = updated;
                font_provider.load_from_data(&format!(".lan-mouse {{ font-size: {font_size}px; }}"));
            },
            Err(e) => window.show_toast(&format!("Could not save text size: {e}")),
        }
    }));
    window.imp().preferences_box.append(&appearance);
    let group = adw::PreferencesGroup::builder()
        .title("Windows integration")
        .description("Choose how the app behaves when you sign in or close its window.")
        .build();
    let close = gtk::Switch::builder()
        .active(settings.borrow().close_to_tray)
        .valign(gtk::Align::Center)
        .build();
    let close_row = adw::ActionRow::builder()
        .title("Close window to tray")
        .subtitle("Keep the background service running; use Quit to exit")
        .subtitle_lines(0)
        .activatable_widget(&close)
        .build();
    close_row.add_suffix(&close);
    group.add(&close_row);
    close.connect_state_set(clone!(@weak window, @strong settings => @default-return glib::Propagation::Stop, move |_, active| {
        let mut updated = settings.borrow().clone(); updated.close_to_tray = active;
        match save(&updated) {
            Ok(()) => { *settings.borrow_mut() = updated; glib::Propagation::Proceed },
            Err(e) => { window.show_toast(&format!("Could not save settings: {e}")); glib::Propagation::Stop }
        }
    }));
    let startup = gtk::Switch::builder()
        .active(startup_enabled())
        .valign(gtk::Align::Center)
        .build();
    let startup_row = adw::ActionRow::builder()
        .title("Start with Windows")
        .subtitle("Current user only; launches quietly in the tray at sign-in")
        .subtitle_lines(0)
        .activatable_widget(&startup)
        .build();
    if let Ok(key) = RegKey::predef(HKEY_CURRENT_USER).open_subkey_with_flags(RUN_KEY, KEY_READ) {
        if let Ok(actual) = key.get_value::<String, _>(RUN_NAME) {
            if startup_command()
                .map(|expected| actual != expected)
                .unwrap_or(true)
            {
                startup_row.set_subtitle(
                    "Startup points to another location. Switch off, then on to use this folder.",
                );
            }
        }
    }
    startup_row.add_suffix(&startup);
    group.add(&startup_row);
    startup.connect_state_set(clone!(@weak window, @weak startup_row => @default-return glib::Propagation::Stop, move |_, active| {
        match set_startup(active) {
            Ok(()) => { startup_row.set_subtitle("Current user only; launches quietly in the tray at sign-in"); window.show_toast(if active { "Windows startup enabled for this folder" } else { "Windows startup disabled" }); glib::Propagation::Proceed },
            Err(e) => { window.show_toast(&format!("Could not change Windows startup: {e}")); glib::Propagation::Stop }
        }
    }));
    let minimized = gtk::Switch::builder()
        .active(settings.borrow().start_minimized)
        .valign(gtk::Align::Center)
        .build();
    let minimized_row = adw::ActionRow::builder()
        .title("Open in tray")
        .subtitle("Hide the main window on ordinary launches")
        .subtitle_lines(0)
        .activatable_widget(&minimized)
        .build();
    minimized_row.add_suffix(&minimized);
    group.add(&minimized_row);
    minimized.connect_state_set(clone!(@weak window, @strong settings => @default-return glib::Propagation::Stop, move |_, active| {
        let mut updated = settings.borrow().clone(); updated.start_minimized = active;
        match save(&updated) {
            Ok(()) => { *settings.borrow_mut() = updated; glib::Propagation::Proceed },
            Err(e) => { window.show_toast(&format!("Could not save settings: {e}")); glib::Propagation::Stop }
        }
    }));
    window.imp().preferences_box.append(&group);
    let diagnostics = adw::PreferencesGroup::builder()
        .title("Diagnostics and controls")
        .build();
    let hide = gtk::Button::with_label("Minimize to tray");
    hide.connect_clicked(clone!(@weak window => move |_| window.set_visible(false)));
    let logs = gtk::Button::with_label("Open logs");
    logs.connect_clicked(clone!(@weak window => move |_| {
        let path = config_path().with_file_name("logs"); let _ = std::fs::create_dir_all(&path); open_path(path, &window);
    }));
    let folder = gtk::Button::with_label("Settings folder");
    folder.connect_clicked(clone!(@weak window => move |_| open_path(config_path().parent().unwrap().to_path_buf(), &window)));
    let quit = gtk::Button::with_label("Quit");
    quit.connect_clicked(clone!(@weak app => move |_| app.quit()));
    for (title, subtitle, button) in [
        (
            "Keep working in the background",
            "Restore the window from the notification-area icon",
            &hide,
        ),
        (
            "Application logs",
            "Open diagnostic logs without a console window",
            &logs,
        ),
        (
            "Portable settings",
            "View config.toml and desktop preferences",
            &folder,
        ),
        (
            "Exit MTR-LAN-Copy",
            "Stop the window, tray icon and background service",
            &quit,
        ),
    ] {
        button.set_valign(gtk::Align::Center);
        let row = adw::ActionRow::builder()
            .title(title)
            .subtitle(subtitle)
            .subtitle_lines(0)
            .build();
        row.add_suffix(button);
        diagnostics.add(&row);
    }
    window.imp().preferences_box.append(&diagnostics);
    let security = adw::PreferencesGroup::builder()
        .title("Security and compatibility")
        .build();
    let warning = adw::ActionRow::builder().title("Use trusted private networks only")
        .subtitle("This fork has no encryption or authentication. Host input capture is opt-in. No firewall rules are installed automatically.").subtitle_lines(0).build();
    security.add(&warning);
    window.imp().preferences_box.append(&security);

    let menu = Menu::new();
    let show_item = MenuItem::new("Open MTR-LAN-Copy", true, None);
    let hide_item = MenuItem::new("Minimize to tray", true, None);
    let logs_item = MenuItem::new("Open logs", true, None);
    let quit_item = MenuItem::new("Quit MTR-LAN-Copy", true, None);
    let tray = (|| -> anyhow::Result<_> {
        menu.append_items(&[&show_item, &hide_item, &logs_item, &quit_item])?;
        let mut pixels = vec![0u8; 32 * 32 * 4];
        for y in 0..32 {
            for x in 0..32 {
                let i = (y * 32 + x) * 4;
                if (x as i32 - 16).pow(2) * 2 + (y as i32 - 17).pow(2) < 210 {
                    pixels[i..i + 4].copy_from_slice(
                        if (x == 16 && y < 18) || (y == 15 && x > 6 && x < 26) {
                            &[255, 255, 255, 255]
                        } else {
                            &[66, 140, 245, 255]
                        },
                    );
                }
            }
        }
        Ok(TrayIconBuilder::new()
            .with_tooltip("MTR-LAN-Copy · right-click for controls")
            .with_menu(Box::new(menu))
            .with_icon(Icon::from_rgba(pixels, 32, 32)?)
            .build()?)
    })();
    let tray = match tray {
        Ok(tray) => Rc::new(tray),
        Err(e) => {
            hide.set_sensitive(false);
            close.set_sensitive(false);
            minimized.set_sensitive(false);
            window.show_toast(&format!(
                "Tray is unavailable: {e}. Closing the window exits."
            ));
            window.present();
            return;
        }
    };
    window.connect_close_request(clone!(@strong settings => move |window| {
        if settings.borrow().close_to_tray { window.set_visible(false); glib::Propagation::Stop }
        else { glib::Propagation::Proceed }
    }));
    let tooltip = Rc::new(RefCell::new(String::new()));
    glib::timeout_add_local(
        Duration::from_millis(100),
        clone!(@weak window, @weak app, @strong tray, @strong tooltip => @default-return glib::ControlFlow::Break, move || {
            // The Rc also keeps the native icon alive until the GTK loop exits.
            let _ = &tray;
            let role = window.imp().network_ui.borrow().as_ref().map(|ui| ui.last_role.get()).unwrap_or_default();
            let status = format!("MTR-LAN-Copy · {} · {:?} · {}", window.imp().identity_label.label(), role, window.imp().status_badge.label());
            if *tooltip.borrow() != status { let _ = tray.set_tooltip(Some(&status)); *tooltip.borrow_mut() = status; }
            while let Ok(event) = MenuEvent::receiver().try_recv() {
                if event.id == *show_item.id() { window.present(); }
                else if event.id == *hide_item.id() { window.set_visible(false); }
                else if event.id == *logs_item.id() { open_path(config_path().with_file_name("logs"), &window); }
                else if event.id == *quit_item.id() { app.quit(); }
            }
            while let Ok(event) = TrayIconEvent::receiver().try_recv() {
                if matches!(event, TrayIconEvent::DoubleClick { .. }) { window.present(); }
            }
            glib::ControlFlow::Continue
        }),
    );
    let minimized =
        settings.borrow().start_minimized || std::env::args().any(|s| s == "--minimized");
    if !minimized {
        window.present();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn desktop_defaults_do_not_enable_startup() {
        let defaults = Settings::default();
        assert!(defaults.close_to_tray);
        assert!(!defaults.start_minimized);
        // Startup is deliberately not a serialized preference: the actual Run key is authoritative.
        assert!(!toml::to_string(&defaults).unwrap().contains("startup"));
    }
    #[test]
    fn startup_paths_are_quoted() {
        let command = startup_command().unwrap();
        assert!(command.starts_with('"'));
        assert!(command.contains("\" --minimized --config \""));
        assert!(command.ends_with('"'));
    }

    #[test]
    fn old_preferences_get_readable_text_and_font_size_round_trips() {
        let old: Settings =
            toml::from_str("close_to_tray = true\nstart_minimized = false\n").unwrap();
        assert_eq!(old.font_size, 18);
        let custom = Settings {
            font_size: 20,
            ..old
        };
        let roundtrip: Settings = toml::from_str(&toml::to_string(&custom).unwrap()).unwrap();
        assert_eq!(roundtrip.font_size, 20);
    }
}

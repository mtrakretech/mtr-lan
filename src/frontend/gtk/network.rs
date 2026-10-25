use super::{client_row::ClientRow, window::Window};
use crate::{
    client::Position,
    config::Role,
    frontend::{FrontendEvent, ServiceStatus},
};
use adw::prelude::*;
use gtk::{glib, glib::clone, subclass::prelude::ObjectSubclassIsExt};
use std::{
    cell::{Cell, RefCell},
    io::{Read, Seek, SeekFrom},
    rc::Rc,
    time::Duration,
};

pub struct NetworkUi {
    name: gtk::Entry,
    role: gtk::DropDown,
    identity: adw::ActionRow,
    health: gtk::Label,
    discovered: gtk::ListBox,
    discovery_info: gtk::Label,
    dirty: Rc<Cell<bool>>,
    updating: Rc<Cell<bool>>,
    pending_name: Rc<RefCell<Option<String>>>,
    initialized: bool,
    pub(super) last_role: Rc<Cell<Role>>,
    signature: String,
}

fn label(text: &str, class: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.set_xalign(0.0);
    label.set_wrap(true);
    label.add_css_class(class);
    label
}
fn page(title: &str, description: &str) -> (gtk::ScrolledWindow, gtk::Box) {
    let body = gtk::Box::new(gtk::Orientation::Vertical, 24);
    body.add_css_class("workspace-page");
    body.append(&label(title, "page-title"));
    body.append(&label(description, "page-description"));
    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .hexpand(true)
        .child(&body)
        .build();
    (scroll, body)
}

pub fn setup(window: &Window) {
    window
        .imp()
        .identity_label
        .set_label(&crate::config::computer_name());
    let (scroll, pc) = page(
        "This PC",
        "Choose a name and decide which computer provides the keyboard and mouse.",
    );
    window.imp().local_pc_page.append(&scroll);
    let identity = adw::ActionRow::builder()
        .title(crate::config::computer_name())
        .subtitle("Checking local interfaces…")
        .subtitle_lines(0)
        .use_markup(false)
        .build();
    let info = gtk::ListBox::new();
    info.set_selection_mode(gtk::SelectionMode::None);
    info.add_css_class("boxed-list");
    info.append(&identity);
    pc.append(&info);
    let name = gtk::Entry::builder()
        .placeholder_text("Display name")
        .max_length(80)
        .width_chars(20)
        .valign(gtk::Align::Center)
        .build();
    let group = adw::PreferencesGroup::builder()
        .title("Identity and role")
        .build();
    let name_row = adw::ActionRow::builder()
        .title("Display name")
        .subtitle("Shown to other PCs; does not rename Windows itself")
        .subtitle_lines(0)
        .build();
    name_row.add_suffix(&name);
    group.add(&name_row);
    let role = gtk::DropDown::from_strings(&["Client · receive input", "Host · share this PC"]);
    role.set_valign(gtk::Align::Center);
    let role_row = adw::ActionRow::builder()
        .title("This PC is a")
        .subtitle("Set the other Windows PC to the opposite role")
        .subtitle_lines(0)
        .build();
    role_row.add_suffix(&role);
    group.add(&role_row);
    pc.append(&group);
    let save = gtk::Button::with_label("Save name and role");
    save.add_css_class("suggested-action");
    save.set_halign(gtk::Align::Start);
    pc.append(&save);
    let save_from_name = save.clone();
    name.connect_activate(move |_| save_from_name.emit_clicked());
    let dirty = Rc::new(Cell::new(false));
    let updating = Rc::new(Cell::new(false));
    let last_role = Rc::new(Cell::new(Role::Client));
    let name_save: Rc<RefCell<Option<glib::SourceId>>> = Rc::new(RefCell::new(None));
    let pending_name: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    name.connect_changed(
        clone!(@strong dirty, @strong updating => move |_| if !updating.get() { dirty.set(true); }),
    );
    role.connect_selected_notify(
        clone!(@strong dirty, @strong updating => move |_| if !updating.get() { dirty.set(true); }),
    );
    save.connect_clicked(clone!(@weak window, @weak name, @weak role, @strong last_role, @strong name_save, @strong pending_name => move |_| {
        if let Some(timer) = name_save.borrow_mut().take() {
            timer.remove();
        }
        let selected = if role.selected() == 1 { Role::Host } else { Role::Client };
        let name = name.text().trim().to_string();
        if name.is_empty() || name.chars().count() > 80 || name.chars().any(char::is_control) {
            window.show_toast("Choose a display name of 1–80 characters");
            return;
        }
        if selected == Role::Host && last_role.get() != Role::Host {
            let dialog = adw::MessageDialog::new(Some(&window), Some("Enable host input sharing?"), Some("Use a trusted private LAN only: this fork does not encrypt or authenticate traffic. Input is shared only with enabled, responding peers at their configured screen edge. Press Ctrl+Alt+Shift+F12 to release immediately. Secure desktops and elevated windows may not work."));
            dialog.add_responses(&[("cancel", "Cancel"), ("enable", "Enable host")]);
            dialog.set_close_response("cancel"); dialog.set_response_appearance("enable", adw::ResponseAppearance::Suggested);
            dialog.connect_response(None, clone!(@weak window, @strong pending_name => move |dialog, response| {
                if response == "enable" {
                    *pending_name.borrow_mut() = Some(name.clone());
                    window.request(FrontendEvent::SetIdentity(name.clone(), selected));
                }
                dialog.close();
            })); dialog.present();
        } else {
            *pending_name.borrow_mut() = Some(name.clone());
            window.request(FrontendEvent::SetIdentity(name, selected));
        }
    }));
    // The PC name saves itself shortly after typing; the role change keeps its
    // explicit confirmation because enabling the host is security-relevant.
    name.connect_changed(clone!(@strong name_save, @strong updating, @strong dirty, @weak name, @weak window, @strong last_role, @strong pending_name => @default-return (), move |_| {
        if updating.get() {
            return;
        }
        dirty.set(true);
        if let Some(existing) = name_save.borrow_mut().take() {
            existing.remove();
        }
        let role_for_save = last_role.clone();
        let pending_for_save = pending_name.clone();
        *name_save.borrow_mut() = Some(glib::timeout_add_local(
            std::time::Duration::from_millis(700),
            clone!(@weak name, @weak window => @default-return glib::ControlFlow::Break, move || {
                let trimmed = name.text().trim().to_string();
                let stored = window
                    .imp()
                    .network_ui
                    .borrow()
                    .as_ref()
                    .map(|ui| ui.identity.title().to_string())
                    .unwrap_or_default();
                let differs = stored.trim() != trimmed;
                let allowed = !trimmed.is_empty()
                    && trimmed.chars().count() <= 80
                    && trimmed.chars().all(|c| !c.is_control());
                let role_unchanged = window
                    .imp()
                    .network_ui
                    .borrow()
                    .as_ref()
                    .is_some_and(|ui| ui.role.selected() == u32::from(role_for_save.get() == Role::Host));
                if allowed && differs && role_unchanged {
                    *pending_for_save.borrow_mut() = Some(trimmed.clone());
                    window.request(FrontendEvent::SetIdentity(trimmed, role_for_save.get()));
                }
                glib::ControlFlow::Break
            }),
        ));
    }));
    let health = label("Waiting for role status…", "info-card");
    pc.append(&health);
    let release = gtk::Button::with_label("Release keyboard and mouse");
    release.set_halign(gtk::Align::Start);
    release.connect_clicked(
        clone!(@weak window => move |_| window.request(FrontendEvent::ReleaseInput())),
    );
    pc.append(&release);
    pc.append(&label("Coming back: while the other PC is controlling this one, push the cursor to the edge that faces it and hold it there about half a second; the gesture stays armed for a few seconds after shared input pauses. Emergency release (on whichever PC sends input): Ctrl + Alt + Shift + F12. After releasing, move the pointer away from the edge before entering again. Both PCs must run the companion app and explicitly enable each other. Discovery is not pairing or proof of trust.", "notice-card"));

    let (scroll, discover) = page(
        "Discover",
        "Compatible PCs announce their names automatically on the local network.",
    );
    window.imp().discovery_page.append(&scroll);
    let discovery_info = label("Starting discovery…", "muted");
    discover.append(&discovery_info);
    let discovered = gtk::ListBox::new();
    discovered.set_selection_mode(gtk::SelectionMode::None);
    discovered.add_css_class("boxed-list");
    let empty = label(
        "No compatible PCs found yet. Start desktop v6 on the other PC, then wait a few seconds.",
        "empty-state",
    );
    discovered.set_placeholder(Some(&empty));
    discover.append(&discovered);
    let manual = gtk::Button::with_label("Add by PC name or IP address");
    manual.set_halign(gtk::Align::Start);
    manual.connect_clicked(clone!(@weak window => move |_| window.request_client_create()));
    discover.append(&manual);
    discover.append(&label("Switches normally pass discovery within the same LAN. Routers, VLANs, guest Wi-Fi and client isolation may block multicast. Use a PC name/IP for routed connections. Allow input UDP 4242 and discovery UDP 4243 only on trusted private networks. PCs are never enabled automatically.", "notice-card"));

    setup_logs(window);
    window.imp().network_ui.replace(Some(NetworkUi {
        name,
        role,
        identity,
        health,
        discovered,
        discovery_info,
        dirty,
        updating,
        pending_name,
        initialized: false,
        last_role,
        signature: String::new(),
    }));
}

pub fn update(window: &Window, status: &ServiceStatus) {
    window.imp().identity_label.set_label(&status.device_name);
    window.imp().identity_label.set_tooltip_text(Some(&format!(
        "{} · {}",
        status.device_name, status.computer_name
    )));
    let mut ui = window.imp().network_ui.borrow_mut();
    let Some(ui) = ui.as_mut() else {
        return;
    };
    ui.identity.set_title(&status.device_name);
    ui.identity.set_subtitle(&format!(
        "Windows PC name: {}\nLocal IPv4: {}\nInput UDP {} · discovery UDP 4243",
        status.computer_name,
        status.local_addresses.join(", "),
        status.port
    ));
    if ui.pending_name.borrow().as_deref() == Some(status.device_name.as_str()) {
        ui.pending_name.borrow_mut().take();
        window.show_toast("Display name saved");
    }
    ui.last_role.set(status.role);
    if ui.name.text().trim() == status.device_name
        && ui.role.selected() == u32::from(status.role == Role::Host)
    {
        ui.dirty.set(false);
    }
    if !ui.initialized || !ui.dirty.get() {
        ui.updating.set(true);
        ui.name.set_text(&status.device_name);
        ui.role.set_selected(u32::from(status.role == Role::Host));
        ui.updating.set(false);
        ui.initialized = true;
    }
    ui.health.set_text(if status.role == Role::Host {
        if !status.capture_available { "Host capture is unavailable. Check Live logs." }
        else if status.sharing { "Sharing input now. Ctrl+Alt+Shift+F12 releases immediately." }
        else { "Host ready. Only enabled, responding peers can be entered at a configured screen edge. Physical cross-PC input still needs testing on both devices." }
    } else { "Client mode: local input is never captured. This PC accepts input only from explicitly enabled peers." });
    ui.discovery_info.set_text(&format!(
        "{}\n{} compatible PC(s) currently visible",
        status.discovery_status,
        status.discovered.len()
    ));
    let signature = format!(
        "{:?}|{:?}",
        status.discovered,
        status
            .peers
            .iter()
            .map(|p| (&p.hostname, &p.addresses))
            .collect::<Vec<_>>()
    );
    if signature != ui.signature {
        while let Some(child) = ui.discovered.first_child() {
            ui.discovered.remove(&child);
        }
        for device in &status.discovered {
            let row = adw::ActionRow::builder()
                .title(&device.name)
                .subtitle(format!(
                    "{} · {} · {:?} · UDP {}",
                    device.computer_name, device.address, device.role, device.port
                ))
                .subtitle_lines(0)
                .use_markup(false)
                .build();
            let existing = status.peers.iter().any(|p| {
                p.hostname.as_deref() == Some(&device.address)
                    || p.addresses.contains(&device.address)
            });
            let add = gtk::Button::with_label(if existing { "Added" } else { "Add computer" });
            add.set_valign(gtk::Align::Center);
            add.set_sensitive(!existing);
            let address = device.address.clone();
            let port = device.port;
            add.connect_clicked(clone!(@weak window => move |_| { window.request(FrontendEvent::AddClient(Some(address.clone()), port, Position::Right)); window.show_toast("Added disabled. Check the device and explicitly enable it on both PCs."); }));
            row.add_suffix(&add);
            ui.discovered.append(&row);
        }
        ui.signature = signature;
    }
    for (index, client) in window.clients().iter::<super::ClientObject>().enumerate() {
        let Ok(client) = client else {
            continue;
        };
        let Some(peer) = status.peers.iter().find(|p| p.handle == client.handle()) else {
            continue;
        };
        if let Some(row) = window
            .imp()
            .client_list
            .row_at_index(index as i32)
            .and_then(|r| r.downcast::<ClientRow>().ok())
        {
            row.set_title(
                peer.display_name
                    .as_deref()
                    .or(peer.hostname.as_deref())
                    .unwrap_or("New computer"),
            );
            let same_role = status.discovered.iter().any(|d| {
                d.role == status.role
                    && (peer.addresses.contains(&d.address)
                        || peer.hostname.as_deref() == Some(d.address.as_str()))
            });
            let state = if !peer.enabled {
                "Disabled".into()
            } else if same_role {
                format!("Role mismatch: both PCs are {:?}", status.role)
            } else if peer.responding {
                peer.latency_ms
                    .map(|ms| format!("Responding · ~{ms} ms"))
                    .unwrap_or_else(|| "Responding · RTT not measured".into())
            } else if peer.addresses.is_empty() {
                "Resolving name / no address found".into()
            } else {
                "No response · check app, firewall and peer enablement".into()
            };
            row.set_subtitle(&format!("{state}\n{}", peer.addresses.join(", ")));
        }
    }
}

fn setup_logs(window: &Window) {
    let body = gtk::Box::new(gtk::Orientation::Vertical, 18);
    body.add_css_class("workspace-page");
    body.set_vexpand(true);
    body.append(&label("Live logs", "page-title"));
    body.append(&label(
        "Live local diagnostics, refreshed every second. Keyboard text is not recorded.",
        "page-description",
    ));
    let controls = gtk::Box::new(gtk::Orientation::Horizontal, 16);
    let source = gtk::DropDown::from_strings(&["Background service", "Application"]);
    let follow = gtk::CheckButton::with_label("Follow latest");
    follow.set_active(true);
    controls.append(&source);
    controls.append(&follow);
    body.append(&controls);
    let text = gtk::TextView::builder()
        .editable(false)
        .cursor_visible(false)
        .monospace(true)
        .wrap_mode(gtk::WrapMode::WordChar)
        .vexpand(true)
        .build();
    text.add_css_class("log-view");
    let scroll = gtk::ScrolledWindow::builder()
        .vexpand(true)
        .hexpand(true)
        .child(&text)
        .build();
    body.append(&scroll);
    let info = label(
        "Showing at most the last 64 KiB. Logs stay on this PC.",
        "muted",
    );
    body.append(&info);
    window.imp().logs_page.append(&body);
    let previous = Rc::new(RefCell::new(String::new()));
    glib::timeout_add_local(
        Duration::from_secs(1),
        clone!(@weak window, @weak text, @weak source, @weak follow, @weak info, @strong previous => @default-return glib::ControlFlow::Break, move || {
            let base = std::env::var_os("LAN_MOUSE_CONFIG").map(std::path::PathBuf::from).or_else(|| crate::config::default_config_path().ok());
            if let Some(base) = base {
                let path = base.with_file_name("logs").join(if source.selected() == 0 { "service.log" } else { "app.log" });
                let result = (|| -> std::io::Result<String> {
                    let mut file = std::fs::File::open(&path)?; let length = file.metadata()?.len(); file.seek(SeekFrom::Start(length.saturating_sub(65536)))?;
                    let mut data = Vec::new(); file.take(65536).read_to_end(&mut data)?; Ok(String::from_utf8_lossy(&data).into_owned())
                })();
                match result {
                    Ok(data) => { if *previous.borrow() != data { text.buffer().set_text(&data); if follow.is_active() { let mut end = text.buffer().end_iter(); text.scroll_to_iter(&mut end, 0.0, false, 0.0, 1.0); } *previous.borrow_mut() = data; } info.set_text(&format!("{} · last 64 KiB · updated live", path.display())); },
                    Err(e) => info.set_text(&format!("Waiting for {}: {e}", path.display())),
                }
            }
            glib::ControlFlow::Continue
        }),
    );
}

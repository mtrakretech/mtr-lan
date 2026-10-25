mod imp;

use std::io::Write;

use adw::prelude::*;
use adw::subclass::prelude::*;
use glib::{clone, Object};
use gtk::{
    gio,
    glib::{self, closure_local},
    NoSelection,
};

use crate::{
    client::{Client, ClientHandle, Position},
    config::DEFAULT_PORT,
    frontend::{gtk::client_object::ClientObject, FrontendEvent},
};

use super::client_row::ClientRow;

glib::wrapper! {
    pub struct Window(ObjectSubclass<imp::Window>)
        @extends adw::ApplicationWindow, gtk::Window, gtk::Widget,
        @implements gio::ActionGroup, gio::ActionMap, gtk::Accessible, gtk::Buildable,
                    gtk::ConstraintTarget, gtk::Native, gtk::Root, gtk::ShortcutManager;
}

impl Window {
    pub(crate) fn new(app: &adw::Application) -> Self {
        Object::builder().property("application", app).build()
    }

    pub fn clients(&self) -> gio::ListStore {
        self.imp()
            .clients
            .borrow()
            .clone()
            .expect("Could not get clients")
    }

    fn setup_clients(&self) {
        let model = gio::ListStore::new::<ClientObject>();
        self.imp().clients.replace(Some(model));

        let selection_model = NoSelection::new(Some(self.clients()));
        self.imp().client_list.bind_model(
            Some(&selection_model),
            clone!(@weak self as window => @default-panic, move |obj| {
                let client_object = obj.downcast_ref().expect("Expected object of type `ClientObject`.");
                let row = window.create_client_row(client_object);
                row.connect_closure("request-update", false, closure_local!(@strong window => move |row: ClientRow, active: bool| {
                    let index = row.index() as u32;
                    let Some(client) = window.clients().item(index) else {
                        return;
                    };
                    let client = client.downcast_ref::<ClientObject>().unwrap();
                    window.request_client_update(client, active);
                }));
                row.connect_closure("request-delete", false, closure_local!(@strong window => move |row: ClientRow| {
                    let index = row.index() as u32;
                    window.request_client_delete(index);
                }));
                row.upcast()
            })
        );
    }

    /// workaround for a bug in libadwaita that shows an ugly line beneath
    /// the last element if a placeholder is set.
    /// https://gitlab.gnome.org/GNOME/gtk/-/merge_requests/6308
    pub fn set_placeholder_visible(&self, visible: bool) {
        let placeholder = self.imp().client_placeholder.get();
        self.imp().client_list.set_placeholder(match visible {
            true => Some(&placeholder),
            false => None,
        });
    }

    fn setup_icon(&self) {
        self.set_icon_name(Some("de.feschber.LanMouse"));
    }

    fn create_client_row(&self, client_object: &ClientObject) -> ClientRow {
        let row = ClientRow::new(client_object);
        row.bind(client_object);
        row.set_expanded(client_object.get_data().hostname.is_none());
        // collapsing a row commits its edits, exactly like leaving a text field
        row.connect_notify_local(
            Some("expanded"),
            clone!(@weak row => move |_, _| {
                if !row.is_expanded() {
                    row.save_now_with(None);
                }
            }),
        );
        row
    }

    pub fn new_client(&self, client: Client, active: bool) {
        let client = ClientObject::new(client, active);
        self.clients().append(&client);
        self.set_placeholder_visible(false);
    }

    pub fn client_idx(&self, handle: ClientHandle) -> Option<usize> {
        self.clients().iter::<ClientObject>().position(|c| {
            if let Ok(c) = c {
                c.handle() == handle
            } else {
                false
            }
        })
    }

    pub fn delete_client(&self, handle: ClientHandle) {
        let Some(idx) = self.client_idx(handle) else {
            log::warn!("could not find client with handle {handle}");
            return;
        };

        self.clients().remove(idx as u32);
        if self.clients().n_items() == 0 {
            self.set_placeholder_visible(true);
        }
    }

    pub fn update_client(&self, client: Client) {
        let Some(idx) = self.client_idx(client.handle) else {
            log::warn!("could not find client with handle {}", client.handle);
            return;
        };
        let client_object = self.clients().item(idx as u32).unwrap();
        let client_object: &ClientObject = client_object.downcast_ref().unwrap();
        let data = client_object.get_data();
        if data.display_name != client.display_name {
            client_object.set_display_name(client.display_name.clone().unwrap_or_default());
        }

        /* only change if it actually has changed, otherwise
         * the update signal is triggered */
        if data.hostname != client.hostname {
            client_object.set_hostname(client.hostname.unwrap_or("".into()));
        }
        if data.port != client.port as u32 {
            client_object.set_port(client.port as u32);
        }
        if data.position != client.pos.to_string() {
            client_object.set_position(client.pos.to_string());
        }
    }

    pub fn activate_client(&self, handle: ClientHandle, active: bool) {
        let Some(idx) = self.client_idx(handle) else {
            log::warn!("could not find client with handle {handle}");
            return;
        };
        let client_object = self.clients().item(idx as u32).unwrap();
        let client_object: &ClientObject = client_object.downcast_ref().unwrap();
        let data = client_object.get_data();
        if data.active != active {
            client_object.set_active(active);
            log::debug!("set active to {active}");
        }
    }

    pub fn request_client_create(&self) {
        self.imp().main_stack.set_visible_child_name("connections");
        let event = FrontendEvent::AddClient(None, DEFAULT_PORT, Position::default());
        self.request(event);
    }

    pub fn request_port_change(&self) {
        let port = self.imp().port_entry.get().text().to_string();
        if let Some(port) = port.as_str().parse::<u16>().ok().filter(|p| *p != 0) {
            self.request(FrontendEvent::ChangePort(port));
            self.show_toast("Listening port saved");
        } else {
            self.show_toast("Enter a port between 1 and 65535");
        }
    }

    pub fn request_client_update(&self, client: &ClientObject, active: bool) {
        let data = client.get_data();
        let display_name = data.display_name.clone().unwrap_or_default();
        let position = match Position::try_from(data.position.as_str()) {
            Ok(pos) => pos,
            _ => {
                log::error!("invalid position: {}", data.position);
                return;
            }
        };
        let hostname = data.hostname;
        let port = data.port as u16;

        let event = FrontendEvent::UpdateClient(client.handle(), hostname, port, position);
        log::debug!("requesting update: {event:?}");
        self.request(event);

        let event = FrontendEvent::ActivateClient(client.handle(), active);
        self.request(FrontendEvent::RenamePeer(client.handle(), display_name));
        log::debug!("requesting activate: {event:?}");
        self.request(event);
        self.show_toast("Peer settings saved");
    }

    pub fn request_client_delete(&self, idx: u32) {
        if let Some(obj) = self.clients().item(idx) {
            let client_object: &ClientObject = obj
                .downcast_ref()
                .expect("Expected object of type `ClientObject`.");
            let handle = client_object.handle();
            let event = FrontendEvent::DelClient(handle);
            self.request(event);
        }
    }

    pub(super) fn request(&self, event: FrontendEvent) {
        let json = serde_json::to_string(&event).unwrap();
        log::debug!("requesting {json}");
        let mut stream = self.imp().stream.borrow_mut();
        let Some(stream) = stream.as_mut() else {
            return;
        };
        let bytes = json.as_bytes();
        let len = bytes.len().to_be_bytes();
        if let Err(e) = stream.write_all(&len) {
            log::error!("error sending message: {e}");
        };
        if let Err(e) = stream.write_all(bytes) {
            log::error!("error sending message: {e}");
        };
    }

    pub fn update_status(&self, status: &crate::frontend::ServiceStatus) {
        super::network::update(self, status);
        self.imp().status_row.set_title("Service is running");
        self.imp().status_row.set_subtitle(&format!(
            "Listening on UDP {} · up {}m {:02}s\n{}",
            status.port,
            status.uptime_secs / 60,
            status.uptime_secs % 60,
            if status.role == crate::config::Role::Host {
                if status.capture_available {
                    "Host · move to a responding client's screen edge to share"
                } else {
                    "Host capture unavailable — check Live logs"
                }
            } else {
                "Client · ready to receive from explicitly enabled hosts"
            }
        ));
        self.imp()
            .status_badge
            .set_label(if status.sharing { "SHARING" } else { "ONLINE" });
        self.imp().status_badge.remove_css_class("offline");
        self.imp().sidebar_status.set_label("●  Service online");
        self.imp()
            .peers_metric
            .set_label(&format!("{} / {}", status.enabled, status.configured));
        self.imp()
            .received_metric
            .set_label(&status.packets_received.to_string());
        self.imp()
            .sent_metric
            .set_label(&status.packets_sent.to_string());
        let last = status
            .last_received_secs
            .map(|s| format!("{s}s ago"))
            .unwrap_or_else(|| "none yet".into());
        self.imp()
            .traffic_row
            .set_title(&format!("Last received: {last}"));
        self.imp().traffic_row.set_subtitle("Enabled computers are not necessarily connected. Network activity does not verify a trusted peer.");
        if self.imp().port.get() != status.port {
            self.imp().set_port(status.port);
        }
    }

    pub fn show_toast(&self, msg: &str) {
        let toast = adw::Toast::new(msg);
        let toast_overlay = &self.imp().toast_overlay;
        toast_overlay.add_toast(toast);
    }

    pub fn service_error(&self, title: &str, details: &str) {
        self.imp().status_row.set_title(title);
        self.imp().status_row.set_subtitle(details);
        self.imp().status_badge.set_label("OFFLINE");
        self.imp().status_badge.add_css_class("offline");
        self.imp().sidebar_status.set_label("●  Service offline");
    }
}

mod imp;

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::glib::{self, Object};
use std::time::Duration;

use crate::config::DEFAULT_PORT;

use super::client_object::ClientData;
use super::ClientObject;
use gtk::glib::clone;

glib::wrapper! {
    pub struct ClientRow(ObjectSubclass<imp::ClientRow>)
    @extends gtk::ListBoxRow, gtk::Widget, adw::PreferencesRow, adw::ExpanderRow,
    @implements gtk::Accessible, gtk::Actionable, gtk::Buildable, gtk::ConstraintTarget;
}

/// Everything that defines one peer as stored in config.toml. Two snapshots
/// differing means the row must be saved again.
#[derive(Clone, PartialEq, Debug)]
pub(crate) struct RowSnapshot {
    display_name: Option<String>,
    hostname: Option<String>,
    port: u32,
    position: String,
    active: bool,
}

fn snapshot(data: &ClientData, active: Option<bool>) -> RowSnapshot {
    RowSnapshot {
        display_name: data.display_name.clone(),
        hostname: data.hostname.clone(),
        port: data.port,
        position: data.position.clone(),
        active: active.unwrap_or(data.active),
    }
}

impl ClientRow {
    pub fn new(_client_object: &ClientObject) -> Self {
        Object::builder().build()
    }

    pub fn bind(&self, client_object: &ClientObject) {
        let imp = self.imp();
        imp.client.replace(Some(client_object.clone()));
        // Baseline: everything currently shown matches the service state, so a
        // save is only requested on real edits, not on service echo round-trips.
        *imp.last_emitted.borrow_mut() = Some(snapshot(&client_object.get_data(), None));

        let mut bindings = imp.bindings.borrow_mut();
        let display_binding = client_object
            .bind_property("display-name", &imp.display_name.get(), "text")
            .transform_to(|_, value: Option<String>| Some(value.unwrap_or_default()))
            .transform_from(|_, value: String| {
                Some(if value.trim().is_empty() {
                    None
                } else {
                    Some(value)
                })
            })
            .bidirectional()
            .sync_create()
            .build();
        bindings.push(display_binding);

        let active_binding = client_object
            .bind_property("active", &imp.enable_switch.get(), "state")
            .bidirectional()
            .sync_create()
            .build();

        let switch_position_binding = client_object
            .bind_property("active", &imp.enable_switch.get(), "active")
            .bidirectional()
            .sync_create()
            .build();

        let hostname_binding = client_object
            .bind_property("hostname", &imp.hostname.get(), "text")
            .transform_to(|_, v: Option<String>| {
                if let Some(hostname) = v {
                    Some(hostname)
                } else {
                    Some("".to_string())
                }
            })
            .transform_from(|_, v: String| {
                if v.as_str().trim() == "" {
                    Some(None)
                } else {
                    Some(Some(v))
                }
            })
            .bidirectional()
            .sync_create()
            .build();

        let title_binding = client_object
            .bind_property("hostname", self, "title")
            .transform_to(|_, v: Option<String>| {
                if let Some(hostname) = v {
                    Some(hostname)
                } else {
                    Some("New computer".to_string())
                }
            })
            .sync_create()
            .build();

        let port_binding = client_object
            .bind_property("port", &imp.port.get(), "text")
            .transform_from(|_, v: String| {
                if v.is_empty() {
                    Some(DEFAULT_PORT as u32)
                } else {
                    Some(v.parse::<u16>().unwrap_or(DEFAULT_PORT) as u32)
                }
            })
            .transform_to(|_, v: u32| {
                if v == 4242 {
                    Some("".to_string())
                } else {
                    Some(v.to_string())
                }
            })
            .bidirectional()
            .sync_create()
            .build();

        let subtitle_binding = client_object
            .bind_property("port", self, "subtitle")
            .transform_to(|_, port: u32| Some(format!("UDP {port} · expand to edit")))
            .sync_create()
            .build();

        let position_binding = client_object
            .bind_property("position", &imp.position.get(), "selected")
            .transform_from(|_, v: u32| match v {
                1 => Some("right"),
                2 => Some("top"),
                3 => Some("bottom"),
                _ => Some("left"),
            })
            .transform_to(|_, v: String| match v.as_str() {
                "right" => Some(1),
                "top" => Some(2u32),
                "bottom" => Some(3u32),
                _ => Some(0u32),
            })
            .bidirectional()
            .sync_create()
            .build();

        bindings.push(active_binding);
        bindings.push(switch_position_binding);
        bindings.push(hostname_binding);
        bindings.push(title_binding);
        bindings.push(port_binding);
        bindings.push(subtitle_binding);
        bindings.push(position_binding);

        // Every peer edit persists by itself, shortly after the typing settles.
        let display_name = imp.display_name.get();
        display_name.connect_changed(clone!(@weak self as row => move |_| row.schedule_saving()));
        let hostname = imp.hostname.get();
        hostname.connect_changed(clone!(@weak self as row => move |_| row.schedule_saving()));
        let port = imp.port.get();
        port.connect_changed(clone!(@weak self as row => move |_| row.schedule_saving()));
        let position = imp.position.get();
        position
            .connect_selected_notify(clone!(@weak self as row => move |_| row.schedule_saving()));
    }

    /// Request a save now, or (with `active: Some`) persist an exact switch
    /// state the data store does not know about yet. Skipped when nothing
    /// differs from the last saved snapshot, so service acknowledgements never
    /// loop back into save requests.
    pub fn save_now_with(&self, active: Option<bool>) {
        let imp = self.imp();
        if let Some(source) = imp.scheduled.take() {
            source.remove();
        }
        let Some(client_object) = imp.client.borrow().clone() else {
            return;
        };
        let wanted = snapshot(&client_object.get_data(), active);
        if imp.last_emitted.borrow().as_ref() == Some(&wanted) {
            return;
        }
        *imp.last_emitted.borrow_mut() = Some(wanted.clone());
        self.emit_by_name::<()>("request-update", &[&wanted.active]);
    }

    /// Save shortly after the last edit, so fast typing sends one request and
    /// one `config.toml` write, not one per keystroke.
    fn schedule_saving(&self) {
        let imp = self.imp();
        if let Some(source) = imp.scheduled.take() {
            source.remove();
        }
        *imp.scheduled.borrow_mut() = Some(glib::timeout_add_local(
            Duration::from_millis(700),
            clone!(@strong self as row => @default-return glib::ControlFlow::Break, move || {
                *row.imp().scheduled.borrow_mut() = None;
                row.save_now_with(None);
                glib::ControlFlow::Break
            }),
        ));
    }

    pub fn unbind(&self) {
        for binding in self.imp().bindings.borrow_mut().drain(..) {
            binding.unbind();
        }
        let imp = self.imp();
        if let Some(source) = imp.scheduled.take() {
            source.remove();
        }
        imp.client.borrow_mut().take();
        imp.last_emitted.borrow_mut().take();
    }
}

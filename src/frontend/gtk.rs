mod client_object;
mod client_row;
mod network;
mod window;
#[cfg(windows)]
mod windows;

use std::{
    env,
    io::{ErrorKind, Read},
    process, str,
};

use crate::frontend::gtk::window::Window;

use adw::prelude::*;
use adw::Application;
use gtk::{
    gdk::Display, glib::clone, subclass::prelude::ObjectSubclassIsExt, CssProvider, IconTheme,
};
use gtk::{gio, glib, prelude::ApplicationExt};

use self::client_object::ClientObject;

use super::FrontendNotify;

pub fn run() -> bool {
    log::debug!("running gtk frontend");
    #[cfg(windows)]
    let result = std::thread::Builder::new()
        .stack_size(8 * 1024 * 1024) // https://gitlab.gnome.org/GNOME/gtk/-/commit/52dbb3f372b2c3ea339e879689c1de535ba2c2c3 -> caused crash on windows
        .name("gtk".into())
        .spawn(|| std::panic::catch_unwind(gtk_main));
    #[cfg(windows)]
    let ret = match result {
        Ok(worker) => match worker.join() {
            Ok(Ok(code)) => code,
            Ok(Err(payload)) => {
                log::error!("GTK frontend panicked: {}", panic_message(payload.as_ref()));
                glib::ExitCode::FAILURE
            }
            Err(payload) => {
                log::error!(
                    "GTK worker thread panicked: {}",
                    panic_message(payload.as_ref())
                );
                glib::ExitCode::FAILURE
            }
        },
        Err(error) => {
            log::error!("could not start GTK worker thread: {error}");
            glib::ExitCode::FAILURE
        }
    };
    #[cfg(not(windows))]
    let ret = gtk_main();

    if ret == glib::ExitCode::FAILURE {
        log::error!("frontend exited with failure");
    } else {
        log::info!("frontend exited successfully");
    }

    ret == glib::ExitCode::SUCCESS
}

#[cfg(windows)]
fn panic_message(payload: &(dyn std::any::Any + Send)) -> &str {
    payload
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| payload.downcast_ref::<&str>().copied())
        .unwrap_or("non-string panic payload")
}

fn gtk_main() -> glib::ExitCode {
    gio::resources_register_include!("lan-mouse.gresource").expect("Failed to register resources.");

    let app = Application::builder()
        .application_id("de.feschber.LanMouse")
        .build();

    app.connect_startup(|_| load_icons());
    app.connect_startup(|_| load_css());
    app.connect_startup(|app| {
        let quit = gio::SimpleAction::new("quit", None);
        quit.connect_activate(clone!(@weak app => move |_, _| app.quit()));
        app.add_action(&quit);
    });
    app.connect_activate(build_ui);

    let args: Vec<&'static str> = vec![];
    app.run_with_args(&args)
}

fn load_css() {
    let provider = CssProvider::new();
    provider.load_from_resource("de/feschber/LanMouse/style.css");
    gtk::style_context_add_provider_for_display(
        &Display::default().expect("Could not connect to a display."),
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}

fn load_icons() {
    let display = &Display::default().expect("Could not connect to a display.");
    let icon_theme = IconTheme::for_display(display);
    icon_theme.add_resource_path("/de/feschber/LanMouse/icons");
}

fn build_ui(app: &Application) {
    if let Some(window) = app.active_window() {
        window.present();
        return;
    }
    log::debug!("connecting to lan-mouse-socket");
    let mut rx = match super::wait_for_service() {
        Ok(stream) => stream,
        Err(e) => {
            log::error!("could not connect to lan-mouse-socket: {e}");
            let dialog = gtk::MessageDialog::builder()
                .text("MTR-LAN-Copy service is unavailable")
                .secondary_text(e.to_string())
                .buttons(gtk::ButtonsType::Close)
                .build();
            dialog.connect_response(
                clone!(@weak app => move |dialog, _| { dialog.close(); app.quit(); }),
            );
            dialog.present();
            return;
        }
    };
    let tx = match rx.try_clone() {
        Ok(sock) => sock,
        Err(e) => {
            log::error!("{e}");
            process::exit(1);
        }
    };
    log::debug!("connected to lan-mouse-socket");

    let (sender, receiver) = async_channel::bounded(10);

    gio::spawn_blocking(move || {
        match loop {
            // read length
            let mut len = [0u8; 8];
            match rx.read_exact(&mut len) {
                Ok(_) => (),
                Err(e) if e.kind() == ErrorKind::UnexpectedEof => break Ok(()),
                Err(e) => break Err(e),
            };
            let len = usize::from_be_bytes(len);
            if len > 1024 * 1024 {
                break Err(std::io::Error::new(
                    ErrorKind::InvalidData,
                    "Oversized service message",
                ));
            }

            // read payload
            let mut buf = vec![0u8; len];
            match rx.read_exact(&mut buf) {
                Ok(_) => (),
                Err(e) if e.kind() == ErrorKind::UnexpectedEof => break Ok(()),
                Err(e) => break Err(e),
            };

            // parse json
            let json = str::from_utf8(&buf).unwrap();
            match serde_json::from_str(json) {
                Ok(notify) => {
                    if sender.send_blocking(notify).is_err() {
                        break Ok(());
                    }
                }
                Err(e) => log::error!("{e}"),
            }
        } {
            Ok(()) => {}
            Err(e) => log::error!("{e}"),
        }
    });

    let window = Window::new(app);
    window.imp().stream.borrow_mut().replace(tx);
    network::setup(&window);
    let last_status = std::rc::Rc::new(std::cell::Cell::new(std::time::Instant::now()));
    let tick_status = last_status.clone();
    glib::timeout_add_local(
        std::time::Duration::from_secs(1),
        clone!(@weak window => @default-return glib::ControlFlow::Break, move || {
            if tick_status.get().elapsed().as_secs() > 6 {
                window.service_error("Service not responding", "No status reply for over 6 seconds. Quit and relaunch; check logs/service.log.");
            }
            window.request(super::FrontendEvent::Status());
            glib::ControlFlow::Continue
        }),
    );
    window.request(super::FrontendEvent::Status());
    glib::spawn_future_local(clone!(@weak window => async move {
        loop {
            let notify = match receiver.recv().await {
                Ok(n) => n,
                Err(_) => {
                    window.service_error("Background service stopped", "Quit and reopen MTR-LAN-Copy. See logs/service.log for details.");
                    window.show_toast("Connection to service lost");
                    break;
                }
            };
            match notify {
                FrontendNotify::Status(status) => { last_status.set(std::time::Instant::now()); window.update_status(&status); },
                FrontendNotify::ShowWindow() => window.present(),
                FrontendNotify::NotifyClientActivate(handle, active) => {
                    window.activate_client(handle, active);
                }
                FrontendNotify::NotifyClientCreate(client) => {
                    window.new_client(client, false);
                },
                FrontendNotify::NotifyClientUpdate(client) => {
                    window.update_client(client);
                }
                FrontendNotify::NotifyError(e) => {
                    window.show_toast(e.as_str());
                },
                FrontendNotify::NotifyClientDelete(client) => {
                    window.delete_client(client);
                }
                FrontendNotify::Enumerate(clients) => {
                    for (client, active) in clients {
                        if window.client_idx(client.handle).is_some() {
                            window.activate_client(client.handle, active);
                            window.update_client(client);
                        } else {
                            window.new_client(client, active);
                        }
                    }
                },
                FrontendNotify::NotifyPortChange(port, msg) => {
                    match msg {
                        None => window.show_toast(format!("port changed: {port}").as_str()),
                        Some(msg) => window.show_toast(msg.as_str()),
                    }
                    window.imp().set_port(port);
                }
            }
        }
    }));

    #[cfg(windows)]
    windows::setup(app, &window);
    #[cfg(not(windows))]
    window.present();
}

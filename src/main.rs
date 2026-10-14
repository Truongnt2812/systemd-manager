mod journal;
mod new_service;
mod systemd;
mod window;

use adw::prelude::*;
use gtk::glib;

const APP_ID: &str = "io.github.systemd_manager.SystemdManager";

fn main() -> glib::ExitCode {
    // Makes the X11 window class match the .desktop file name, so docks and
    // taskbars can find our icon (Wayland already uses the application id).
    glib::set_prgname(Some(APP_ID));
    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_startup(|_| gtk::Window::set_default_icon_name(APP_ID));
    app.connect_activate(|app| {
        if let Some(win) = app.active_window() {
            win.present();
            return;
        }
        let w = window::Window::build(app);
        w.window.present();
    });
    app.run()
}

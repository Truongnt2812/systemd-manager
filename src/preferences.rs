//! Preferences: light/dark style and fonts, saved in
//! `~/.config/systemd-manager/settings.ini`.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib::translate::IntoGlib;
use gtk::{glib, pango};

/// CSS class for the unit file and journal text, which use the monospace font.
pub const MONO_CLASS: &str = "sm-mono";

const GROUP: &str = "appearance";
const STYLES: [(&str, &str); 3] = [
    ("system", "Follow System"),
    ("light", "Light"),
    ("dark", "Dark"),
];
const DEFAULT_MONO: &str = "Monospace 11";

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Settings {
    /// "system", "light" or "dark".
    pub style: String,
    /// A Pango font description such as "Cantarell 12"; None is the system font.
    pub interface_font: Option<String>,
    /// Font for unit files and the journal; None is the theme's monospace font.
    pub monospace_font: Option<String>,
}

fn path() -> PathBuf {
    glib::user_config_dir()
        .join("systemd-manager")
        .join("settings.ini")
}

impl Settings {
    pub fn parse(text: &str) -> Settings {
        let file = glib::KeyFile::new();
        let mut s = Settings {
            style: "system".into(),
            ..Default::default()
        };
        if file.load_from_data(text, glib::KeyFileFlags::NONE).is_err() {
            return s;
        }
        let get = |key: &str| {
            file.string(GROUP, key)
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        if let Some(style) = get("style").filter(|v| STYLES.iter().any(|(k, _)| k == v)) {
            s.style = style;
        }
        s.interface_font = get("interface-font");
        s.monospace_font = get("monospace-font");
        s
    }

    pub fn to_ini(&self) -> String {
        let file = glib::KeyFile::new();
        file.set_string(GROUP, "style", &self.style);
        file.set_string(
            GROUP,
            "interface-font",
            self.interface_font.as_deref().unwrap_or(""),
        );
        file.set_string(
            GROUP,
            "monospace-font",
            self.monospace_font.as_deref().unwrap_or(""),
        );
        file.to_data().to_string()
    }

    pub fn load() -> Settings {
        Settings::parse(&std::fs::read_to_string(path()).unwrap_or_default())
    }

    fn save(&self) {
        let p = path();
        let res = p
            .parent()
            .map(std::fs::create_dir_all)
            .unwrap_or(Ok(()))
            .and_then(|_| std::fs::write(&p, self.to_ini()));
        if let Err(e) = res {
            eprintln!("Could not save {}: {e}", p.display());
        }
    }
}

/// CSS that sets a text view's font from a Pango font description.
pub fn font_css(selector: &str, desc: &str) -> String {
    let d = pango::FontDescription::from_string(desc);
    let mut rules = Vec::new();
    if let Some(family) = d.family().filter(|f| !f.is_empty()) {
        let family = family.replace('\\', "\\\\").replace('"', "\\\"");
        rules.push(format!("font-family: \"{family}\";"));
    }
    if d.size() > 0 {
        let size = d.size() as f64 / pango::SCALE as f64;
        let unit = if d.is_size_absolute() { "px" } else { "pt" };
        rules.push(format!("font-size: {size}{unit};"));
    }
    if d.set_fields().contains(pango::FontMask::WEIGHT) {
        rules.push(format!("font-weight: {};", d.weight().into_glib()));
    }
    match d.style() {
        pango::Style::Italic => rules.push("font-style: italic;".into()),
        pango::Style::Oblique => rules.push("font-style: oblique;".into()),
        _ => {}
    }
    format!("{selector} {{ {} }}", rules.join(" "))
}

thread_local! {
    static MONO_CSS: RefCell<Option<gtk::CssProvider>> = const { RefCell::new(None) };
}

/// Applies the settings to the whole app.
pub fn apply(s: &Settings) {
    adw::StyleManager::default().set_color_scheme(match s.style.as_str() {
        "light" => adw::ColorScheme::ForceLight,
        "dark" => adw::ColorScheme::ForceDark,
        _ => adw::ColorScheme::Default,
    });

    if let Some(gs) = gtk::Settings::default() {
        match &s.interface_font {
            Some(font) => gs.set_gtk_font_name(Some(font)),
            // Back to the desktop's font.
            None => gs.reset_property("gtk-font-name"),
        }
    }

    let Some(display) = gtk::gdk::Display::default() else {
        return;
    };
    MONO_CSS.with(|cell| {
        let mut cell = cell.borrow_mut();
        let provider = cell.get_or_insert_with(|| {
            let p = gtk::CssProvider::new();
            gtk::style_context_add_provider_for_display(
                &display,
                &p,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
            p
        });
        let css = s
            .monospace_font
            .as_deref()
            .map(|f| font_css(&format!("textview.{MONO_CLASS}"), f))
            .unwrap_or_default();
        provider.load_from_string(&css);
    });
}

/// A font row: a font button plus a button that goes back to the default.
fn font_row(
    title: &str,
    subtitle: &str,
    current: Option<&str>,
    fallback: &str,
    on_change: impl Fn(Option<String>) + 'static,
) -> adw::ActionRow {
    let button = gtk::FontDialogButton::builder()
        .dialog(&gtk::FontDialog::builder().title(title).build())
        .font_desc(&pango::FontDescription::from_string(
            current.unwrap_or(fallback),
        ))
        .valign(gtk::Align::Center)
        .build();
    let reset = gtk::Button::builder()
        .icon_name("edit-undo-symbolic")
        .tooltip_text("Use the default font")
        .valign(gtk::Align::Center)
        .css_classes(["flat"])
        .visible(current.is_some())
        .build();
    let row = adw::ActionRow::builder()
        .title(title)
        .subtitle(subtitle)
        .build();
    row.add_suffix(&reset);
    row.add_suffix(&button);

    let on_change = Rc::new(on_change);
    // Set while the reset button moves the font button back, so that change
    // isn't taken as the user picking a font.
    let resetting = Rc::new(std::cell::Cell::new(false));
    {
        let reset = reset.clone();
        let on_change = on_change.clone();
        let resetting = resetting.clone();
        button.connect_font_desc_notify(move |b| {
            if resetting.get() {
                return;
            }
            if let Some(d) = b.font_desc() {
                reset.set_visible(true);
                on_change(Some(d.to_string()));
            }
        });
    }
    let fallback = fallback.to_string();
    reset.connect_clicked(move |r| {
        r.set_visible(false);
        on_change(None);
        resetting.set(true);
        button.set_font_desc(&pango::FontDescription::from_string(&fallback));
        resetting.set(false);
    });
    row
}

pub fn show(parent: &impl IsA<gtk::Widget>) {
    let settings = Rc::new(RefCell::new(Settings::load()));
    let update = {
        let settings = settings.clone();
        move |f: &dyn Fn(&mut Settings)| {
            let mut s = settings.borrow_mut();
            f(&mut s);
            apply(&s);
            s.save();
        }
    };
    let update = Rc::new(update);

    let style_row = adw::ComboRow::builder()
        .title("Style")
        .model(&gtk::StringList::new(&STYLES.map(|(_, label)| label)))
        .selected(
            STYLES
                .iter()
                .position(|(k, _)| *k == settings.borrow().style)
                .unwrap_or(0) as u32,
        )
        .build();
    {
        let update = update.clone();
        style_row.connect_selected_notify(move |r| {
            let key = STYLES[(r.selected() as usize).min(STYLES.len() - 1)].0;
            update(&|s| s.style = key.into());
        });
    }
    let appearance = adw::PreferencesGroup::builder().title("Appearance").build();
    appearance.add(&style_row);

    // The desktop's font, read before any custom font replaces it.
    let system_font = gtk::Settings::default()
        .and_then(|gs| {
            let current = gs.gtk_font_name();
            gs.reset_property("gtk-font-name");
            let system = gs.gtk_font_name();
            gs.set_gtk_font_name(current.as_deref());
            system
        })
        .map(|f| f.to_string())
        .unwrap_or_else(|| "Sans 11".into());

    let s = settings.borrow().clone();
    let interface = {
        let update = update.clone();
        font_row(
            "Interface Font",
            "Text in lists, buttons and labels",
            s.interface_font.as_deref(),
            &system_font,
            move |font| update(&|s| s.interface_font = font.clone()),
        )
    };
    let mono = {
        let update = update.clone();
        font_row(
            "Monospace Font",
            "Unit files and the journal",
            s.monospace_font.as_deref(),
            DEFAULT_MONO,
            move |font| update(&|s| s.monospace_font = font.clone()),
        )
    };
    let fonts = adw::PreferencesGroup::builder().title("Fonts").build();
    fonts.add(&interface);
    fonts.add(&mono);

    let page = adw::PreferencesPage::builder()
        .title("General")
        .icon_name("preferences-system-symbolic")
        .build();
    page.add(&appearance);
    page.add(&fonts);
    let dialog = adw::PreferencesDialog::new();
    dialog.add(&page);
    dialog.present(Some(parent));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_round_trip() {
        let s = Settings {
            style: "dark".into(),
            interface_font: Some("Inter 12".into()),
            monospace_font: None,
        };
        assert_eq!(Settings::parse(&s.to_ini()), s);
    }

    #[test]
    fn bad_settings_fall_back() {
        let s = Settings::parse("[appearance]\nstyle=purple\n");
        assert_eq!(s.style, "system");
        assert_eq!(Settings::parse("not ini at all {").style, "system");
        assert_eq!(Settings::parse("").interface_font, None);
    }

    #[test]
    fn font_css_from_description() {
        assert_eq!(
            font_css("textview.x", "JetBrains Mono Bold 13"),
            "textview.x { font-family: \"JetBrains Mono\"; font-size: 13pt; font-weight: 700; }"
        );
        assert_eq!(
            font_css("t", "Fira Code Italic 10.5"),
            "t { font-family: \"Fira Code\"; font-size: 10.5pt; font-weight: 400; font-style: italic; }"
        );
    }
}

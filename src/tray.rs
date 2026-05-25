use tray_icon::{
    menu::{Menu, MenuItem, PredefinedMenuItem},
    Icon, TrayIcon, TrayIconBuilder,
};

/// Decode the bundled white monochrome tray icon (`assets/icon-tray.png`, 32×32
/// RGBA) into a `tray_icon::Icon`. Embedded via `include_bytes!` so the single
/// binary stays self-contained.
fn tray_icon_image() -> anyhow::Result<Icon> {
    const PNG: &[u8] = include_bytes!("../assets/icon-tray.png");
    let img = image::load_from_memory_with_format(PNG, image::ImageFormat::Png)?.into_rgba8();
    let (w, h) = (img.width(), img.height());
    Ok(Icon::from_rgba(img.into_raw(), w, h)?)
}

/// The system-tray icon plus the menu-item ids the app matches `MenuEvent`s
/// against. Labels are localized once at construction from the saved language;
/// the menu isn't rebuilt on a runtime language switch (it's created once).
pub struct SystemTray {
    _tray_icon: TrayIcon,
    pub open_item_id: String,
    pub start_item_id: String,
    pub stop_item_id: String,
    pub settings_item_id: String,
    pub quit_item_id: String,
}

impl SystemTray {
    /// Build the tray icon. `lang` is an i18n language code (`"ru"`/`"en"`) used
    /// to label the menu items. The menu only opens on right-click — left-click
    /// is handled by the app to show the window (`with_menu_on_left_click(false)`).
    pub fn new(lang: &str) -> anyhow::Result<Self> {
        use crate::i18n::tr;

        let tray_menu = Menu::new();
        let open_item = MenuItem::new(tr(lang, "tray.open"), true, None);
        let start_item = MenuItem::new(tr(lang, "tray.start"), true, None);
        let stop_item = MenuItem::new(tr(lang, "tray.stop"), true, None);
        let settings_item = MenuItem::new(tr(lang, "tray.settings"), true, None);
        let quit_item = MenuItem::new(tr(lang, "tray.quit"), true, None);

        let open_item_id = open_item.id().0.clone();
        let start_item_id = start_item.id().0.clone();
        let stop_item_id = stop_item.id().0.clone();
        let settings_item_id = settings_item.id().0.clone();
        let quit_item_id = quit_item.id().0.clone();

        tray_menu.append(&open_item)?;
        tray_menu.append(&PredefinedMenuItem::separator())?;
        tray_menu.append(&start_item)?;
        tray_menu.append(&stop_item)?;
        tray_menu.append(&PredefinedMenuItem::separator())?;
        tray_menu.append(&settings_item)?;
        tray_menu.append(&PredefinedMenuItem::separator())?;
        tray_menu.append(&quit_item)?;

        let icon = tray_icon_image()?;

        let tray_icon = TrayIconBuilder::new()
            .with_menu(Box::new(tray_menu))
            .with_menu_on_left_click(false)
            .with_tooltip("Zapret UI")
            .with_icon(icon)
            .build()?;

        Ok(Self {
            _tray_icon: tray_icon,
            open_item_id,
            start_item_id,
            stop_item_id,
            settings_item_id,
            quit_item_id,
        })
    }
}

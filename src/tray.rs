use tray_icon::{
    menu::{Menu, MenuItem},
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

pub struct SystemTray {
    _tray_icon: TrayIcon,
    pub show_item_id: String,
    pub quit_item_id: String,
}

impl SystemTray {
    pub fn new() -> anyhow::Result<Self> {
        let tray_menu = Menu::new();
        let show_item = MenuItem::new("Show", true, None);
        let quit_item = MenuItem::new("Quit", true, None);

        let show_item_id = show_item.id().0.clone();
        let quit_item_id = quit_item.id().0.clone();

        tray_menu.append(&show_item)?;
        tray_menu.append(&quit_item)?;

        let icon = tray_icon_image()?;

        let tray_icon = TrayIconBuilder::new()
            .with_menu(Box::new(tray_menu))
            .with_tooltip("Zapret UI")
            .with_icon(icon)
            .build()?;

        Ok(Self {
            _tray_icon: tray_icon,
            show_item_id,
            quit_item_id,
        })
    }
}

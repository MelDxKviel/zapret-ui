use tray_icon::{
    menu::{Menu, MenuItem},
    Icon, TrayIcon, TrayIconBuilder,
};

pub struct SystemTray {
    _tray_icon: TrayIcon,
    pub show_item_id: String,
    pub quit_item_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayMenuAction {
    Show,
    Quit,
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

        // Create a simple 16x16 icon
        let mut rgba = vec![0u8; 16 * 16 * 4];
        for y in 0..16 {
            for x in 0..16 {
                let idx = (y * 16 + x) * 4;
                rgba[idx] = 41;     // R
                rgba[idx + 1] = 121; // G
                rgba[idx + 2] = 255; // B
                rgba[idx + 3] = 255; // A
            }
        }
        let icon = Icon::from_rgba(rgba, 16, 16)?;

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

    pub fn handle_menu_event(&self, event_id: &str) -> Option<TrayMenuAction> {
        if event_id == self.show_item_id {
            Some(TrayMenuAction::Show)
        } else if event_id == self.quit_item_id {
            Some(TrayMenuAction::Quit)
        } else {
            None
        }
    }
}

slint::slint! {
    import { Palette } from "std-widgets.slint";

    export component NativeTray inherits SystemTrayIcon {
        in property <image> light-icon;
        in property <image> dark-icon;
        in property <string> open-label;
        in property <string> start-label;
        in property <string> stop-label;
        in property <string> settings-label;
        in property <string> quit-label;

        callback open();
        callback start();
        callback stop();
        callback settings();
        callback quit();

        // The tray has its own native appearance, independent of the window's
        // selected theme. Slint observes menu-bar appearance changes on macOS.
        icon: Palette.color-scheme == ColorScheme.light ? root.dark-icon : root.light-icon;
        tooltip: "Zapret UI";
        clicked => { root.open(); }

        Menu {
            MenuItem { title: root.open-label; activated => { root.open(); } }
            MenuSeparator {}
            MenuItem { title: root.start-label; activated => { root.start(); } }
            MenuItem { title: root.stop-label; activated => { root.stop(); } }
            MenuSeparator {}
            MenuItem { title: root.settings-label; activated => { root.settings(); } }
            MenuSeparator {}
            MenuItem { title: root.quit-label; activated => { root.quit(); } }
        }
    }
}

/// The native tray's lifetime and callbacks belong to the Slint UI thread.
/// Keeping menu dispatch inside Slint also preserves TextEdit's Copy/Paste
/// actions, which share the native-menu dispatcher on Windows and macOS.
pub struct SystemTray {
    native: NativeTray,
}

impl SystemTray {
    /// Build the tray from the saved UI language. On macOS a click opens the
    /// native menu; on Windows a left-click opens the window and a right-click
    /// opens the menu. No polling or extra window is needed.
    pub fn new(lang: &str) -> anyhow::Result<Self> {
        use crate::i18n::tr;

        let native = NativeTray::new()?;
        native.set_open_label(tr(lang, "tray.open").into());
        native.set_start_label(tr(lang, "tray.start").into());
        native.set_stop_label(tr(lang, "tray.stop").into());
        native.set_settings_label(tr(lang, "tray.settings").into());
        native.set_quit_label(tr(lang, "tray.quit").into());

        let mut pixels = image::load_from_memory_with_format(
            include_bytes!("../assets/icon-tray.png"),
            image::ImageFormat::Png,
        )?
        .into_rgba8();
        let (width, height) = pixels.dimensions();
        let light = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
            pixels.as_raw(),
            width,
            height,
        );
        for pixel in pixels.pixels_mut() {
            pixel.0[..3].fill(0);
        }
        let dark = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
            pixels.as_raw(),
            width,
            height,
        );
        native.set_light_icon(slint::Image::from_rgba8(light));
        native.set_dark_icon(slint::Image::from_rgba8(dark));

        Ok(Self { native })
    }

    pub fn on_open(&self, callback: impl Fn() + 'static) {
        self.native.on_open(callback);
    }

    pub fn on_start(&self, callback: impl Fn() + 'static) {
        self.native.on_start(callback);
    }

    pub fn on_stop(&self, callback: impl Fn() + 'static) {
        self.native.on_stop(callback);
    }

    pub fn on_settings(&self, callback: impl Fn() + 'static) {
        self.native.on_settings(callback);
    }

    pub fn on_quit(&self, callback: impl Fn() + 'static) {
        self.native.on_quit(callback);
    }
}

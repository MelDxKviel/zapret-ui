//! Advisory kernel lock: automatically released even if the app crashes.
pub struct SingleInstance {
    _file: std::fs::File,
}
impl SingleInstance {
    pub fn new(_name: &str) -> Result<Self, &'static str> {
        let home = directories::BaseDirs::new().ok_or("Cannot resolve home directory")?;
        let dir = home.config_dir().join("zapret-ui");
        std::fs::create_dir_all(&dir).map_err(|_| "Cannot create app directory")?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.join("instance.lock"))
            .map_err(|_| "Cannot open instance lock")?;
        file.try_lock().map_err(|_| "Already running")?;
        Ok(Self { _file: file })
    }
}
pub fn focus_existing_window(_title: &str) {
    // LaunchServices activates the already running bundled app.
    let _ = std::process::Command::new("/usr/bin/osascript")
        .args([
            "-e",
            "tell application id \"io.github.zapret-ui\" to activate",
        ])
        .spawn();
}

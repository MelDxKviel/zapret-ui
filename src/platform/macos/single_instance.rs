//! Advisory kernel lock plus event-driven activation of the existing GUI.
use std::{
    os::unix::{fs::PermissionsExt, net::UnixDatagram},
    path::{Path, PathBuf},
    sync::Mutex,
};

// Bind as soon as the lock is acquired: the kernel queues a second launch even
// while Slint is still starting. The UI installs its handler once it is ready.
static ACTIVATION_SOCKET: Mutex<Option<UnixDatagram>> = Mutex::new(None);
const ACTIVATE: &[u8] = b"activate";

pub struct SingleInstance {
    _file: std::fs::File,
    socket_path: PathBuf,
}

fn app_dir() -> Result<PathBuf, &'static str> {
    let home = directories::BaseDirs::new().ok_or("Cannot resolve home directory")?;
    Ok(home.config_dir().join("zapret-ui"))
}

impl SingleInstance {
    pub fn new(_name: &str) -> Result<Self, &'static str> {
        let (instance, socket) = Self::acquire(&app_dir()?)?;
        *ACTIVATION_SOCKET
            .lock()
            .map_err(|_| "Cannot initialize activation listener")? = Some(socket);
        Ok(instance)
    }

    fn acquire(dir: &Path) -> Result<(Self, UnixDatagram), &'static str> {
        std::fs::create_dir_all(dir).map_err(|_| "Cannot create app directory")?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.join("instance.lock"))
            .map_err(|_| "Cannot open instance lock")?;
        file.try_lock().map_err(|_| "Already running")?;

        let instance = Self {
            _file: file,
            socket_path: dir.join("instance.sock"),
        };
        // A crash releases the file lock but can leave a socket behind. Only
        // the lock owner may unlink it; a second instance must never do so.
        if let Err(err) = std::fs::remove_file(&instance.socket_path) {
            if err.kind() != std::io::ErrorKind::NotFound {
                return Err("Cannot remove stale activation socket");
            }
        }
        let socket = UnixDatagram::bind(&instance.socket_path)
            .map_err(|_| "Cannot bind activation socket")?;
        std::fs::set_permissions(
            &instance.socket_path,
            std::fs::Permissions::from_mode(0o600),
        )
        .map_err(|_| "Cannot protect activation socket")?;
        Ok((instance, socket))
    }
}

impl Drop for SingleInstance {
    fn drop(&mut self) {
        // The lock stays held until after this cleanup, so a newly launched
        // instance cannot bind a socket which we would then accidentally remove.
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

/// Register once after creating the UI. The callback runs off the UI thread;
/// use `slint::invoke_from_event_loop` to show and focus the window.
pub fn set_activation_handler(handler: impl Fn() + Send + Sync + 'static) {
    let socket = ACTIVATION_SOCKET
        .lock()
        .ok()
        .and_then(|mut slot| slot.take());
    let Some(socket) = socket else {
        tracing::warn!("Single-instance activation listener is unavailable");
        return;
    };
    let result = std::thread::Builder::new()
        .name("app-activation".into())
        .spawn(move || {
            let mut message = [0u8; 64];
            loop {
                match socket.recv(&mut message) {
                    Ok(size) if &message[..size] == ACTIVATE => handler(),
                    Ok(_) => {}
                    Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(err) => {
                        tracing::warn!("Single-instance activation listener stopped: {err}");
                        break;
                    }
                }
            }
        });
    if let Err(err) = result {
        tracing::warn!("Cannot start activation listener: {err}");
    }
}

pub fn focus_existing_window(_title: &str) {
    let result = (|| -> std::io::Result<()> {
        let dir = app_dir().map_err(std::io::Error::other)?;
        let socket = UnixDatagram::unbound()?;
        socket.set_write_timeout(Some(std::time::Duration::from_millis(250)))?;
        let path = dir.join("instance.sock");
        // Cover the brief interval between the first launch locking the file
        // and binding its socket. This retry only runs during a second launch.
        for attempt in 0..10 {
            match socket.send_to(ACTIVATE, &path) {
                Ok(_) => return Ok(()),
                Err(err)
                    if attempt < 9
                        && matches!(
                            err.kind(),
                            std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                        ) =>
                {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                Err(err) => return Err(err),
            }
        }
        unreachable!()
    })();
    if let Err(err) = result {
        eprintln!("Cannot activate the existing Zapret UI window: {err}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activation_is_queued_until_ui_is_ready_and_lock_is_exclusive() {
        let temp = tempfile::tempdir().unwrap();
        let (instance, receiver) = SingleInstance::acquire(temp.path()).unwrap();
        assert!(SingleInstance::acquire(temp.path()).is_err());
        // A failed lock attempt must not unlink the live activation socket.
        UnixDatagram::unbound()
            .unwrap()
            .send_to(ACTIVATE, &instance.socket_path)
            .unwrap();
        receiver
            .set_read_timeout(Some(std::time::Duration::from_secs(1)))
            .unwrap();
        let mut message = [0u8; 64];
        let size = receiver.recv(&mut message).unwrap();
        assert_eq!(&message[..size], ACTIVATE);
        assert_eq!(
            std::fs::metadata(&instance.socket_path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        drop(instance);
        assert!(!temp.path().join("instance.sock").exists());
        assert!(SingleInstance::acquire(temp.path()).is_ok());
    }

    #[test]
    fn next_launch_recovers_a_socket_left_after_crash() {
        let temp = tempfile::tempdir().unwrap();
        let socket_path = temp.path().join("instance.sock");
        let stale = UnixDatagram::bind(&socket_path).unwrap();
        drop(stale);
        assert!(socket_path.exists());
        let (_instance, _socket) = SingleInstance::acquire(temp.path()).unwrap();
    }
}

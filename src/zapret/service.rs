use std::path::PathBuf;
use std::ffi::OsString;
use crate::contracts::{Strategy, RunningMode};
use crate::ports::ServiceCtl;
use crate::zapret::elevation::check_elevation;
use windows_service::{
    service::{
        ServiceAccess, ServiceState, ServiceType, ServiceStartType, ServiceErrorControl, ServiceInfo,
    },
    service_manager::{ServiceManager, ServiceManagerAccess},
};

pub struct WindowsServiceCtl {
    install_dir: PathBuf,
    service_name: String,
}

impl WindowsServiceCtl {
    pub fn new(install_dir: PathBuf) -> Self {
        Self {
            install_dir,
            service_name: "zapret".to_string(),
        }
    }

    pub fn with_service_name(mut self, name: String) -> Self {
        self.service_name = name;
        self
    }

    fn get_winws_path(&self) -> PathBuf {
        let bin_path = self.install_dir.join("bin").join("winws.exe");
        if bin_path.exists() {
            bin_path
        } else {
            self.install_dir.join("winws.exe")
        }
    }
}

#[async_trait::async_trait]
impl ServiceCtl for WindowsServiceCtl {
    async fn install(&self, strategy: &Strategy) -> anyhow::Result<()> {
        check_elevation()?;

        let winws_path = self.get_winws_path();
        if !winws_path.exists() {
            return Err(anyhow::anyhow!("winws.exe not found at {:?}", winws_path));
        }

        let manager = ServiceManager::local_computer(
            None::<&str>,
            ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
        )?;

        // If a "zapret" service already exists, stop and delete it first — otherwise
        // create_service fails with ERROR_SERVICE_EXISTS ("IO error in winapi call").
        if let Ok(existing) = manager.open_service(
            &self.service_name,
            ServiceAccess::STOP | ServiceAccess::DELETE | ServiceAccess::QUERY_STATUS,
        ) {
            if let Ok(status) = existing.query_status() {
                if status.current_state != ServiceState::Stopped {
                    let _ = existing.stop();
                    // Give the SCM a moment to register the stop.
                    tokio::time::sleep(std::time::Duration::from_millis(800)).await;
                }
            }
            let _ = existing.delete();
            // Deletion is finalized once all handles close; wait briefly so the name frees up.
            drop(existing);
            tokio::time::sleep(std::time::Duration::from_millis(800)).await;
        }

        // Prepare the launch arguments.
        let launch_arguments: Vec<OsString> = strategy.winws_args.iter().map(|s| OsString::from(s.as_str())).collect();

        // Ensure user list files exist so the service's winws.exe can start.
        crate::zapret::batparse::ensure_user_lists(&self.install_dir);

        // Create the ServiceInfo structure.
        let service_info = ServiceInfo {
            name: OsString::from(&self.service_name),
            display_name: OsString::from(&self.service_name),
            service_type: ServiceType::OWN_PROCESS,
            start_type: ServiceStartType::AutoStart,
            error_control: ServiceErrorControl::Normal,
            executable_path: winws_path,
            launch_arguments,
            dependencies: Vec::new(),
            account_name: None,
            account_password: None,
        };

        let _service = manager.create_service(&service_info, ServiceAccess::ALL_ACCESS)?;

        Ok(())
    }

    async fn remove(&self) -> anyhow::Result<()> {
        check_elevation()?;

        let manager = ServiceManager::local_computer(
            None::<&str>,
            ServiceManagerAccess::CONNECT,
        )?;
        let service = manager.open_service(
            &self.service_name,
            ServiceAccess::ALL_ACCESS,
        )?;

        service.delete()?;
        Ok(())
    }

    async fn start(&self) -> anyhow::Result<()> {
        check_elevation()?;

        let manager = ServiceManager::local_computer(
            None::<&str>,
            ServiceManagerAccess::CONNECT,
        )?;
        let service = manager.open_service(
            &self.service_name,
            ServiceAccess::START,
        )?;

        service.start(&[] as &[&str])?;
        Ok(())
    }

    async fn stop(&self) -> anyhow::Result<()> {
        check_elevation()?;

        let manager = ServiceManager::local_computer(
            None::<&str>,
            ServiceManagerAccess::CONNECT,
        )?;
        let service = manager.open_service(
            &self.service_name,
            ServiceAccess::STOP,
        )?;

        service.stop()?;
        Ok(())
    }

    async fn status(&self) -> anyhow::Result<RunningMode> {
        let manager = ServiceManager::local_computer(
            None::<&str>,
            ServiceManagerAccess::CONNECT,
        )?;

        let service_res = manager.open_service(
            &self.service_name,
            ServiceAccess::QUERY_STATUS,
        );

        match service_res {
            Ok(service) => {
                let status = service.query_status()?;
                if status.current_state == ServiceState::Running {
                    Ok(RunningMode::WindowsService)
                } else {
                    Ok(RunningMode::None)
                }
            }
            Err(_) => {
                // If service is not registered/found, return None
                Ok(RunningMode::None)
            }
        }
    }

    async fn is_installed(&self) -> bool {
        let manager = match ServiceManager::local_computer(
            None::<&str>,
            ServiceManagerAccess::CONNECT,
        ) {
            Ok(m) => m,
            Err(_) => return false,
        };
        // Opening with QUERY_STATUS succeeds iff the service is registered.
        manager
            .open_service(&self.service_name, ServiceAccess::QUERY_STATUS)
            .is_ok()
    }
}

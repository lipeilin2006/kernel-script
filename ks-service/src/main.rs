mod driver_comm;
mod ipc;
mod process;

use std::ffi::OsString;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{broadcast, Mutex};
use windows_service::define_windows_service;
use windows_service::service::{
    ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{
    self, ServiceControlHandlerResult, ServiceStatusHandle,
};
use windows_service::service_dispatcher;

use crate::driver_comm::DriverComm;

const SERVICE_NAME: &str = "KsService";

define_windows_service!(ffi_service_main, service_main);

fn main() {
    init_logging();
    if std::env::args().any(|arg| arg == "--console") {
        tracing::info!("service console mode starting");
        run_console();
    } else if let Err(error) = service_dispatcher::start(SERVICE_NAME, ffi_service_main) {
        tracing::error!(%error, "service dispatcher failed; use --console for interactive mode");
    }
}

fn service_main(_arguments: Vec<OsString>) {
    if let Err(error) = run_windows_service() {
        tracing::error!(%error, "service failed");
    }
}

fn run_windows_service() -> Result<(), windows_service::Error> {
    let (stop_tx, stop_rx) = broadcast::channel(2);
    let status_handle: ServiceStatusHandle =
        service_control_handler::register(SERVICE_NAME, move |control| match control {
            ServiceControl::Stop | ServiceControl::Shutdown => {
                let _ = stop_tx.send(());
                ServiceControlHandlerResult::NoError
            }
            _ => ServiceControlHandlerResult::NotImplemented,
        })?;
    status_handle.set_service_status(status(
        ServiceState::StartPending,
        ServiceControlAccept::empty(),
    ))?;
    status_handle.set_service_status(status(
        ServiceState::Running,
        ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
    ))?;
    run_runtime(stop_rx);
    status_handle
        .set_service_status(status(ServiceState::Stopped, ServiceControlAccept::empty()))?;
    Ok(())
}

fn status(state: ServiceState, accepts: ServiceControlAccept) -> ServiceStatus {
    ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: state,
        controls_accepted: accepts,
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::from_secs(5),
        process_id: None,
    }
}

fn run_console() {
    tracing::info!("creating Tokio runtime");
    let (stop_tx, stop_rx) = broadcast::channel(2);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to create Tokio runtime");
    runtime.block_on(async move {
        let tx = stop_tx.clone();
        tokio::spawn(async move {
            let _ = tokio::signal::ctrl_c().await;
            let _ = tx.send(());
        });
        run_runtime_async(stop_rx).await;
    });
}

fn run_runtime(stop_rx: broadcast::Receiver<()>) {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(run_runtime_async(stop_rx));
}

async fn run_runtime_async(stop_rx: broadcast::Receiver<()>) {
    tracing::info!("connecting to driver");
    let driver = Arc::new(Mutex::new(DriverComm::new()));
    {
        let mut driver = driver.lock().await;
        match driver.connect() {
            Ok(()) => tracing::info!("driver connected"),
            Err(error) => tracing::error!(%error, "driver connection failed"),
        }
    }
    let ipc = tokio::spawn(ipc::start_server(Arc::clone(&driver), stop_rx));
    if let Err(error) = ipc.await {
        tracing::error!(%error, "IPC server task failed");
    }
    driver.lock().await.disconnect();
}

fn init_logging() {
    let _ = tracing_subscriber::fmt()
        .with_target(false)
        .with_thread_ids(true)
        .try_init();
}

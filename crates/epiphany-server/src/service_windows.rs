//! Native Windows service (SCM) hosting (docs/DEPLOYMENT.md). Windows only.
//!
//! `epiphany-server service install` registers the service (pointing the SCM at
//! `service run`), `service uninstall` removes it, and `service run` is the entry
//! the SCM invokes. When hosted, a `SERVICE_CONTROL_STOP` (or system shutdown)
//! from the SCM drives the same graceful drain as Ctrl-C / SIGTERM in the
//! foreground, by signalling a `Notify` the server awaits as its shutdown future.

use std::ffi::OsString;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Notify;
use windows_service::service::{
    ServiceAccess, ServiceControl, ServiceControlAccept, ServiceErrorControl, ServiceExitCode,
    ServiceInfo, ServiceStartType, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::service_dispatcher;
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

const SERVICE_NAME: &str = "Epiphany";
const SERVICE_DISPLAY: &str = "Epiphany OLAP server";

/// Dispatch the `service` subcommand.
pub(crate) fn handle(sub: &str) -> Result<(), Box<dyn std::error::Error>> {
    match sub {
        "run" => {
            // Hand the thread to the SCM dispatcher; it calls `service_main`.
            service_dispatcher::start(SERVICE_NAME, ffi_service_main)?;
            Ok(())
        }
        "install" => install(),
        "uninstall" => uninstall(),
        other => {
            eprintln!("usage: epiphany-server service [install|uninstall|run] (got '{other}')");
            std::process::exit(2);
        }
    }
}

windows_service::define_windows_service!(ffi_service_main, service_main);

fn service_main(_args: Vec<OsString>) {
    if let Err(e) = run_service() {
        // The SCM only sees the exit; best-effort note to any attached console.
        eprintln!("epiphany service error: {e}");
    }
}

fn status(
    state: ServiceState,
    accept: ServiceControlAccept,
    exit_code: ServiceExitCode,
) -> ServiceStatus {
    ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: state,
        controls_accepted: accept,
        exit_code,
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    }
}

fn run_service() -> Result<(), Box<dyn std::error::Error>> {
    let config = crate::config::Config::from_env();
    crate::observability::init(&config.log_filter);

    // The SCM control handler runs on its own thread; a Stop stores a permit on
    // the Notify (notify_one is permit-based, so it is never lost to a race), and
    // the server's shutdown future consumes it.
    let notify = Arc::new(Notify::new());
    let on_stop = notify.clone();
    let status_handle =
        service_control_handler::register(SERVICE_NAME, move |control| match control {
            ServiceControl::Stop | ServiceControl::Shutdown => {
                on_stop.notify_one();
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        })?;

    status_handle.set_service_status(status(
        ServiceState::Running,
        ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
        ServiceExitCode::Win32(0),
    ))?;

    let runtime = tokio::runtime::Runtime::new()?;
    let result = runtime.block_on(crate::run_server(config, async move {
        notify.notified().await;
    }));

    // Report a non-zero exit to the SCM when startup or the run loop failed, so a
    // failed service is not mistaken for a clean stop (the SCM can then restart or
    // alert per its recovery policy).
    let exit_code = match &result {
        Ok(()) => ServiceExitCode::Win32(0),
        Err(_) => ServiceExitCode::ServiceSpecific(1),
    };
    status_handle.set_service_status(status(
        ServiceState::Stopped,
        ServiceControlAccept::empty(),
        exit_code,
    ))?;
    result
}

fn install() -> Result<(), Box<dyn std::error::Error>> {
    let manager =
        ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CREATE_SERVICE)?;
    let info = ServiceInfo {
        name: OsString::from(SERVICE_NAME),
        display_name: OsString::from(SERVICE_DISPLAY),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: std::env::current_exe()?,
        launch_arguments: vec![OsString::from("service"), OsString::from("run")],
        dependencies: vec![],
        account_name: None, // LocalSystem
        account_password: None,
    };
    manager.create_service(&info, ServiceAccess::QUERY_STATUS)?;
    println!(
        "Installed service '{SERVICE_NAME}'. Set EPIPHANY_* in the service's environment, then \
         start it with: sc start {SERVICE_NAME}"
    );
    Ok(())
}

fn uninstall() -> Result<(), Box<dyn std::error::Error>> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    let service = manager.open_service(SERVICE_NAME, ServiceAccess::DELETE)?;
    service.delete()?;
    println!("Deleted service '{SERVICE_NAME}'.");
    Ok(())
}

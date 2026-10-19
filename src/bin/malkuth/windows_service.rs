//! Windows service host and `malkuth service …` management commands.
//!
//! `malkuth daemon --config <path> --service --service-name <name>` runs the
//! same daemon loop as the systemd unit, but under the Service Control Manager:
//! SCM Start reports `StartPending` → `Running`, SCM Stop/Shutdown feeds the
//! daemon's exit watch channel so the workers drain before the process exits,
//! and the final state is always `Stopped`.
//!
//! The management surface mirrors the systemd operations one-for-one:
//!
//! ```text
//! malkuth service install --config <path> [--name malkuth] [--no-start]
//! malkuth service uninstall [--name malkuth]
//! malkuth service start|stop|restart|status [--name malkuth]
//! ```

use std::ffi::{OsStr, OsString};
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;

use windows_service::service::{
    ServiceAccess, ServiceControl, ServiceControlAccept, ServiceErrorControl, ServiceExitCode,
    ServiceInfo, ServiceStartType, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
use windows_service::{define_windows_service, service_dispatcher};

const DEFAULT_SERVICE_NAME: &str = "malkuth";
const DEFAULT_DISPLAY_NAME: &str = "malkuth service supervisor";

static SERVICE_NAME: OnceLock<String> = OnceLock::new();
static CONFIG_PATH: OnceLock<String> = OnceLock::new();

define_windows_service!(ffi_service_main, service_main);

/// Entry point for `malkuth daemon … --service`: blocks in the SCM dispatcher
/// until the service is stopped.
pub fn run_service(name: &str, config_path: &str) -> windows_service::Result<()> {
    let _ = SERVICE_NAME.set(name.to_owned());
    let _ = CONFIG_PATH.set(config_path.to_owned());
    service_dispatcher::start(name, ffi_service_main)
}

fn service_main(_arguments: Vec<OsString>) {
    let name = SERVICE_NAME
        .get()
        .cloned()
        .or_else(|| arg_value("--service-name"))
        .unwrap_or_else(|| DEFAULT_SERVICE_NAME.to_owned());
    let config_path = CONFIG_PATH
        .get()
        .cloned()
        .or_else(|| arg_value("--config"))
        .unwrap_or_default();

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let status_handle = match service_control_handler::register(&name, move |control| match control
    {
        ServiceControl::Stop | ServiceControl::Shutdown => {
            let _ = shutdown_tx.send(true);
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        _ => ServiceControlHandlerResult::NotImplemented,
    }) {
        Ok(handle) => handle,
        Err(e) => {
            eprintln!("malkuth service: cannot register control handler: {e}");
            return;
        }
    };

    if let Err(e) = set_status(
        &status_handle,
        ServiceState::StartPending,
        ServiceControlAccept::empty(),
    ) {
        eprintln!("malkuth service: cannot report StartPending: {e}");
        return;
    }

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            let _ = set_status(
                &status_handle,
                ServiceState::Stopped,
                ServiceControlAccept::empty(),
            );
            eprintln!("malkuth service: cannot build tokio runtime: {e}");
            return;
        }
    };

    if let Err(e) = set_status(
        &status_handle,
        ServiceState::Running,
        ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
    ) {
        eprintln!("malkuth service: cannot report Running: {e}");
        return;
    }

    let result = runtime.block_on(crate::run_daemon_with_shutdown(
        &config_path,
        Some(shutdown_rx),
    ));
    if let Err(e) = result {
        eprintln!("malkuth daemon exited with error: {e}");
    }
    let _ = set_status(
        &status_handle,
        ServiceState::Stopped,
        ServiceControlAccept::empty(),
    );
}

fn set_status(
    handle: &service_control_handler::ServiceStatusHandle,
    state: ServiceState,
    accepted: ServiceControlAccept,
) -> windows_service::Result<()> {
    handle.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: state,
        controls_accepted: accepted,
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::from_secs(10),
        process_id: None,
    })
}

fn arg_value(flag: &str) -> Option<String> {
    let mut args = std::env::args_os().skip(1);
    while let Some(arg) = args.next() {
        if arg.to_string_lossy() == flag {
            return args.next().and_then(|v| v.into_string().ok());
        }
    }
    None
}

// ── `malkuth service …` ────────────────────────────────────────────────────

/// Dispatch `malkuth service <subcommand> …`.
pub fn service_command(args: &[String]) -> Result<(), String> {
    let Some(subcommand) = args.first().map(String::as_str) else {
        return Err(usage());
    };
    let opts = Options::parse(&args[1..])?;
    match subcommand {
        "install" => install(&opts),
        "uninstall" => uninstall(&opts),
        "start" => start(&opts),
        "stop" => stop(&opts),
        "restart" => restart(&opts),
        "status" => status(&opts),
        other => Err(format!("unknown service subcommand {other:?}\n{}", usage())),
    }
}

fn usage() -> String {
    "usage: malkuth service <install|uninstall|start|stop|restart|status> \
     [--config <path>] [--name <name>] [--display-name <text>] [--no-start]"
        .to_owned()
}

#[derive(Default)]
struct Options {
    config: Option<PathBuf>,
    name: String,
    display_name: Option<String>,
    no_start: bool,
}

impl Options {
    fn parse(args: &[String]) -> Result<Self, String> {
        let mut opts = Options {
            name: DEFAULT_SERVICE_NAME.to_owned(),
            ..Options::default()
        };
        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "--config" | "-c" => {
                    i += 1;
                    opts.config = Some(PathBuf::from(args.get(i).ok_or("--config needs a path")?));
                }
                "--name" => {
                    i += 1;
                    opts.name = args.get(i).ok_or("--name needs a value")?.clone();
                }
                "--display-name" => {
                    i += 1;
                    opts.display_name =
                        Some(args.get(i).ok_or("--display-name needs a value")?.clone());
                }
                "--no-start" => opts.no_start = true,
                other => return Err(format!("unknown option {other:?}\n{}", usage())),
            }
            i += 1;
        }
        Ok(opts)
    }
}

fn manager(create: bool) -> Result<ServiceManager, String> {
    let mut access = ServiceManagerAccess::CONNECT;
    if create {
        access |= ServiceManagerAccess::CREATE_SERVICE;
    }
    ServiceManager::local_computer(None::<&str>, access)
        .map_err(|e| format!("cannot open SCM: {e}"))
}

fn install(opts: &Options) -> Result<(), String> {
    let config = opts
        .config
        .as_ref()
        .ok_or_else(|| "service install needs --config <path>".to_owned())?;
    let executable =
        std::env::current_exe().map_err(|e| format!("cannot resolve current exe: {e}"))?;
    let manager = manager(true)?;
    if manager
        .open_service(&opts.name, ServiceAccess::QUERY_STATUS)
        .is_ok()
    {
        return Err(format!(
            "service {:?} already exists; run `malkuth service uninstall` first",
            opts.name
        ));
    }

    let info = ServiceInfo {
        name: OsString::from(&opts.name),
        display_name: OsString::from(
            opts.display_name
                .clone()
                .unwrap_or_else(|| DEFAULT_DISPLAY_NAME.to_owned()),
        ),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: executable,
        launch_arguments: vec![
            OsString::from("daemon"),
            OsString::from("--config"),
            config.as_os_str().to_owned(),
            OsString::from("--service"),
            OsString::from("--service-name"),
            OsString::from(&opts.name),
        ],
        dependencies: Vec::new(),
        account_name: None,
        account_password: None,
    };
    let service = manager
        .create_service(
            &info,
            ServiceAccess::QUERY_STATUS
                | ServiceAccess::START
                | ServiceAccess::STOP
                | ServiceAccess::DELETE,
        )
        .map_err(|e| format!("cannot create service: {e}"))?;
    println!(
        "installed service {:?} (config {})",
        opts.name,
        config.display()
    );
    if !opts.no_start {
        service
            .start(&[] as &[&OsStr])
            .map_err(|e| format!("service created but start failed: {e}"))?;
        println!("started service {:?}", opts.name);
    }
    Ok(())
}

fn uninstall(opts: &Options) -> Result<(), String> {
    let manager = manager(false)?;
    let service = manager
        .open_service(
            &opts.name,
            ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE,
        )
        .map_err(|e| format!("cannot open service {:?}: {e}", opts.name))?;
    stop_if_running(&service, &opts.name)?;
    service
        .delete()
        .map_err(|e| format!("cannot delete service {:?}: {e}", opts.name))?;
    println!("uninstalled service {:?}", opts.name);
    Ok(())
}

fn start(opts: &Options) -> Result<(), String> {
    let manager = manager(false)?;
    let service = manager
        .open_service(
            &opts.name,
            ServiceAccess::START | ServiceAccess::QUERY_STATUS,
        )
        .map_err(|e| format!("cannot open service {:?}: {e}", opts.name))?;
    service
        .start(&[] as &[&OsStr])
        .map_err(|e| format!("cannot start service {:?}: {e}", opts.name))?;
    println!("started service {:?}", opts.name);
    Ok(())
}

fn stop(opts: &Options) -> Result<(), String> {
    let manager = manager(false)?;
    let service = manager
        .open_service(
            &opts.name,
            ServiceAccess::STOP | ServiceAccess::QUERY_STATUS,
        )
        .map_err(|e| format!("cannot open service {:?}: {e}", opts.name))?;
    stop_if_running(&service, &opts.name)?;
    println!("stopped service {:?}", opts.name);
    Ok(())
}

fn restart(opts: &Options) -> Result<(), String> {
    let manager = manager(false)?;
    let service = manager
        .open_service(
            &opts.name,
            ServiceAccess::START | ServiceAccess::STOP | ServiceAccess::QUERY_STATUS,
        )
        .map_err(|e| format!("cannot open service {:?}: {e}", opts.name))?;
    stop_if_running(&service, &opts.name)?;
    wait_for_state(&service, &opts.name, ServiceState::Stopped)?;
    service
        .start(&[] as &[&OsStr])
        .map_err(|e| format!("cannot start service {:?}: {e}", opts.name))?;
    println!("restarted service {:?}", opts.name);
    Ok(())
}

fn status(opts: &Options) -> Result<(), String> {
    let manager = manager(false)?;
    let service = manager
        .open_service(&opts.name, ServiceAccess::QUERY_STATUS)
        .map_err(|e| format!("cannot open service {:?}: {e}", opts.name))?;
    let status = service
        .query_status()
        .map_err(|e| format!("cannot query service {:?}: {e}", opts.name))?;
    println!(
        "{}: {:?}{}",
        opts.name,
        status.current_state,
        status
            .process_id
            .map(|pid| format!(" (pid {pid})"))
            .unwrap_or_default()
    );
    Ok(())
}

fn stop_if_running(service: &windows_service::service::Service, name: &str) -> Result<(), String> {
    let status = service
        .query_status()
        .map_err(|e| format!("cannot query service {name:?}: {e}"))?;
    if matches!(
        status.current_state,
        ServiceState::Running | ServiceState::StartPending | ServiceState::ContinuePending
    ) {
        service
            .stop()
            .map_err(|e| format!("cannot stop service {name:?}: {e}"))?;
        wait_for_state(service, name, ServiceState::Stopped)?;
    }
    Ok(())
}

fn wait_for_state(
    service: &windows_service::service::Service,
    name: &str,
    wanted: ServiceState,
) -> Result<(), String> {
    for _ in 0..60 {
        let status = service
            .query_status()
            .map_err(|e| format!("cannot query service {name:?}: {e}"))?;
        if status.current_state == wanted {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    Err(format!("service {name:?} did not reach {wanted:?}"))
}

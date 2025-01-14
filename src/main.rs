//#![cfg(unix)]

use crate::config::CliConfig;
use color_eyre::{eyre::Context, Help, Report, SectionExt};
#[cfg(unix)]
use daemonize::Daemonize;
use log::{error, info, trace, LevelFilter};
use std::panic;
use structopt::StructOpt;
use tokio_core::reactor::Core;

#[cfg(feature = "alsa_backend")]
mod alsa_mixer;
mod config;
#[cfg(feature = "dbus_mpris")]
mod dbus_mpris;
mod error;
mod main_loop;
mod process;
mod setup;
mod utils;

enum LogTarget {
    Terminal,
    Syslog,
}

fn setup_logger(log_target: LogTarget, log_level: LevelFilter) {
    let logger = fern::Dispatch::new().level(log_level);

    let logger = match log_target {
        LogTarget::Terminal => logger.chain(std::io::stdout()),
        #[cfg(unix)]
        LogTarget::Syslog => {
            let log_format = syslog::Formatter3164 {
                facility: syslog::Facility::LOG_DAEMON,
                hostname: None,
                process: "spotifyd".to_owned(),
                pid: 0,
            };
            logger.chain(syslog::unix(log_format).expect("Couldn't initialize logger"))
        }
        #[cfg(target_os = "windows")]
        LogTarget::Syslog => {
            let dirs = directories::BaseDirs::new().unwrap();
            let mut log_file = dirs.config_dir().to_path_buf();
            log_file.push("spotifyd");
            std::fs::create_dir_all(&log_file).expect("Couldn't create log dir.");
            log_file.push(".spotifyd.log");

            logger.chain(
                std::fs::OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .open(log_file)
                    .expect("Couldn't initialize logger"),
            )
        }
    };

    logger.apply().expect("Couldn't initialize logger");
}

fn main() -> Result<(), Report> {
    let mut cli_config: CliConfig = CliConfig::from_args();

    let is_daemon = !cli_config.no_daemon;

    let log_target = if is_daemon {
        LogTarget::Syslog
    } else {
        LogTarget::Terminal
    };
    let log_level = if cli_config.verbose {
        LevelFilter::Trace
    } else {
        LevelFilter::Info
    };

    setup_logger(log_target, log_level);
    color_eyre::install().expect("Coundn't initialize error reporting");

    cli_config
        .load_config_file_values()
        .wrap_err("could not load the config file")
        .with_section(|| {
            concat!(
                "the config format should be valid TOML\n",
                "we recently changed the config format, see https://github.com/Spotifyd/spotifyd/issues/765"
            )
            .header("note:")
        })?;
    trace!("{:?}", &cli_config);

    // Returns the old SpotifydConfig struct used within the rest of the daemon.
    let internal_config = config::get_internal_config(cli_config);

    if is_daemon {
        #[cfg(unix)]
        {
            info!("Daemonizing running instance");

            let mut daemonize = Daemonize::new();
            if let Some(pid) = internal_config.pid.as_ref() {
                daemonize = daemonize.pid_file(pid);
            }
            match daemonize.start() {
                Ok(_) => info!("Detached from shell, now running in background."),
                Err(e) => error!("Something went wrong while daemonizing: {}", e),
            };
        }

        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            use std::process::{exit, Command};

            let mut args = std::env::args().collect::<Vec<_>>();
            args.remove(0);
            args.push("--no-daemon".to_string());

            Command::new(std::env::current_exe().unwrap())
                .args(args)
                .env("SPOTIFYD_CHILD", "1")
                .creation_flags(8 /* DETACHED_PROCESS */)
                .spawn()
                .expect("Couldn't spawn daemon");

            exit(0);
        }
    }

    panic::set_hook(Box::new(|panic_info| {
        error!(
            "PANIC: Shutting down spotifyd. Error message: {}",
            match (
                panic_info.payload().downcast_ref::<String>(),
                panic_info.payload().downcast_ref::<&str>(),
            ) {
                (Some(s), _) => &**s,
                (_, Some(&s)) => s,
                _ => "Unknown error type, can't produce message.",
            }
        );
    }));

    let mut core = Core::new().unwrap();
    let handle = core.handle();

    let initial_state = setup::initial_state(handle, internal_config);
    core.run(initial_state).unwrap();

    Ok(())
}

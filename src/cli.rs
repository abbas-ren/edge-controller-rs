use anyhow::{anyhow, Result};
use clap::{error::ErrorKind, ArgAction, CommandFactory, Parser, ValueEnum};
use std::ffi::OsString;

use crate::constants::{DEFAULT_BIND_PORT, DEFAULT_CONFIG_PATH, DEFAULT_METRICS_PORT};

const ENVIRONMENT_HELP: &str = "Environment overrides:
    DEV_CONTROLLER_LOG_LEVEL     Set logging level if --log-level is not used
    DEV_CONTROLLER_LOG_FILE      Set an alternate log file without --log-file
    DEV_CONTROLLER_LOG_NETWORK   Enable network logging with 1/true/yes/on
    DEV_CONTROLLER_LOG_STREAM    Enable stream logging with 1/true/yes/on
    DEV_CONTROLLER_BIND          Set the controller API listen address
    DEV_CONTROLLER_INTERFACE     Override the configured network interface
    DEV_CONTROLLER_TOKEN         Set the API bearer token";

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl LogLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Trace => "trace",
            Self::Debug => "debug",
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
        }
    }
}

/// Feature toggles for optional generations and RTOS support.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FeatureFlags {
    /// Enable Gen3 relay and FlashWriter routes.
    pub gen3: bool,
    /// Enable Gen4 relay and FlashWriter routes.
    pub gen4: bool,
    /// Enable Gen5 mapping, power, and firmware routes.
    pub gen5: bool,
    /// Enable RTOS serial-capture routes.
    pub rtos: bool,
}

impl FeatureFlags {
    /// Return whether routes for the numeric hardware generation are enabled.
    pub fn generation_enabled(&self, generation: u8) -> bool {
        match generation {
            3 => self.gen3,
            4 => self.gen4,
            5 => self.gen5,
            _ => false,
        }
    }
}

#[derive(Debug, Parser)]
#[command(
    name = "edgecontroller",
    about = "Edge controller service",
    long_about = None,
    after_long_help = ENVIRONMENT_HELP
)]
struct CliArgs {
    #[arg(long, short = 'c', default_value = DEFAULT_CONFIG_PATH, help = "Path to the legacy login config")]
    config_path: String,

    #[arg(long, short = 'C', action = ArgAction::SetTrue, help = "Validate configuration and state without opening hardware")]
    check: bool,

    #[arg(long, short = 'l', value_enum, help = "Trace log level")]
    log_level: Option<LogLevel>,

    #[arg(long, help = "Write logs to a file in addition to stderr")]
    log_file: Option<String>,

    #[arg(long, action = ArgAction::SetTrue, help = "Enable network logging details")]
    log_network: bool,

    #[arg(long, action = ArgAction::SetTrue, help = "Enable stream logging details")]
    log_stream: bool,

    #[arg(long, default_value_t = DEFAULT_METRICS_PORT, value_parser = clap::value_parser!(u16).range(1..), help = "Prometheus metrics port")]
    metrics_port: u16,

    #[arg(long, default_value_t = DEFAULT_BIND_PORT, value_parser = clap::value_parser!(u16).range(1..), help = "Controller HTTP bind port")]
    bind_port: u16,

    #[arg(long, short = 'v', action = ArgAction::SetTrue, conflicts_with = "log_level", help = "Enable debug logging")]
    verbose: bool,

    #[arg(long, action = ArgAction::SetTrue, help = "Enable Gen3 relay and IPL functionality")]
    enable_gen3: bool,

    #[arg(long, action = ArgAction::SetTrue, help = "Enable Gen4 relay and IPL functionality")]
    enable_gen4: bool,

    #[arg(long, action = ArgAction::SetTrue, help = "Enable Gen5 mapping and power routes")]
    enable_gen5: bool,

    #[arg(long, action = ArgAction::SetTrue, help = "Enable RTOS capture endpoints")]
    enable_rtos: bool,

    #[arg(
        long,
        requires = "vid_pid",
        help = "Relay-board USB iSerial value; requires --vid-pid"
    )]
    relay_serial_number: Option<String>,

    #[arg(
        long,
        value_name = "VID:PID",
        help = "Required relay-board hexadecimal USB VID:PID; discovers iSerial when serial is omitted"
    )]
    vid_pid: Option<String>,
}

/// Parsed command-line mode for the controller process.
#[derive(Debug, PartialEq, Eq)]
pub enum Cli {
    /// Print generated command-line help and exit successfully.
    Help,
    /// Start the service or perform offline validation with these options.
    Start {
        /// Legacy login configuration path.
        config_path: String,
        /// Validate configuration and persisted state without opening hardware.
        check_only: bool,
        /// Effective tracing level selected explicitly on the command line.
        log_level: Option<String>,
        /// Optional additional log-file path.
        log_file: Option<String>,
        /// Enable debug details for controller-owned network modules.
        log_network: bool,
        /// Enable debug details for serial and WebSocket stream modules.
        log_stream: bool,
        /// Auxiliary Prometheus exporter port.
        metrics_port: u16,
        /// Main HTTP API listener port.
        bind_port: u16,
        /// Explicit generation and RTOS route toggles.
        features: FeatureFlags,
        /// Optional relay-board USB iSerial value.
        relay_serial_number: Option<String>,
        /// Relay-board USB VID:PID selector, required whenever a relay is selected.
        vid_pid: Option<String>,
    },
}

impl Cli {
    /// Parse arguments excluding the executable name.
    pub fn parse<I>(arguments: I) -> Result<Self>
    where
        I: IntoIterator<Item = OsString>,
    {
        let mut values = vec![OsString::from("edgecontroller")];
        values.extend(arguments);

        match CliArgs::try_parse_from(values) {
            Ok(args) => {
                let log_level = if args.verbose {
                    Some("debug".to_owned())
                } else {
                    args.log_level.map(|level| level.as_str().to_owned())
                };

                let features = FeatureFlags {
                    gen3: args.enable_gen3,
                    gen4: args.enable_gen4,
                    gen5: args.enable_gen5,
                    rtos: args.enable_rtos,
                };

                Ok(Self::Start {
                    config_path: args.config_path,
                    check_only: args.check,
                    log_level,
                    log_file: args.log_file,
                    log_network: args.log_network,
                    log_stream: args.log_stream,
                    metrics_port: args.metrics_port,
                    bind_port: args.bind_port,
                    features,
                    relay_serial_number: args.relay_serial_number,
                    vid_pid: args.vid_pid,
                })
            }
            Err(error)
                if matches!(
                    error.kind(),
                    ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
                ) =>
            {
                Ok(Self::Help)
            }
            Err(error) => Err(anyhow!(error.to_string())),
        }
    }

    /// Render the long Clap help text used by `--help`.
    pub fn help_text() -> Result<String> {
        let mut output = Vec::new();
        CliArgs::command().write_long_help(&mut output)?;
        Ok(String::from_utf8(output)?)
    }
}

#[cfg(test)]
mod tests {
    use super::{Cli, FeatureFlags};
    use std::ffi::OsString;

    fn arguments(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn no_arguments_selects_default_configuration() {
        let cli = Cli::parse(arguments(&[])).unwrap();
        assert!(
            matches!(cli, Cli::Start { config_path, .. } if config_path == crate::constants::DEFAULT_CONFIG_PATH)
        );
    }

    #[test]
    fn feature_flags_are_parsed_explicitly() {
        let cli = Cli::parse(arguments(&["--enable-gen4", "--enable-rtos"])).unwrap();
        assert!(
            matches!(cli, Cli::Start { features, .. } if features == FeatureFlags { gen3: false, gen4: true, gen5: false, rtos: true })
        );
    }

    #[test]
    fn log_level_and_auxiliary_logging_flags_are_parsed() {
        let cli = Cli::parse(arguments(&[
            "--log-level",
            "debug",
            "--log-file",
            "/tmp/edge.log",
            "--log-network",
            "--log-stream",
        ]))
        .unwrap();
        assert!(
            matches!(cli, Cli::Start { log_level, log_file, log_network, log_stream, .. } if log_level.as_deref() == Some("debug") && log_file.as_deref() == Some("/tmp/edge.log") && log_network && log_stream)
        );
    }

    #[test]
    fn multiple_generations_can_be_enabled_together() {
        let cli = Cli::parse(arguments(&["--enable-gen3", "--enable-gen5"])).unwrap();
        assert!(
            matches!(cli, Cli::Start { features, .. } if features.gen3 && features.gen5 && !features.gen4)
        );
    }

    #[test]
    fn listener_ports_accept_boundaries_and_reject_zero() {
        let cli = Cli::parse(arguments(&["--metrics-port", "1", "--bind-port", "65535"])).unwrap();
        assert!(matches!(
            cli,
            Cli::Start {
                metrics_port: 1,
                bind_port: 65535,
                ..
            }
        ));

        assert!(Cli::parse(arguments(&["--metrics-port", "0"])).is_err());
        assert!(Cli::parse(arguments(&["--bind-port", "0"])).is_err());
    }

    #[test]
    fn invalid_log_level_is_rejected() {
        assert!(Cli::parse(arguments(&["--log-level", "verbose"])).is_err());
    }

    #[test]
    fn verbose_and_explicit_log_level_are_mutually_exclusive() {
        assert!(Cli::parse(arguments(&["--verbose", "--log-level", "info"])).is_err());
    }

    #[test]
    fn generated_help_documents_options_and_environment() {
        let help = Cli::help_text().unwrap();

        for expected in [
            "--config-path",
            "--verbose",
            "--enable-gen3",
            "DEV_CONTROLLER_BIND",
            "DEV_CONTROLLER_TOKEN",
        ] {
            assert!(help.contains(expected), "help omitted {expected}");
        }
        assert!(!help.contains("payload logging"));
    }
}

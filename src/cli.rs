use anyhow::{anyhow, Result};
use clap::{
    ArgAction, Parser, ValueEnum, error::ErrorKind,
};
use std::ffi::OsString;

use crate::constants::{
    DEFAULT_BIND_PORT, DEFAULT_CONFIG_PATH, DEFAULT_METRICS_PORT,
};

/// Short help text used for the binary entrypoint and `--help` output.
pub const USAGE: &str = "\
Usage:
  dev-controller-rs [OPTIONS]

Options:
  -c, --config-path <PATH>      Path to the legacy login config
  -C, --check                   Validate config and state without opening hardware
  -l, --log-level <LEVEL>       trace|debug|info|warn|error
      --log-file <PATH>         Write logs to a file in addition to stderr
      --log-network             Include network-layer log metadata
      --log-stream              Include stream payload diagnostics
      --metrics-port <PORT>     Prometheus exporter port [default: 8081]
      --bind-port <PORT>        Controller HTTP bind port [default: 8888]
      --enable-gen3             Enable Gen3 relay functionality
      --enable-gen4             Enable Gen4 relay and IPL functionality
      --enable-gen5             Enable Gen5 mapping and power routes
      --enable-rtos             Enable RTOS capture endpoints
  -h, --help                    Display help information

Environment overrides:
  DEV_CONTROLLER_LOG_LEVEL     Set logging level if --log-level is not used
  DEV_CONTROLLER_LOG_FILE      Set an alternate log file without passing --log-file
  DEV_CONTROLLER_LOG_NETWORK   Set to 1/true/yes/on to enable network logging
  DEV_CONTROLLER_LOG_STREAM    Set to 1/true/yes/on to enable stream logging
";

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
    pub gen3: bool,
    pub gen4: bool,
    pub gen5: bool,
    pub rtos: bool,
}

impl FeatureFlags {
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
#[command(name = "edgecontroller", about = "Edge controller service", long_about = None)]
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

    #[arg(long, action = ArgAction::SetTrue, help = "Enable stream payload logging")]
    log_stream: bool,

    #[arg(long, default_value_t = DEFAULT_METRICS_PORT, help = "Prometheus metrics port")]
    metrics_port: u16,

    #[arg(long, default_value_t = DEFAULT_BIND_PORT, help = "Controller HTTP bind port")]
    bind_port: u16,

    #[arg(long, short = 'v', action = ArgAction::SetTrue, help = "Enable debug logging")]
    verbose: bool,

    #[arg(long, action = ArgAction::SetTrue, help = "Enable Gen3 relay functionality")]
    enable_gen3: bool,

    #[arg(long, action = ArgAction::SetTrue, help = "Enable Gen4 relay and IPL functionality")]
    enable_gen4: bool,

    #[arg(long, action = ArgAction::SetTrue, help = "Enable Gen5 mapping and power routes")]
    enable_gen5: bool,

    #[arg(long, action = ArgAction::SetTrue, help = "Enable RTOS capture endpoints")]
    enable_rtos: bool,
}

/// Parsed command-line mode for the controller process.
#[derive(Debug, PartialEq, Eq)]
pub enum Cli {
    Help,
    Start {
        config_path: String,
        check_only: bool,
        log_level: Option<String>,
        log_file: Option<String>,
        log_network: bool,
        log_stream: bool,
        metrics_port: u16,
        bind_port: u16,
        features: FeatureFlags,
    },
}

impl Cli {
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
                })
            }
            Err(error) if matches!(error.kind(), ErrorKind::DisplayHelp | ErrorKind::DisplayVersion) => Ok(Self::Help),
            Err(error) => Err(anyhow!(error.to_string())),
        }
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
        assert!(matches!(cli, Cli::Start { config_path, .. } if config_path == crate::config::LOGIN_PATH));
    }

    #[test]
    fn feature_flags_are_parsed_explicitly() {
        let cli = Cli::parse(arguments(&["--enable-gen4", "--enable-rtos"])).unwrap();
        assert!(matches!(cli, Cli::Start { features, .. } if features == FeatureFlags { gen3: false, gen4: true, gen5: false, rtos: true }));
    }

    #[test]
    fn log_level_and_auxiliary_logging_flags_are_parsed() {
        let cli = Cli::parse(arguments(&["--log-level", "debug", "--log-file", "/tmp/edge.log", "--log-network", "--log-stream"])).unwrap();
        assert!(matches!(cli, Cli::Start { log_level, log_file, log_network, log_stream, .. } if log_level.as_deref() == Some("debug") && log_file.as_deref() == Some("/tmp/edge.log") && log_network && log_stream));
    }
}

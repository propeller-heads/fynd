//! The command-line arguments of the health monitor.

use std::{fmt, path::PathBuf, time::Duration};

/// The settings of the health monitor.
///
/// The monitor runs when `--rpc-url` is set, and then needs `--tycho-url` too.
#[derive(clap::Args, Clone)]
pub struct MonitorArgs {
    /// Node RPC URL. The node must support `eth_simulateV1` and `debug_traceCall`. Setting it
    /// turns the health monitor on.
    #[arg(long, env = "RPC_URL", hide_env_values = true)]
    pub rpc_url: Option<String>,

    /// Tycho URL, such as `tycho-beta.propellerheads.xyz`, without a scheme.
    #[arg(long, env = "TYCHO_URL")]
    pub tycho_url: Option<String>,

    /// Tycho API key.
    #[arg(long, env = "TYCHO_API_KEY", hide_env_values = true)]
    pub tycho_api_key: Option<String>,

    /// Whether to connect to Tycho over TLS.
    #[arg(long, env = "TYCHO_USE_TLS", default_value_t = true, action = clap::ArgAction::Set)]
    pub tycho_use_tls: bool,

    /// Minimum TVL in the chain's native token for a component to be tracked. Defaults to the
    /// chain's low TVL threshold.
    #[arg(long, env = "MIN_TVL")]
    pub min_tvl: Option<f64>,

    /// The JSON file the health records are kept in and read back from on start.
    #[arg(long, env = "HEALTH_FILE", default_value = "component-health.json")]
    pub health_file: PathBuf,

    /// Probes per second.
    #[arg(long, env = "PROBE_RATE", default_value_t = 2.0, value_parser = positive_rate)]
    pub probe_rate: f64,

    /// Value of one probe swap in USD.
    #[arg(long, env = "PROBE_USD", default_value_t = 500.0)]
    pub probe_usd: f64,

    /// The largest share of a component's depth that one probe may sell, in (0, 1].
    #[arg(long, env = "PROBE_DEPTH_SHARE", default_value_t = 0.01)]
    pub probe_depth_share: f64,

    /// How old the last probe of a healthy component may get before it is probed again, such as
    /// `6h`, `30m` or `90s`.
    #[arg(long, env = "REFRESH_INTERVAL", default_value = "6h", value_parser = parse_duration)]
    pub refresh_interval: Duration,

    /// The longest wait before retrying a component whose last probe was not clean. The wait
    /// starts at one minute and doubles.
    #[arg(long, env = "MAX_RETRY_BACKOFF", default_value = "1h", value_parser = parse_duration)]
    pub max_retry_backoff: Duration,

    /// Pool reverts in a row, on different blocks, that make a component broken.
    #[arg(
        long,
        env = "BROKEN_AFTER",
        default_value_t = 2,
        value_parser = clap::value_parser!(u32).range(1..)
    )]
    pub broken_after: u32,

    /// Executed probes in a row with a shortfall over `--inaccuracy-bps` that make a component
    /// inaccurate.
    #[arg(
        long,
        env = "INACCURATE_AFTER",
        default_value_t = 3,
        value_parser = clap::value_parser!(u32).range(1..)
    )]
    pub inaccurate_after: u32,

    /// How far, in basis points, a probe may pay less than the component's state predicted
    /// before it counts as a shortfall.
    #[arg(long, env = "INACCURACY_BPS", default_value_t = 100.0)]
    pub inaccuracy_bps: f64,
}

impl MonitorArgs {
    /// Whether the health monitor is turned on.
    pub fn is_enabled(&self) -> bool {
        self.rpc_url.is_some()
    }

    /// The time between two probes.
    pub fn probe_interval(&self) -> Duration {
        Duration::from_secs_f64(1.0 / self.probe_rate)
    }
}

impl fmt::Debug for MonitorArgs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MonitorArgs")
            .field("rpc_url", &self.rpc_url.is_some())
            .field("tycho_url", &self.tycho_url)
            .field("tycho_api_key", &self.tycho_api_key.is_some())
            .field("health_file", &self.health_file)
            .field("probe_rate", &self.probe_rate)
            .field("probe_usd", &self.probe_usd)
            .finish_non_exhaustive()
    }
}

fn positive_rate(text: &str) -> Result<f64, String> {
    let rate: f64 = text
        .parse()
        .map_err(|error| format!("not a number: {error}"))?;
    if rate.is_finite() && rate > 0.0 {
        Ok(rate)
    } else {
        Err("must be above zero".to_string())
    }
}

/// Reads a duration written as a whole number and a unit: `90s`, `30m` or `6h`.
fn parse_duration(text: &str) -> Result<Duration, String> {
    let (digits, unit_seconds) = if let Some(digits) = text.strip_suffix('h') {
        (digits, 3600)
    } else if let Some(digits) = text.strip_suffix('m') {
        (digits, 60)
    } else if let Some(digits) = text.strip_suffix('s') {
        (digits, 1)
    } else {
        return Err(format!("'{text}' has no unit; write it like 90s, 30m or 6h"));
    };
    let count: u64 = digits
        .parse()
        .map_err(|_| format!("'{text}' is not a whole number with a unit like 90s, 30m or 6h"))?;
    if count == 0 {
        return Err("must be above zero".to_string());
    }
    Ok(Duration::from_secs(count * unit_seconds))
}

#[cfg(test)]
pub(crate) mod tests {
    use clap::Parser;
    use rstest::rstest;

    use super::*;

    #[derive(Parser)]
    struct TestCli {
        #[command(flatten)]
        monitor: MonitorArgs,
    }

    pub(crate) fn parse(arguments: &[&str]) -> Result<MonitorArgs, clap::Error> {
        let mut command_line = vec!["test"];
        command_line.extend_from_slice(arguments);
        TestCli::try_parse_from(command_line).map(|cli| cli.monitor)
    }

    #[test]
    fn test_defaults() {
        let args = parse(&[]).unwrap();

        assert!(!args.is_enabled());
        assert_eq!(args.probe_rate, 2.0);
        assert_eq!(args.probe_usd, 500.0);
        assert_eq!(args.probe_depth_share, 0.01);
        assert_eq!(args.refresh_interval, Duration::from_secs(6 * 3600));
        assert_eq!(args.max_retry_backoff, Duration::from_secs(3600));
        assert_eq!((args.broken_after, args.inaccurate_after), (2, 3));
        assert_eq!(args.inaccuracy_bps, 100.0);
        assert!(args.tycho_use_tls);
        assert_eq!(args.health_file, PathBuf::from("component-health.json"));
        assert_eq!(args.probe_interval(), Duration::from_millis(500));
    }

    #[test]
    fn test_rpc_url_turns_the_monitor_on() {
        let args = parse(&["--rpc-url", "https://node.example"]).unwrap();

        assert!(args.is_enabled());
    }

    #[test]
    fn test_tls_can_be_turned_off() {
        let args = parse(&["--tycho-use-tls", "false"]).unwrap();

        assert!(!args.tycho_use_tls);
    }

    #[rstest]
    #[case::seconds("90s", 90)]
    #[case::minutes("30m", 1800)]
    #[case::hours("6h", 21600)]
    fn test_durations(#[case] text: &str, #[case] seconds: u64) {
        let args = parse(&["--refresh-interval", text]).unwrap();

        assert_eq!(args.refresh_interval, Duration::from_secs(seconds));
    }

    #[rstest]
    #[case::refresh_without_unit(&["--refresh-interval", "90"])]
    #[case::refresh_unknown_unit(&["--refresh-interval", "2d"])]
    #[case::backoff_not_a_number(&["--max-retry-backoff", "soonh"])]
    #[case::refresh_zero(&["--refresh-interval", "0s"])]
    #[case::zero_rate(&["--probe-rate", "0"])]
    #[case::negative_rate(&["--probe-rate=-1"])]
    #[case::zero_broken_after(&["--broken-after", "0"])]
    #[case::zero_inaccurate_after(&["--inaccurate-after", "0"])]
    fn test_invalid_values_are_rejected(#[case] arguments: &[&str]) {
        assert!(parse(arguments).is_err());
    }

    #[test]
    fn test_debug_hides_secrets() {
        let args =
            parse(&["--rpc-url", "https://node.example/key123", "--tycho-api-key", "secret"])
                .unwrap();

        let shown = format!("{args:?}");

        assert!(!shown.contains("key123") && !shown.contains("secret"), "{shown}");
    }
}

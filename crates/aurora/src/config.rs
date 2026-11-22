use std::net::IpAddr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};

pub const USAGE: &str = "\
Usage: aurora [--db <path>] [--host <ip>] [--port <port>] [--backup-dir <path>]
              [--allow-unsafe-location] [--public-url <url>] [--max-runs <n>]

  --db <path>          SQLite database file   (env AURORA_DB,   default aurora.db)
  --host <ip>          address to listen on   (env AURORA_HOST, default 127.0.0.1)
  --port <port>        TCP port to listen on  (env AURORA_PORT, default 8787)
  --backup-dir <path>  where backups are kept (env AURORA_BACKUP_DIR,
                       default <db file name>.backups next to the database)
  --allow-unsafe-location
                       use a network or cloud-synced folder anyway, development
                       only (env AURORA_ALLOW_UNSAFE_LOCATION=1)
  --public-url <url>   the http(s) address people reach this server at, for
                       absolute links in agent prompts (env AURORA_PUBLIC_URL,
                       default none: links stay relative)
  --max-runs <n>       how many agent runs may go at once, all agents together;
                       more wait in a queue (env AURORA_MAX_RUNS, 1 to 64,
                       default 4)

Flags override environment variables.

       aurora check [--db <path>] [--json]

  Runs the invariant checks on a database without changing it (see
  docs/operations.md). Works on an offline file, a copy, or beside a running
  server. Exit code 0: every check passed, 1: a check failed, 2: the database
  could not be opened.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub db_path: PathBuf,
    pub host: IpAddr,
    pub port: u16,
    pub backup_dir: Option<PathBuf>,
    pub allow_unsafe_location: bool,
    /// How long request ids are remembered; `None` means 7 days. Not in the
    /// usage text: `--request-retention-days <n>` / `AURORA_REQUEST_RETENTION_DAYS`.
    pub request_retention: Option<Duration>,
    /// The base URL people reach the server at, without a trailing slash;
    /// used for absolute links in the first message of a run.
    pub public_url: Option<String>,
    /// The size cap of one run's transcript file in bytes; `None` means 8 MiB.
    /// Not in the usage text: `--transcript-max-bytes <n>` /
    /// `AURORA_TRANSCRIPT_MAX_BYTES`.
    pub transcript_max_bytes: Option<u64>,
    /// How many runs may go at once; `None` means 4. `--max-runs <n>` /
    /// `AURORA_MAX_RUNS`, 1 to 64.
    pub max_runs: Option<i64>,
    /// How many queued runs one agent may have; `None` means 20. Not in the
    /// usage text: `--max-queued-per-agent <n>` / `AURORA_MAX_QUEUED_PER_AGENT`,
    /// 0 to 1000 (0: nothing waits, a trigger over the limit is dropped).
    pub max_queued_per_agent: Option<i64>,
    /// A queued run that waited this long is canceled; `None` means never.
    /// Not in the usage text: `--max-queue-wait-secs <n>` /
    /// `AURORA_MAX_QUEUE_WAIT_SECS`, 0 (never) to 604800.
    pub max_queue_wait: Option<Duration>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            db_path: PathBuf::from("aurora.db"),
            host: IpAddr::from([127, 0, 0, 1]),
            port: 8787,
            backup_dir: None,
            allow_unsafe_location: false,
            request_retention: None,
            public_url: None,
            transcript_max_bytes: None,
            max_runs: None,
            max_queued_per_agent: None,
            max_queue_wait: None,
        }
    }
}

/// What `aurora check` was asked to check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckConfig {
    pub db_path: PathBuf,
    pub json: bool,
}

/// What the command line asked for.
pub enum Command {
    Run(Config),
    Check(CheckConfig),
    Help,
}

fn parse_check(
    args: impl Iterator<Item = String>,
    env: impl Fn(&str) -> Option<String>,
) -> Result<Command> {
    let mut check = CheckConfig {
        db_path: env("AURORA_DB").map_or_else(|| Config::default().db_path, PathBuf::from),
        json: false,
    };
    let mut args = args;
    while let Some(arg) = args.next() {
        let (flag, inline) = match arg.split_once('=') {
            Some((flag, value)) => (flag.to_owned(), Some(value.to_owned())),
            None => (arg, None),
        };
        match (flag.as_str(), inline) {
            ("--help" | "-h", _) => return Ok(Command::Help),
            ("--json", None) => check.json = true,
            ("--db", inline) => {
                let Some(value) = inline.or_else(|| args.next()) else {
                    bail!("--db needs a value");
                };
                check.db_path = PathBuf::from(value);
            }
            _ => bail!("unknown argument {flag:?} for `aurora check`\n\n{USAGE}"),
        }
    }
    Ok(Command::Check(check))
}

impl Config {
    /// `env` looks up an environment variable; `args` excludes the program name.
    pub fn parse(
        args: impl IntoIterator<Item = String>,
        env: impl Fn(&str) -> Option<String>,
    ) -> Result<Command> {
        let mut args = args.into_iter().peekable();
        if args.peek().is_some_and(|first| first == "check") {
            args.next();
            return parse_check(args, env);
        }
        let mut config = Config::default();
        if let Some(v) = env("AURORA_DB") {
            config.db_path = PathBuf::from(v);
        }
        if let Some(v) = env("AURORA_HOST") {
            config.host = parse_host(&v).context("AURORA_HOST")?;
        }
        if let Some(v) = env("AURORA_PORT") {
            config.port = parse_port(&v).context("AURORA_PORT")?;
        }
        if let Some(v) = env("AURORA_BACKUP_DIR") {
            config.backup_dir = Some(PathBuf::from(v));
        }
        if let Some(v) = env("AURORA_ALLOW_UNSAFE_LOCATION") {
            config.allow_unsafe_location = matches!(v.as_str(), "1" | "true");
        }
        if let Some(v) = env("AURORA_REQUEST_RETENTION_DAYS") {
            config.request_retention =
                Some(parse_days(&v).context("AURORA_REQUEST_RETENTION_DAYS")?);
        }

        if let Some(v) = env("AURORA_PUBLIC_URL") {
            config.public_url = Some(parse_public_url(&v).context("AURORA_PUBLIC_URL")?);
        }
        if let Some(v) = env("AURORA_TRANSCRIPT_MAX_BYTES") {
            config.transcript_max_bytes =
                Some(parse_transcript_cap(&v).context("AURORA_TRANSCRIPT_MAX_BYTES")?);
        }
        if let Some(v) = env("AURORA_MAX_RUNS") {
            config.max_runs = Some(parse_max_runs(&v).context("AURORA_MAX_RUNS")?);
        }
        if let Some(v) = env("AURORA_MAX_QUEUED_PER_AGENT") {
            config.max_queued_per_agent =
                Some(parse_max_queued(&v).context("AURORA_MAX_QUEUED_PER_AGENT")?);
        }
        if let Some(v) = env("AURORA_MAX_QUEUE_WAIT_SECS") {
            config.max_queue_wait = parse_queue_wait(&v).context("AURORA_MAX_QUEUE_WAIT_SECS")?;
        }

        while let Some(arg) = args.next() {
            let (flag, inline) = match arg.split_once('=') {
                Some((flag, value)) => (flag.to_owned(), Some(value.to_owned())),
                None => (arg, None),
            };
            if flag == "--help" || flag == "-h" {
                return Ok(Command::Help);
            }
            if flag == "--allow-unsafe-location" && inline.is_none() {
                config.allow_unsafe_location = true;
                continue;
            }
            if !matches!(
                flag.as_str(),
                "--db"
                    | "--host"
                    | "--port"
                    | "--backup-dir"
                    | "--request-retention-days"
                    | "--public-url"
                    | "--transcript-max-bytes"
                    | "--max-runs"
                    | "--max-queued-per-agent"
                    | "--max-queue-wait-secs"
            ) {
                bail!("unknown argument {flag:?}\n\n{USAGE}");
            }
            let Some(value) = inline.or_else(|| args.next()) else {
                bail!("{flag} needs a value");
            };
            match flag.as_str() {
                "--db" => config.db_path = PathBuf::from(value),
                "--host" => config.host = parse_host(&value).context("--host")?,
                "--backup-dir" => config.backup_dir = Some(PathBuf::from(value)),
                "--public-url" => {
                    config.public_url = Some(parse_public_url(&value).context("--public-url")?);
                }
                "--request-retention-days" => {
                    config.request_retention =
                        Some(parse_days(&value).context("--request-retention-days")?);
                }
                "--transcript-max-bytes" => {
                    config.transcript_max_bytes =
                        Some(parse_transcript_cap(&value).context("--transcript-max-bytes")?);
                }
                "--max-runs" => {
                    config.max_runs = Some(parse_max_runs(&value).context("--max-runs")?);
                }
                "--max-queued-per-agent" => {
                    config.max_queued_per_agent =
                        Some(parse_max_queued(&value).context("--max-queued-per-agent")?);
                }
                "--max-queue-wait-secs" => {
                    config.max_queue_wait =
                        parse_queue_wait(&value).context("--max-queue-wait-secs")?;
                }
                _ => config.port = parse_port(&value).context("--port")?,
            }
        }
        Ok(Command::Run(config))
    }
}

fn parse_host(v: &str) -> Result<IpAddr> {
    v.parse()
        .with_context(|| format!("{v:?} is not an IP address"))
}

fn parse_public_url(v: &str) -> Result<String> {
    aurora_store::normalize_public_url(v)
        .map_err(|why| anyhow::anyhow!("{v:?} is not a usable base URL: {why}"))
}

fn parse_days(v: &str) -> Result<Duration> {
    let days: u64 = v
        .parse()
        .ok()
        .filter(|days| (1..=3650).contains(days))
        .with_context(|| format!("{v:?} is not a number of days from 1 to 3650"))?;
    Ok(Duration::from_secs(days * 24 * 60 * 60))
}

fn parse_transcript_cap(v: &str) -> Result<u64> {
    let (min, max) = (
        crate::transcript::MIN_MAX_BYTES,
        crate::transcript::LARGEST_MAX_BYTES,
    );
    v.trim()
        .parse::<u64>()
        .ok()
        .filter(|bytes| (min..=max).contains(bytes))
        .with_context(|| format!("{v:?} is not a number of bytes from {min} to {max}"))
}

fn parse_max_runs(v: &str) -> Result<i64> {
    v.trim()
        .parse::<i64>()
        .ok()
        .filter(|n| (1..=64).contains(n))
        .with_context(|| format!("{v:?} is not a number from 1 to 64"))
}

fn parse_max_queued(v: &str) -> Result<i64> {
    v.trim()
        .parse::<i64>()
        .ok()
        .filter(|n| (0..=1000).contains(n))
        .with_context(|| format!("{v:?} is not a number from 0 to 1000"))
}

/// `0` means never.
fn parse_queue_wait(v: &str) -> Result<Option<Duration>> {
    let secs: u64 = v
        .trim()
        .parse()
        .ok()
        .filter(|n| *n <= 604_800)
        .with_context(|| format!("{v:?} is not a number of seconds from 0 to 604800"))?;
    Ok((secs > 0).then(|| Duration::from_secs(secs)))
}

fn parse_port(v: &str) -> Result<u16> {
    v.parse()
        .with_context(|| format!("{v:?} is not a port number"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(args: &[&str], env: &[(&str, &str)]) -> Result<Config> {
        let env: Vec<(String, String)> = env
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let lookup = move |k: &str| env.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        match Config::parse(args.iter().map(|s| s.to_string()), lookup)? {
            Command::Run(c) => Ok(c),
            Command::Check(_) => bail!("check"),
            Command::Help => bail!("help"),
        }
    }

    fn check(args: &[&str], env: &[(&str, &str)]) -> Result<CheckConfig> {
        let env: Vec<(String, String)> = env
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let lookup = move |k: &str| env.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        match Config::parse(args.iter().map(|s| s.to_string()), lookup)? {
            Command::Check(c) => Ok(c),
            _ => bail!("not a check"),
        }
    }

    #[test]
    fn check_takes_a_database_and_a_json_flag() {
        let c = check(&["check", "--db", "x.db", "--json"], &[]).unwrap();
        assert_eq!((c.db_path, c.json), (PathBuf::from("x.db"), true));
        let c = check(&["check"], &[("AURORA_DB", "e.db")]).unwrap();
        assert_eq!((c.db_path, c.json), (PathBuf::from("e.db"), false));
        let c = check(&["check", "--db=y.db"], &[("AURORA_DB", "e.db")]).unwrap();
        assert_eq!(c.db_path, PathBuf::from("y.db"));
        assert!(check(&["check", "--port", "1"], &[]).is_err());
        assert!(check(&["check", "--db"], &[]).is_err());
    }

    #[test]
    fn defaults() {
        assert_eq!(run(&[], &[]).unwrap(), Config::default());
    }

    #[test]
    fn flags_override_env() {
        let c = run(
            &["--port", "9000", "--db=x.db"],
            &[("AURORA_PORT", "1"), ("AURORA_DB", "y.db")],
        )
        .unwrap();
        assert_eq!((c.port, c.db_path), (9000, PathBuf::from("x.db")));
    }

    #[test]
    fn backup_dir_and_unsafe_location_options() {
        let c = run(&["--backup-dir", "b", "--allow-unsafe-location"], &[]).unwrap();
        assert_eq!(c.backup_dir, Some(PathBuf::from("b")));
        assert!(c.allow_unsafe_location);
        let c = run(
            &[],
            &[
                ("AURORA_BACKUP_DIR", "e"),
                ("AURORA_ALLOW_UNSAFE_LOCATION", "1"),
            ],
        )
        .unwrap();
        assert_eq!(c.backup_dir, Some(PathBuf::from("e")));
        assert!(c.allow_unsafe_location);
    }

    #[test]
    fn request_retention_is_set_in_days() {
        let week = Duration::from_secs(7 * 24 * 60 * 60);
        assert_eq!(run(&[], &[]).unwrap().request_retention, None);
        let c = run(&["--request-retention-days", "7"], &[]).unwrap();
        assert_eq!(c.request_retention, Some(week));
        let c = run(&[], &[("AURORA_REQUEST_RETENTION_DAYS", "1")]).unwrap();
        assert_eq!(c.request_retention, Some(Duration::from_secs(86_400)));
        assert!(run(&["--request-retention-days", "0"], &[]).is_err());
        assert!(run(&[], &[("AURORA_REQUEST_RETENTION_DAYS", "x")]).is_err());
    }

    #[test]
    fn the_transcript_cap_is_a_checked_number_of_bytes() {
        assert_eq!(run(&[], &[]).unwrap().transcript_max_bytes, None);
        let c = run(&[], &[("AURORA_TRANSCRIPT_MAX_BYTES", "4096")]).unwrap();
        assert_eq!(c.transcript_max_bytes, Some(4096));
        let c = run(
            &["--transcript-max-bytes=2048"],
            &[("AURORA_TRANSCRIPT_MAX_BYTES", "4096")],
        )
        .unwrap();
        assert_eq!(c.transcript_max_bytes, Some(2048));
        assert!(run(&["--transcript-max-bytes", "10"], &[]).is_err());
        assert!(run(&["--transcript-max-bytes", "x"], &[]).is_err());
        assert!(run(&[], &[("AURORA_TRANSCRIPT_MAX_BYTES", "99999999999")]).is_err());
        assert!(run(&["--transcript-max-bytes"], &[]).is_err());
    }

    #[test]
    fn the_run_limits_are_checked_numbers() {
        let c = run(&[], &[]).unwrap();
        assert_eq!(
            (c.max_runs, c.max_queued_per_agent, c.max_queue_wait),
            (None, None, None)
        );
        let c = run(&[], &[("AURORA_MAX_RUNS", "7")]).unwrap();
        assert_eq!(c.max_runs, Some(7));
        let c = run(&["--max-runs=2"], &[("AURORA_MAX_RUNS", "7")]).unwrap();
        assert_eq!(c.max_runs, Some(2), "a flag wins over the environment");
        let c = run(&["--max-runs", "64"], &[]).unwrap();
        assert_eq!(c.max_runs, Some(64));
        for bad in ["0", "65", "-1", "x", "", "1.5"] {
            assert!(run(&["--max-runs", bad], &[]).is_err(), "{bad:?}");
            assert!(run(&[], &[("AURORA_MAX_RUNS", bad)]).is_err(), "{bad:?}");
        }
        assert!(run(&["--max-runs"], &[]).is_err());

        let c = run(&[], &[("AURORA_MAX_QUEUED_PER_AGENT", "0")]).unwrap();
        assert_eq!(c.max_queued_per_agent, Some(0));
        let c = run(&["--max-queued-per-agent", "1000"], &[]).unwrap();
        assert_eq!(c.max_queued_per_agent, Some(1000));
        assert!(run(&["--max-queued-per-agent", "1001"], &[]).is_err());
        assert!(run(&[], &[("AURORA_MAX_QUEUED_PER_AGENT", "-1")]).is_err());

        let c = run(&[], &[("AURORA_MAX_QUEUE_WAIT_SECS", "90")]).unwrap();
        assert_eq!(c.max_queue_wait, Some(Duration::from_secs(90)));
        let c = run(&["--max-queue-wait-secs", "0"], &[]).unwrap();
        assert_eq!(c.max_queue_wait, None, "0 means never");
        assert!(run(&["--max-queue-wait-secs", "604801"], &[]).is_err());
        assert!(run(&[], &[("AURORA_MAX_QUEUE_WAIT_SECS", "soon")]).is_err());
    }

    #[test]
    fn public_url_is_checked_and_trimmed() {
        assert_eq!(run(&[], &[]).unwrap().public_url, None);
        let c = run(&["--public-url", "https://aurora.example.com/"], &[]).unwrap();
        assert_eq!(c.public_url.as_deref(), Some("https://aurora.example.com"));
        let c = run(&[], &[("AURORA_PUBLIC_URL", "http://10.0.0.5:8787")]).unwrap();
        assert_eq!(c.public_url.as_deref(), Some("http://10.0.0.5:8787"));
        let c = run(
            &["--public-url=http://a.example"],
            &[("AURORA_PUBLIC_URL", "http://b.example")],
        )
        .unwrap();
        assert_eq!(c.public_url.as_deref(), Some("http://a.example"));
        assert!(run(&["--public-url", "ftp://x"], &[]).is_err());
        assert!(run(&["--public-url", "not a url"], &[]).is_err());
        assert!(run(&["--public-url"], &[]).is_err());
        assert!(run(&[], &[("AURORA_PUBLIC_URL", "localhost:1")]).is_err());
    }

    #[test]
    fn bad_values_are_errors() {
        assert!(run(&["--port", "nope"], &[]).is_err());
        assert!(run(&["--bogus"], &[]).is_err());
        assert!(run(&["--port"], &[]).is_err());
        assert!(run(&[], &[("AURORA_HOST", "not-an-ip")]).is_err());
    }
}

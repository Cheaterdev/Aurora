use std::net::IpAddr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};

pub const USAGE: &str = "\
Usage: aurora [--db <path>] [--host <ip>] [--port <port>] [--backup-dir <path>]
              [--allow-unsafe-location]

  --db <path>          SQLite database file   (env AURORA_DB,   default aurora.db)
  --host <ip>          address to listen on   (env AURORA_HOST, default 127.0.0.1)
  --port <port>        TCP port to listen on  (env AURORA_PORT, default 8787)
  --backup-dir <path>  where backups are kept (env AURORA_BACKUP_DIR,
                       default <db file name>.backups next to the database)
  --allow-unsafe-location
                       use a network or cloud-synced folder anyway, development
                       only (env AURORA_ALLOW_UNSAFE_LOCATION=1)

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
                "--db" | "--host" | "--port" | "--backup-dir" | "--request-retention-days"
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
                "--request-retention-days" => {
                    config.request_retention =
                        Some(parse_days(&value).context("--request-retention-days")?);
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

fn parse_days(v: &str) -> Result<Duration> {
    let days: u64 = v
        .parse()
        .ok()
        .filter(|days| (1..=3650).contains(days))
        .with_context(|| format!("{v:?} is not a number of days from 1 to 3650"))?;
    Ok(Duration::from_secs(days * 24 * 60 * 60))
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
    fn bad_values_are_errors() {
        assert!(run(&["--port", "nope"], &[]).is_err());
        assert!(run(&["--bogus"], &[]).is_err());
        assert!(run(&["--port"], &[]).is_err());
        assert!(run(&[], &[("AURORA_HOST", "not-an-ip")]).is_err());
    }
}

use std::net::IpAddr;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};

pub const USAGE: &str = "\
Usage: aurora [--db <path>] [--host <ip>] [--port <port>]

  --db <path>     SQLite database file        (env AURORA_DB,   default aurora.db)
  --host <ip>     address to listen on        (env AURORA_HOST, default 127.0.0.1)
  --port <port>   TCP port to listen on       (env AURORA_PORT, default 8787)

Flags override environment variables.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub db_path: PathBuf,
    pub host: IpAddr,
    pub port: u16,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            db_path: PathBuf::from("aurora.db"),
            host: IpAddr::from([127, 0, 0, 1]),
            port: 8787,
        }
    }
}

/// What the command line asked for.
pub enum Command {
    Run(Config),
    Help,
}

impl Config {
    /// `env` looks up an environment variable; `args` excludes the program name.
    pub fn parse(
        args: impl IntoIterator<Item = String>,
        env: impl Fn(&str) -> Option<String>,
    ) -> Result<Command> {
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

        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            let (flag, inline) = match arg.split_once('=') {
                Some((flag, value)) => (flag.to_owned(), Some(value.to_owned())),
                None => (arg, None),
            };
            if flag == "--help" || flag == "-h" {
                return Ok(Command::Help);
            }
            if !matches!(flag.as_str(), "--db" | "--host" | "--port") {
                bail!("unknown argument {flag:?}\n\n{USAGE}");
            }
            let Some(value) = inline.or_else(|| args.next()) else {
                bail!("{flag} needs a value");
            };
            match flag.as_str() {
                "--db" => config.db_path = PathBuf::from(value),
                "--host" => config.host = parse_host(&value).context("--host")?,
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
            Command::Help => bail!("help"),
        }
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
    fn bad_values_are_errors() {
        assert!(run(&["--port", "nope"], &[]).is_err());
        assert!(run(&["--bogus"], &[]).is_err());
        assert!(run(&["--port"], &[]).is_err());
        assert!(run(&[], &[("AURORA_HOST", "not-an-ip")]).is_err());
    }
}

//! One connection URL, two schemes.
//!
//! ```text
//! postgres://user:password@host:5432/dbname?sslmode=disable&connect_timeout=10
//! mysql://user:password@127.0.0.1:3306/dbname
//! ```
//!
//! Invariant: **the password is never formatted.** `Display` redacts it, and
//! that is the only way a URL reaches a report, a manifest, an error message or
//! an MCP result. A connection URL that reached an agent transcript intact
//! would be a credential in a log file, and the log file outlives the run.

use std::fmt;

use inillucent_base::error::{refusal, DbError};
use inillucent_base::DbResult;

/// Which server a URL names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scheme {
    /// PostgreSQL, over the version 3 frontend/backend protocol.
    Postgres,
    /// MySQL or MariaDB, over the client/server protocol.
    Mysql,
}

impl Scheme {
    /// Returns the port a server of this kind listens on by default.
    pub fn default_port(self) -> u16 {
        match self {
            Scheme::Postgres => 5432,
            Scheme::Mysql => 3306,
        }
    }

    /// Returns the name this scheme is written as.
    pub fn name(self) -> &'static str {
        match self {
            Scheme::Postgres => "postgres",
            Scheme::Mysql => "mysql",
        }
    }
}

/// Where to connect, as who, and to which database.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConnectionUrl {
    /// Which protocol to speak.
    pub scheme: Scheme,
    /// The host to reach, without brackets even when it is IPv6.
    pub host: String,
    /// The port, defaulted from the scheme when the URL omits one.
    pub port: u16,
    /// The role or account to log in as.
    pub user: String,
    /// The password, which never appears in `Display`.
    pub password: Option<String>,
    /// The database to read.
    pub database: String,
    /// Every query parameter, in the order it was written.
    pub parameters: Vec<(String, String)>,
}

impl ConnectionUrl {
    /// Parses a connection URL.
    ///
    /// Every failure names what was wrong with the URL and shows nothing of the
    /// password, because a parse error is the first thing that gets pasted into
    /// a bug report.
    ///
    /// @param text - the URL as it was written on the command line
    pub fn parse(text: &str) -> DbResult<ConnectionUrl> {
        let (scheme, rest) = if let Some(rest) = strip_scheme(text, "postgres") {
            (Scheme::Postgres, rest)
        } else if let Some(rest) = strip_scheme(text, "postgresql") {
            (Scheme::Postgres, rest)
        } else if let Some(rest) = strip_scheme(text, "mysql") {
            (Scheme::Mysql, rest)
        } else if let Some(rest) = strip_scheme(text, "mariadb") {
            (Scheme::Mysql, rest)
        } else {
            return Err(refusal(
                "a connection URL must begin postgres://, postgresql://, mysql:// or mariadb://",
            ));
        };

        // The query string first, so a `?` inside it cannot be mistaken for the
        // one that starts it, and a `@` or `/` inside a parameter value cannot
        // be read as authority punctuation.
        let (authority_and_path, query) = match rest.split_once('?') {
            Some((head, tail)) => (head, Some(tail)),
            None => (rest, None),
        };

        // The **last** `@` separates userinfo from the host, because a password
        // may contain one and a host may not.
        let (userinfo, hostport_and_path) = match authority_and_path.rsplit_once('@') {
            Some((head, tail)) => (Some(head), tail),
            None => (None, authority_and_path),
        };

        let (hostport, database) = match hostport_and_path.split_once('/') {
            Some((head, tail)) => (head, tail),
            None => (hostport_and_path, ""),
        };

        let (host, port) = split_host_and_port(hostport, scheme)?;
        let (user, password) = split_userinfo(userinfo, scheme)?;

        let mut parameters = Vec::new();
        if let Some(query) = query {
            for pair in query.split('&').filter(|pair| !pair.is_empty()) {
                let (name, value) = match pair.split_once('=') {
                    Some((name, value)) => (name, value),
                    None => (pair, ""),
                };
                parameters.push((decode(name)?, decode(value)?));
            }
        }

        let database = decode(database)?;
        if database.is_empty() {
            return Err(refusal(format!(
                "the {} URL names no database; write it after the host, as {}://host/dbname",
                scheme.name(),
                scheme.name()
            )));
        }

        Ok(ConnectionUrl {
            scheme,
            host,
            port,
            user,
            password,
            database,
            parameters,
        })
    }

    /// Returns the value of a query parameter, if the URL carries one.
    ///
    /// @param name - the parameter's name, compared case-insensitively
    pub fn parameter(&self, name: &str) -> Option<&str> {
        self.parameters
            .iter()
            .find(|(held, _)| held.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// Returns `host:port`, bracketing an IPv6 literal so it can be dialled.
    pub fn address(&self) -> String {
        if self.host.contains(':') {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }

    /// Returns the connect and read timeout the URL asks for, in seconds.
    ///
    /// Defaulted rather than unbounded: a migration that hangs against a
    /// firewalled host with no message is indistinguishable from one that is
    /// working, and this tool's whole discipline is that an intermediate state
    /// must be recognisable.
    pub fn timeout_seconds(&self) -> u64 {
        self.parameter("connect_timeout")
            .or_else(|| self.parameter("timeout"))
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|seconds| *seconds > 0)
            .unwrap_or(30)
    }

    /// Refuses a URL that asks for transport security this client cannot give.
    ///
    /// **Named, not silently downgraded.** A client that answered `sslmode=require`
    /// by connecting in the clear would be doing the one thing the parameter
    /// exists to forbid, and the operator would have no way to find out.
    pub fn refuse_unsupported_transport(&self) -> DbResult<()> {
        let asked = self
            .parameter("sslmode")
            .or_else(|| self.parameter("ssl-mode"))
            .unwrap_or("disable");
        match asked.to_ascii_lowercase().as_str() {
            "disable" | "allow" | "prefer" | "disabled" | "preferred" => Ok(()),
            other => Err(refusal(format!(
                "sslmode={other} asks for a TLS connection, and this migration client speaks only \
                 plaintext. Run the migration from a host you trust the network to - a loopback \
                 address, or the database's own machine - and write sslmode=disable to say so."
            ))),
        }
    }
}

impl fmt::Display for ConnectionUrl {
    /// Writes the URL with the password replaced by `***`.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        write!(formatter, "{}://{}", self.scheme.name(), self.user)?;
        if self.password.is_some() {
            formatter.write_str(":***")?;
        }
        write!(formatter, "@{host}:{}/{}", self.port, self.database)
    }
}

/// Returns what follows `<scheme>://`, when the text begins with it.
///
/// @param text - the whole URL
/// @param scheme - the scheme name to strip
fn strip_scheme<'t>(text: &'t str, scheme: &str) -> Option<&'t str> {
    let prefix = format!("{scheme}://");
    if text.len() >= prefix.len() && text.get(..prefix.len())?.eq_ignore_ascii_case(&prefix) {
        text.get(prefix.len()..)
    } else {
        None
    }
}

/// Splits `host`, `host:port`, `[v6]` or `[v6]:port` into its two parts.
///
/// @param text - the authority's host portion
/// @param scheme - which default port applies
fn split_host_and_port(text: &str, scheme: Scheme) -> DbResult<(String, u16)> {
    if let Some(rest) = text.strip_prefix('[') {
        let (host, tail) = rest
            .split_once(']')
            .ok_or_else(|| refusal("an IPv6 host in a connection URL must be closed with ]"))?;
        let port = match tail.strip_prefix(':') {
            Some(port) => parse_port(port)?,
            None => scheme.default_port(),
        };
        return Ok((decode(host)?, port));
    }
    match text.rsplit_once(':') {
        Some((host, port)) => Ok((defaulted_host(decode(host)?), parse_port(port)?)),
        None => Ok((defaulted_host(decode(text)?), scheme.default_port())),
    }
}

/// Returns the host to dial, defaulting an empty one to the loopback address.
///
/// @param host - the host as the URL wrote it
fn defaulted_host(host: String) -> String {
    if host.is_empty() {
        "127.0.0.1".to_string()
    } else {
        host
    }
}

/// Returns a port number, refusing anything that is not one.
///
/// @param text - the digits after the colon
fn parse_port(text: &str) -> DbResult<u16> {
    text.parse::<u16>()
        .map_err(|_| refusal(format!("'{text}' is not a port number")))
}

/// Splits `user`, `user:password` or an absent userinfo into its two parts.
///
/// A URL with no user is not an error: `psql` defaults to the operating
/// system's user and `mysql` to `root`, and a migration run by hand on the
/// database's own machine is the case where that matters.
///
/// @param userinfo - the text before the last `@`, when there was one
/// @param scheme - which default user applies
fn split_userinfo(userinfo: Option<&str>, scheme: Scheme) -> DbResult<(String, Option<String>)> {
    let Some(userinfo) = userinfo else {
        return Ok((default_user(scheme), None));
    };
    // The **first** `:` separates user from password, because a password may
    // contain one and a user name may not.
    let (user, password) = match userinfo.split_once(':') {
        Some((user, password)) => (decode(user)?, Some(decode(password)?)),
        None => (decode(userinfo)?, None),
    };
    let user = if user.is_empty() {
        default_user(scheme)
    } else {
        user
    };
    Ok((user, password))
}

/// Returns the account a URL that names none logs in as.
///
/// @param scheme - which server is being reached
fn default_user(scheme: Scheme) -> String {
    match scheme {
        Scheme::Postgres => std::env::var("PGUSER")
            .ok()
            .or_else(|| std::env::var("USERNAME").ok())
            .or_else(|| std::env::var("USER").ok())
            .unwrap_or_else(|| "postgres".to_string()),
        Scheme::Mysql => std::env::var("MYSQL_USER").unwrap_or_else(|_| "root".to_string()),
    }
}

/// Returns a percent-decoded component.
///
/// @param text - one URL component, still encoded
fn decode(text: &str) -> Result<String, DbError> {
    let bytes = text.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut at = 0usize;
    while at < bytes.len() {
        match bytes.get(at) {
            Some(b'%') => {
                let high = bytes
                    .get(at.saturating_add(1))
                    .and_then(|byte| (*byte as char).to_digit(16));
                let low = bytes
                    .get(at.saturating_add(2))
                    .and_then(|byte| (*byte as char).to_digit(16));
                match (high, low) {
                    (Some(high), Some(low)) => {
                        out.push(((high << 4) | low) as u8);
                        at = at.saturating_add(3);
                    }
                    _ => {
                        return Err(refusal(
                            "a % in a connection URL must be followed by two hexadecimal digits",
                        ))
                    }
                }
            }
            Some(byte) => {
                out.push(*byte);
                at = at.saturating_add(1);
            }
            None => break,
        }
    }
    String::from_utf8(out)
        .map_err(|_| refusal("a connection URL decoded to bytes that are not UTF-8"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The ordinary shape, every field read out of it.
    #[test]
    fn a_full_postgres_url_parses_into_its_parts() {
        let url =
            ConnectionUrl::parse("postgres://jason:hunter2@db.example:5433/corpus?sslmode=disable")
                .expect("parses");
        assert_eq!(url.scheme, Scheme::Postgres);
        assert_eq!(url.user, "jason");
        assert_eq!(url.password.as_deref(), Some("hunter2"));
        assert_eq!(url.host, "db.example");
        assert_eq!(url.port, 5433);
        assert_eq!(url.database, "corpus");
        assert_eq!(url.parameter("sslmode"), Some("disable"));
    }

    /// Each scheme supplies its own port, and its own alias spelling parses.
    #[test]
    fn a_missing_port_takes_the_schemes_default() {
        let postgres = ConnectionUrl::parse("postgresql://u@h/d").expect("parses");
        assert_eq!(postgres.port, 5432);
        let mysql = ConnectionUrl::parse("mariadb://u@h/d").expect("parses");
        assert_eq!(mysql.scheme, Scheme::Mysql);
        assert_eq!(mysql.port, 3306);
    }

    /// The password may hold the punctuation the URL uses, so the splits are
    /// last-`@` and first-`:` rather than first and last.
    #[test]
    fn punctuation_inside_a_password_does_not_move_the_splits() {
        let url =
            ConnectionUrl::parse("mysql://root:p%40ss:word@127.0.0.1:3307/app").expect("parses");
        assert_eq!(url.user, "root");
        assert_eq!(url.password.as_deref(), Some("p@ss:word"));
        assert_eq!(url.host, "127.0.0.1");
        assert_eq!(url.port, 3307);
        assert_eq!(url.database, "app");
    }

    /// An IPv6 literal is bracketed in the URL and unbracketed in the struct,
    /// and comes back bracketed from `address` so it can be dialled.
    #[test]
    fn an_ipv6_host_keeps_its_brackets_only_where_they_belong() {
        let url = ConnectionUrl::parse("postgres://u@[::1]:5432/d").expect("parses");
        assert_eq!(url.host, "::1");
        assert_eq!(url.address(), "[::1]:5432");
    }

    /// **The password never reaches a formatted string.** This is the test that
    /// keeps a credential out of a report, a manifest and an agent transcript.
    #[test]
    fn display_redacts_the_password() {
        let url = ConnectionUrl::parse("postgres://jason:hunter2@db:5432/corpus").expect("parses");
        let shown = url.to_string();
        assert!(!shown.contains("hunter2"), "{shown}");
        assert_eq!(shown, "postgres://jason:***@db:5432/corpus");
    }

    /// A URL with no password shows no colon at all, rather than an empty one.
    #[test]
    fn display_of_a_passwordless_url_has_no_separator() {
        let url = ConnectionUrl::parse("mysql://root@127.0.0.1/app").expect("parses");
        assert_eq!(url.to_string(), "mysql://root@127.0.0.1:3306/app");
    }

    /// A request for TLS is refused by name rather than answered in the clear.
    #[test]
    fn sslmode_require_is_refused_and_says_why() {
        let url = ConnectionUrl::parse("postgres://u@h/d?sslmode=require").expect("parses");
        let error = url.refuse_unsupported_transport().expect_err("refuses");
        assert!(error.message().contains("plaintext"), "{}", error.message());
        let allowed = ConnectionUrl::parse("postgres://u@h/d?sslmode=prefer").expect("parses");
        assert!(allowed.refuse_unsupported_transport().is_ok());
    }

    /// A URL that names no database is refused, because the alternative is
    /// connecting to whichever one the server defaults to.
    #[test]
    fn a_url_with_no_database_is_refused() {
        assert!(ConnectionUrl::parse("postgres://u@h").is_err());
        assert!(ConnectionUrl::parse("postgres://u@h/").is_err());
    }

    /// Something that is not a connection URL at all - a file path, which is
    /// what the other migration kinds take - is refused with a message naming
    /// the schemes.
    #[test]
    fn a_file_path_is_not_a_connection_url() {
        let error = ConnectionUrl::parse("C:/data/app.db").expect_err("refuses");
        assert!(
            error.message().contains("postgres://"),
            "{}",
            error.message()
        );
    }

    /// Percent-encoding is decoded in every component, and a truncated escape
    /// is an error rather than a silently different password.
    #[test]
    fn percent_encoding_decodes_and_a_broken_escape_is_refused() {
        let url = ConnectionUrl::parse("postgres://a%40b:c%2Fd@h/my%20db").expect("parses");
        assert_eq!(url.user, "a@b");
        assert_eq!(url.password.as_deref(), Some("c/d"));
        assert_eq!(url.database, "my db");
        assert!(ConnectionUrl::parse("postgres://u:p%2@h/d").is_err());
    }

    /// A timeout is read from the URL and defaulted when it is absent, absurd
    /// or unparseable.
    #[test]
    fn the_timeout_is_read_or_defaulted() {
        let given = ConnectionUrl::parse("postgres://u@h/d?connect_timeout=5").expect("parses");
        assert_eq!(given.timeout_seconds(), 5);
        let zero = ConnectionUrl::parse("postgres://u@h/d?connect_timeout=0").expect("parses");
        assert_eq!(zero.timeout_seconds(), 30);
        let absent = ConnectionUrl::parse("postgres://u@h/d").expect("parses");
        assert_eq!(absent.timeout_seconds(), 30);
    }
}

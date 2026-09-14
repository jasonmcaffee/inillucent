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
#[derive(Clone, PartialEq, Eq)]
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

    /// Returns what the URL asked for, as this client's own vocabulary.
    ///
    /// PostgreSQL spells it `sslmode` and MySQL spells it `ssl-mode`, and their
    /// value sets overlap without matching. Both are read here so that one
    /// policy covers both protocols; an unrecognised value is a refusal rather
    /// than a default, because every way of getting this wrong ends with
    /// credentials on a wire somebody can read.
    fn asked_transport(&self) -> DbResult<Asked> {
        let Some(written) = self
            .parameter("sslmode")
            .or_else(|| self.parameter("ssl-mode"))
        else {
            return Ok(Asked::Unstated);
        };
        match written.to_ascii_lowercase().as_str() {
            "disable" | "disabled" => Ok(Asked::Plaintext),
            // PostgreSQL's `allow` and `prefer`, and MySQL's `preferred`, all
            // mean "encrypt if the server will". This client refuses them
            // rather than implementing them: a mode that silently accepts
            // plaintext is a mode whose security depends on a server setting
            // nobody in the migration can see, and an operator reading
            // `sslmode=prefer` in a runbook believes it did something.
            "allow" | "prefer" | "preferred" => Err(refusal(format!(
                "sslmode={written} means \"encrypt if the server happens to allow it\", so \
                 whether your credentials crossed the network in the clear is decided by the \
                 server and is not reported anywhere. Write sslmode=require for verified TLS, or \
                 sslmode=disable together with --insecure-plaintext to say plaintext is what you \
                 want."
            ))),
            "require" | "required" | "verify-ca" | "verify-full" | "verify_ca"
            | "verify_identity" | "verify-identity" => Ok(Asked::VerifiedTls),
            other => Err(refusal(format!(
                "sslmode={other} is not a transport this client knows. It takes 'require' for \
                 verified TLS, or 'disable' for plaintext."
            ))),
        }
    }

    /// Reports whether the host is one this machine reaches without a network.
    ///
    /// A loopback address is the one case where plaintext carries no risk that
    /// encryption would remove: nothing leaves the machine. It is decided from
    /// the *address*, not from the name, so `localhost.evil.example` resolving
    /// somewhere else is not loopback - and a name that does not parse as an
    /// address is not loopback either, because a name is resolved later and by
    /// something this check cannot see.
    pub fn is_loopback(&self) -> bool {
        match self.host.parse::<std::net::IpAddr>() {
            Ok(address) => address.is_loopback(),
            // The two spellings of the loopback name, which resolve to a
            // loopback address on every platform this runs on and are what an
            // operator actually types.
            Err(_) => {
                self.host.eq_ignore_ascii_case("localhost")
                    || self.host.eq_ignore_ascii_case("localhost.")
            }
        }
    }

    /// Decides how this connection is made, and refuses the unsafe defaults.
    ///
    /// **Verified TLS unless the operator said otherwise in as many words.**
    /// The rule this replaced accepted an absent `sslmode` and every mode that
    /// permits plaintext, so the default for a URL naming a host across a
    /// network was to send a password and then every row in the clear. The
    /// documentation advised running it on a trusted host, which is advice
    /// rather than a control.
    ///
    /// Plaintext now needs two things at once: the URL saying `sslmode=disable`
    /// and the caller passing `insecure_plaintext`. One without the other is a
    /// refusal, because each of them alone is a thing somebody types without
    /// meaning it - a copied URL, or a flag added to get past an unrelated
    /// error.
    ///
    /// @param insecure_plaintext - whether the operator passed the flag that
    ///   permits an unencrypted connection
    pub fn transport(&self, insecure_plaintext: bool) -> DbResult<Transport> {
        let asked = self.asked_transport()?;
        match (asked, insecure_plaintext) {
            (Asked::VerifiedTls, _) => Ok(Transport::VerifiedTls),
            // A loopback address is the exception that needs no flag: the bytes
            // do not leave the machine, so there is no network to protect them
            // from, and requiring a flag there would train an operator to pass
            // it everywhere.
            (Asked::Plaintext, _) if self.is_loopback() => Ok(Transport::Plaintext),
            (Asked::Unstated, _) if self.is_loopback() => Ok(Transport::Plaintext),
            (Asked::Plaintext, true) => Ok(Transport::Plaintext),
            (Asked::Plaintext, false) => Err(refusal(format!(
                "sslmode=disable would send this migration's credentials and every row it reads \
                 to {} in the clear. Pass --insecure-plaintext as well if that is really what you \
                 want, or drop sslmode=disable to use verified TLS.",
                self.address()
            ))),
            (Asked::Unstated, true) => Err(refusal(format!(
                "--insecure-plaintext was passed and the URL does not say sslmode=disable, so it \
                 is not clear which you meant. Write sslmode=disable in the URL to connect to {} \
                 in the clear, or drop --insecure-plaintext to use verified TLS.",
                self.address()
            ))),
            (Asked::Unstated, false) => Ok(Transport::VerifiedTls),
        }
    }

    /// Returns the certificate authority file the URL names, if it names one.
    ///
    /// PostgreSQL's own parameter name, and MySQL's, so an operator who has a
    /// private authority already knows what to write. Without one the platform
    /// trust store decides, which is the right default and the only one that
    /// works with a public certificate.
    pub fn root_certificate(&self) -> Option<&str> {
        self.parameter("sslrootcert")
            .or_else(|| self.parameter("ssl-ca"))
            .filter(|value| !value.is_empty())
    }
}

/// What a URL's `sslmode` asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Asked {
    /// The URL said nothing about transport.
    Unstated,
    /// The URL asked for no encryption.
    Plaintext,
    /// The URL asked for encryption with the server's identity checked.
    VerifiedTls,
}

/// How a connection is made, once the policy has decided.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transport {
    /// An unencrypted socket. Loopback, or an explicit choice.
    Plaintext,
    /// TLS with the server's certificate chain and host name checked.
    VerifiedTls,
}

impl Transport {
    /// Returns the word a report and a manifest record this choice by.
    ///
    /// It goes into the migration's own output so that "was this migration
    /// encrypted" is answerable afterwards from the artifact rather than from
    /// whoever ran it. A credential never accompanies it; the transport is not
    /// a secret and the URL that carried it is redacted separately.
    pub fn name(self) -> &'static str {
        match self {
            Transport::Plaintext => "plaintext",
            Transport::VerifiedTls => "verified-tls",
        }
    }
}

impl fmt::Debug for ConnectionUrl {
    /// Writes every field, with `***` for the password.
    ///
    /// **`Debug` was derived, and a derived `Debug` prints the password.** The
    /// module's invariant above says the password is never formatted; `Display`
    /// honoured it and had a test, and `{:?}` walked straight past both. No
    /// `{:?}` on this type existed when task-1946 found it (H5), which is what
    /// made it latent rather than a leak: one `dbg!`, one `#[derive(Debug)]` on
    /// a struct holding a `ConnectionUrl`, or one error type wrapping it, and a
    /// password is in a log file that outlives the run.
    ///
    /// Every other field is printed, because the reason to reach for `{:?}` on a
    /// connection URL is to see how it parsed - which port was defaulted, which
    /// parameters survived - and a `Debug` that hid those would be replaced by a
    /// worse one the first time somebody needed them.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ConnectionUrl")
            .field("scheme", &self.scheme)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("user", &self.user)
            .field("password", &self.password.as_ref().map(|_| Redacted))
            .field("database", &self.database)
            .field("parameters", &self.parameters)
            .finish()
    }
}

/// What stands in for a password in a `Debug` rendering.
///
/// A unit struct rather than the string `"***"`, so the field reads
/// `password: Some(***)` rather than `password: Some("***")` - the quotes would
/// say a password of three asterisks, which is a different claim.
struct Redacted;

impl fmt::Debug for Redacted {
    /// Writes `***`.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("***")
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
            ConnectionUrl::parse("postgres://user:hunter2@db.example:5433/corpus?sslmode=disable")
                .expect("parses");
        assert_eq!(url.scheme, Scheme::Postgres);
        assert_eq!(url.user, "user");
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
        let url = ConnectionUrl::parse("postgres://user:hunter2@db:5432/corpus").expect("parses");
        let shown = url.to_string();
        assert!(!shown.contains("hunter2"), "{shown}");
        assert_eq!(shown, "postgres://user:***@db:5432/corpus");
    }

    /// `{:?}` redacts the password too, which the derived `Debug` did not.
    #[test]
    fn debug_redacts_the_password() {
        let url = ConnectionUrl::parse(
            "postgres://user:hunter2@db:5432/corpus?sslmode=disable&connect_timeout=10",
        )
        .expect("parses");
        let shown = format!("{url:?}");
        assert!(!shown.contains("hunter2"), "{shown}");
        assert!(shown.contains("password: Some(***)"), "{shown}");

        // And the fields somebody reaches for `{:?}` to see are all still there,
        // including the port the URL stated and the parameters after it.
        for expected in [
            "ConnectionUrl",
            "scheme: Postgres",
            "host: \"db\"",
            "port: 5432",
            "user: \"user\"",
            "database: \"corpus\"",
            "sslmode",
            "connect_timeout",
        ] {
            assert!(
                shown.contains(expected),
                "{expected} is missing from {shown}"
            );
        }
    }

    /// A URL with no password says so rather than saying it is redacted.
    #[test]
    fn debug_says_none_when_there_is_no_password() {
        let url = ConnectionUrl::parse("postgres://user@db:5432/corpus").expect("parses");
        let shown = format!("{url:?}");
        assert!(shown.contains("password: None"), "{shown}");
    }

    /// A URL with no password shows no colon at all, rather than an empty one.
    #[test]
    fn display_of_a_passwordless_url_has_no_separator() {
        let url = ConnectionUrl::parse("mysql://root@127.0.0.1/app").expect("parses");
        assert_eq!(url.to_string(), "mysql://root@127.0.0.1:3306/app");
    }

    /// A URL naming a host across a network gets verified TLS with nothing
    /// said, which is the default this replaced.
    ///
    /// The rule before this accepted an absent `sslmode` and connected in the
    /// clear, so a password and then every row crossed the network with
    /// nothing recorded about it anywhere.
    #[test]
    fn a_remote_url_that_says_nothing_gets_verified_tls() {
        let url = ConnectionUrl::parse("postgres://u@db.example:5432/d").expect("parses");
        assert_eq!(
            url.transport(false).expect("decides"),
            Transport::VerifiedTls
        );
    }

    /// `sslmode=require` is honoured rather than refused.
    #[test]
    fn sslmode_require_asks_for_verified_tls() {
        for written in [
            "postgres://u@h/d?sslmode=require",
            "postgres://u@h/d?sslmode=verify-full",
            "mysql://u@h/d?ssl-mode=REQUIRED",
        ] {
            let url = ConnectionUrl::parse(written).expect("parses");
            assert_eq!(
                url.transport(false).expect("decides"),
                Transport::VerifiedTls,
                "{written}"
            );
        }
    }

    /// **`sslmode=disable` alone is not enough**, and neither is the flag
    /// alone. Both are needed, because each one on its own is something
    /// somebody types without meaning it.
    #[test]
    fn plaintext_to_a_remote_host_needs_both_halves() {
        let url =
            ConnectionUrl::parse("postgres://u@db.example/d?sslmode=disable").expect("parses");
        let refused = url.transport(false).expect_err("refuses");
        assert!(
            refused.message().contains("--insecure-plaintext"),
            "{}",
            refused.message()
        );
        assert_eq!(url.transport(true).expect("permits"), Transport::Plaintext);

        let unstated = ConnectionUrl::parse("postgres://u@db.example/d").expect("parses");
        let refused = unstated.transport(true).expect_err("refuses");
        assert!(
            refused.message().contains("sslmode=disable"),
            "{}",
            refused.message()
        );
    }

    /// A mode that means "encrypt if the server feels like it" is refused.
    ///
    /// The alternative is a connection whose security is decided by a server
    /// setting nobody in the migration can see, reported nowhere.
    #[test]
    fn a_mode_that_permits_a_silent_downgrade_is_refused() {
        for written in ["prefer", "allow", "preferred"] {
            let url = ConnectionUrl::parse(&format!("postgres://u@h/d?sslmode={written}"))
                .expect("parses");
            let refused = url.transport(false).expect_err("refuses");
            assert!(
                refused.message().contains("require"),
                "{written}: {}",
                refused.message()
            );
        }
    }

    /// A loopback address is plaintext with no flag, because nothing leaves
    /// the machine - and the decision is made from the address, not the name.
    #[test]
    fn loopback_is_the_one_address_plaintext_needs_no_flag_for() {
        for written in [
            "postgres://u@127.0.0.1:5432/d",
            "postgres://u@[::1]:5432/d",
            "mysql://u@localhost/d",
        ] {
            let url = ConnectionUrl::parse(written).expect("parses");
            assert_eq!(
                url.transport(false).expect("decides"),
                Transport::Plaintext,
                "{written}"
            );
        }
        // A name that merely begins with the loopback name is not loopback.
        let url = ConnectionUrl::parse("postgres://u@localhost.evil.example/d").expect("parses");
        assert_eq!(
            url.transport(false).expect("decides"),
            Transport::VerifiedTls
        );
    }

    /// An `sslmode` this client does not know is refused rather than
    /// defaulted, because every way of defaulting it wrongly ends the same.
    #[test]
    fn an_unknown_sslmode_is_refused() {
        let url = ConnectionUrl::parse("postgres://u@h/d?sslmode=maybe").expect("parses");
        assert!(url.transport(false).is_err());
    }

    /// The authority file is read from either protocol's own parameter name.
    #[test]
    fn a_named_authority_is_read_from_either_spelling() {
        let url = ConnectionUrl::parse("postgres://u@h/d?sslrootcert=/etc/ca.pem").expect("parses");
        assert_eq!(url.root_certificate(), Some("/etc/ca.pem"));
        let url = ConnectionUrl::parse("mysql://u@h/d?ssl-ca=/etc/ca.pem").expect("parses");
        assert_eq!(url.root_certificate(), Some("/etc/ca.pem"));
        let url = ConnectionUrl::parse("mysql://u@h/d").expect("parses");
        assert_eq!(url.root_certificate(), None);
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

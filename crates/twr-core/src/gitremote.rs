//! Git remote URLs (a repository's `origin`): which ones twr clones on a box, how one is shown,
//! and when two name the same repository (`twr box project`, `twr runner --find-repo` and
//! `--clone-repo`).
//!
//! **Accepted** ([`RemoteUrl::parse`]): `https://host[:port]/path`, `ssh://[user@]host[:port]/path`
//! (`git+ssh://` and `ssh+git://`, git's other names for it, too) and scp-like
//! `[user@]host:path`. Refused: a URL with a password or a token in it (any user info of an
//! `https://` URL, a `user:password@` of an ssh one; an ssh user such as `git@` is fine), an
//! `@` after the host (part of a password with a `/` in it, maybe), more than one `@` before
//! the host (one of them in a password, maybe: which ends it is anyone's guess), a query or
//! fragment, a control character or a space, a leading `-` (an option to whatever reads it),
//! a remote helper (`ext::`, `fd::`, any `<name>::`), `file://`, a local path (no
//! `:`, a `/` before the first `:`, or a leading `/`, `.` or `~`) and every other scheme
//! (`http://`, `git://`, ...). A host is letters, digits, `-`, `.` and `_` (not starting with
//! `-` or `.`), or an IPv6 address in brackets (not in an scp-like URL: write it as `ssh://`).
//!
//! **Shown** redacted: [`redact`] takes the credentials out of any string a URL could be in
//! (one that does not parse included: whatever it cannot tell from credentials goes with
//! them), and a parsed URL holds none ([`RemoteUrl::redacted`]). A refusal's message shows
//! the text only so, [`redact`]ed whole: why it is refused names no part of it (the parse's
//! reasons are `&'static str`s).
//!
//! **Compared** by [`RemoteUrl::identity`], which holds no credentials: the scheme's family
//! (`https` or ssh), the host in lower case, the port (the family's default, 443 or 22, as
//! none), and the path with trailing `/` and `.git` dropped. An scp-like path that does not
//! start with `/` is relative to the ssh user's home directory, as is an `ssh://` path starting
//! with `/~`: such a path stays distinct from an absolute one (`git@host:repo` is
//! `ssh://git@host/~/repo`, not `ssh://git@host/repo`), and its identity holds the user (two
//! users' homes differ). Only on github.com, gitlab.com and bitbucket.org, where every form
//! reaches one repository, do the family, the user and that distinction not count:
//! `git@github.com:o/r` is `ssh://git@github.com/o/r` is `https://github.com/o/r.git`.

use std::fmt;

/// Hosts where an scp-like path, an `ssh://` path and an `https://` path of the same text name
/// one repository whatever the user ([`RemoteUrl::identity`]).
const ALIASED_HOSTS: &[&str] = &["github.com", "gitlab.com", "bitbucket.org"];

/// How a remote is reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Family {
    /// `https://`.
    Https,
    /// `ssh://` or scp-like.
    Ssh,
}

/// How a URL was written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Form {
    Https,
    /// `ssh://` (or `git+ssh://`, `ssh+git://`: [`RemoteUrl::scheme`]).
    SshUrl,
    /// `[user@]host:path`.
    Scp,
}

/// A remote URL that parsed ([`RemoteUrl::parse`]), without credentials.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteUrl {
    form: Form,
    /// The scheme as written (`https`, `ssh`, `git+ssh`, `ssh+git`); empty for scp-like.
    scheme: String,
    /// The ssh user (never for `https://`).
    user: Option<String>,
    /// As written (an IPv6 address with its brackets).
    host: String,
    port: Option<u16>,
    /// `https://` and `ssh://`: from the `/` after the host on; scp-like: what follows the `:`.
    path: String,
    /// The text parsed held credentials, dropped here ([`RemoteUrl::parse_lenient`]).
    had_credentials: bool,
}

/// What two URLs must share to name one repository ([`RemoteUrl::identity`]).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Identity {
    /// `None` on an aliased host (any family).
    family: Option<Family>,
    host: String,
    port: Option<u16>,
    /// The ssh user, for a path relative to its home only.
    user: Option<String>,
    /// Relative to the ssh user's home (`~/` dropped).
    relative: bool,
    path: String,
}

impl RemoteUrl {
    /// `s` as a URL twr may hand to `git clone` (see the module docs), or why not (the URL in
    /// the message [`redact`]ed).
    pub fn parse(s: &str) -> Result<RemoteUrl, String> {
        let u = RemoteUrl::parse_lenient(s)?;
        if u.had_credentials {
            return Err(format!(
                "{} holds a password or a token: twr does not pass credentials around (on a box, \
                 git uses the box's own: an ssh key, a credential helper)",
                redact(s)
            ));
        }
        Ok(u)
    }

    /// [`RemoteUrl::parse`], but a URL with credentials parses too, without them
    /// ([`RemoteUrl::had_credentials`]): for comparing a repository's `origin` as it is.
    pub fn parse_lenient(s: &str) -> Result<RemoteUrl, String> {
        parse(s).map_err(|why| format!("{:?}: {why}", redact(s)))
    }

    /// Whether the text parsed held credentials (only [`RemoteUrl::parse_lenient`] accepts it).
    pub fn had_credentials(&self) -> bool {
        self.had_credentials
    }

    /// `https` or ssh.
    pub fn family(&self) -> Family {
        match self.form {
            Form::Https => Family::Https,
            Form::SshUrl | Form::Scp => Family::Ssh,
        }
    }

    /// The host as written (an IPv6 address with its brackets).
    pub fn host(&self) -> &str {
        &self.host
    }

    /// The URL without credentials: what twr shows, logs, sends and clones.
    pub fn redacted(&self) -> String {
        let port = self.port.map(|p| format!(":{p}")).unwrap_or_default();
        let user = self
            .user
            .as_ref()
            .map(|u| format!("{u}@"))
            .unwrap_or_default();
        match self.form {
            Form::Https => format!("https://{}{port}{}", self.host, self.path),
            Form::SshUrl => format!("{}://{user}{}{port}{}", self.scheme, self.host, self.path),
            Form::Scp => format!("{user}{}:{}", self.host, self.path),
        }
    }

    /// What two URLs naming one repository share (see the module docs).
    pub fn identity(&self) -> Identity {
        let host = self.host.to_ascii_lowercase();
        let family = self.family();
        let port = self.port.filter(|p| {
            *p != match family {
                Family::Https => 443,
                Family::Ssh => 22,
            }
        });
        // Relative to the ssh user's home: scp-like without a leading `/`, or `ssh://host/~...`.
        let (relative, path) = match self.form {
            Form::Https => (false, self.path.as_str()),
            Form::SshUrl => match self.path.strip_prefix("/~") {
                Some(_) => (true, &self.path[1..]),
                None => (false, self.path.as_str()),
            },
            Form::Scp => (!self.path.starts_with('/'), self.path.as_str()),
        };
        let path = trim_path(path);
        if ALIASED_HOSTS.contains(&host.as_str()) {
            let path = path.trim_start_matches('/');
            return Identity {
                family: None,
                host,
                port,
                user: None,
                relative: false,
                path: path.strip_prefix("~/").unwrap_or(path).to_string(),
            };
        }
        let (user, path) = if relative {
            let path = path.strip_prefix("~/").unwrap_or(path);
            (self.user.clone(), path.to_string())
        } else {
            (None, path.to_string())
        };
        Identity {
            family: Some(family),
            host,
            port,
            user,
            relative,
            path,
        }
    }

    /// Whether `self` and `other` name one repository ([`RemoteUrl::identity`]).
    pub fn same_repository(&self, other: &RemoteUrl) -> bool {
        self.identity() == other.identity()
    }
}

impl fmt::Display for RemoteUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.redacted())
    }
}

/// `path` without trailing `/`, then a trailing `.git`, then trailing `/` again.
fn trim_path(path: &str) -> &str {
    let p = path.trim_end_matches('/');
    let p = p.strip_suffix(".git").unwrap_or(p);
    p.trim_end_matches('/')
}

/// [`RemoteUrl::parse_lenient`]'s parse: why `s` is refused, unprefixed. The reasons are
/// literals so that none can quote a part of `s` (a part of a password, split where twr could
/// not tell it from the host): [`RemoteUrl::parse_lenient`] shows `s` only [`redact`]ed whole.
fn parse(s: &str) -> Result<RemoteUrl, &'static str> {
    if s.is_empty() {
        return Err("an empty URL");
    }
    if s.chars().any(char::is_control) {
        return Err("it holds a control character");
    }
    if s.chars().any(char::is_whitespace) {
        return Err("it holds a space");
    }
    if s.starts_with('-') {
        return Err("it starts with `-` (git would take it for an option)");
    }
    if s.contains(['?', '#']) {
        return Err("it has a query or a fragment (`?`, `#`), which can hold a token");
    }
    // git's own test for a remote helper (`transport_get`): scheme characters, then `::`.
    let helper = s
        .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')))
        .is_some_and(|i| i > 0 && s[i..].starts_with("::"));
    if helper {
        return Err(
            "a remote helper (`<name>::`, as `ext::` runs a command): only https://, ssh:// \
             and user@host:path URLs",
        );
    }
    match s.find("://") {
        Some(i) if is_scheme(&s[..i]) => parse_url(&s[..i], &s[i + 3..]),
        // (Not shown: it may be credentials, as in `me:pw://x@host:path`.)
        Some(_) => Err("what precedes its `://` is not a URL scheme"),
        None => parse_scp(s),
    }
}

/// Why a URL with an `@` after its host is refused: a password holding a `/` (or a second `@`)
/// would leave part of itself there, and twr cannot tell which part of the text is which.
const AT_IN_PATH: &str =
    "an `@` after the host, which twr cannot tell from a password holding `/` or `@`";

/// Why a URL with more than one `@` before its host is refused: a password holding an `@`
/// leaves part of itself on either side of each, and twr cannot tell which `@` ends it.
const AT_TWICE: &str = "more than one `@` before the host, which twr cannot tell from a \
                        password holding `@`";

/// `<scheme>://<rest>`.
fn parse_url(scheme: &str, rest: &str) -> Result<RemoteUrl, &'static str> {
    let form = match scheme {
        "https" => Form::Https,
        "ssh" | "git+ssh" | "ssh+git" => Form::SshUrl,
        "file" => return Err("a file:// URL is a local path: only https:// and ssh:// URLs"),
        _ => {
            return Err(
                "its scheme is not one twr clones from: only https://, ssh:// and \
                 user@host:path URLs",
            )
        }
    };
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    // Before the host is read: `https://user:pa/ss@host/r` would be host `user`, port `pa`.
    if path.contains('@') {
        return Err(AT_IN_PATH);
    }
    if authority.matches('@').count() > 1 {
        return Err(AT_TWICE);
    }
    let mut had_credentials = false;
    let (userinfo, hostport) = match authority.find('@') {
        Some(i) => (Some(&authority[..i]), &authority[i + 1..]),
        None => (None, authority),
    };
    let user = match (form, userinfo) {
        (_, None) => None,
        (Form::Https, Some(_)) => {
            had_credentials = true;
            None
        }
        (_, Some(info)) => {
            let user = match info.split_once(':') {
                Some((user, _)) => {
                    had_credentials = true;
                    user
                }
                None => info,
            };
            check_user(user)?;
            Some(user.to_string())
        }
    };
    let (host, port) = split_host_port(hostport)?;
    if path.trim_matches('/').is_empty() {
        return Err("it names no repository (no path after the host)");
    }
    Ok(RemoteUrl {
        form,
        scheme: scheme.to_string(),
        user,
        host,
        port,
        path: path.to_string(),
        had_credentials,
    })
}

/// `[user@]host:path`, as git tells one from a local path: a `:` with no `/` before it.
fn parse_scp(s: &str) -> Result<RemoteUrl, &'static str> {
    const LOCAL: &str = "a local path, not a URL: only https://, ssh:// and user@host:path URLs";
    if s.starts_with(['/', '.', '~']) {
        return Err(LOCAL);
    }
    let Some(colon) = s.find(':') else {
        return Err(LOCAL);
    };
    if s[..colon].contains('/') {
        return Err(LOCAL);
    }
    let (head, mut path) = (&s[..colon], &s[colon + 1..]);
    if head.starts_with('[') {
        return Err("a host in brackets: write the URL as ssh://[host]/path");
    }
    if head.matches('@').count() > 1 {
        return Err(AT_TWICE);
    }
    let (mut user, mut host) = match head.find('@') {
        Some(i) => (Some(&head[..i]), &head[i + 1..]),
        None => (None, head),
    };
    // `user:password@host:path`: git would take `user` for the host, and the rest for a path.
    let mut had_credentials = false;
    let first = path.split('/').next().unwrap_or_default();
    if let Some(at) = first.find('@') {
        had_credentials = true;
        if let Some(c) = path[at + 1..].find(':') {
            // One `@` before the host's `:` in all: not one in the user, nor another after the
            // first one of the password (which part of `one@two@host` is the host?).
            let h = &path[at + 1..at + 1 + c];
            if head.contains('@') || h.contains('@') {
                return Err(AT_TWICE);
            }
            user = Some(head);
            host = h;
            path = &path[at + 1 + c + 1..];
        }
    }
    if path.contains('@') {
        return Err(AT_IN_PATH);
    }
    if let Some(u) = user {
        check_user(u)?;
    }
    check_host(host)?;
    if path.trim_matches('/').is_empty() {
        return Err("it names no repository (no path after the `:`)");
    }
    if path.starts_with('-') {
        return Err("its path starts with `-`");
    }
    Ok(RemoteUrl {
        form: Form::Scp,
        scheme: String::new(),
        user: user.map(str::to_string),
        host: host.to_string(),
        port: None,
        path: path.to_string(),
        had_credentials,
    })
}

/// An ssh user: not empty, no `-` first (an option to ssh), no `/`, `:` or `@`.
fn check_user(user: &str) -> Result<(), &'static str> {
    if user.is_empty() {
        return Err("an empty user before `@`");
    }
    if user.starts_with('-') || user.contains(['/', ':', '@']) {
        return Err(
            "the user is not an ssh user name (one starting with `-`, or holding `/`, `:` or `@`)",
        );
    }
    Ok(())
}

/// `host[:port]`, or `[ipv6][:port]`.
fn split_host_port(hostport: &str) -> Result<(String, Option<u16>), &'static str> {
    let (host, port) = if let Some(inner) = hostport.strip_prefix('[') {
        let Some(end) = inner.find(']') else {
            return Err("an unclosed `[` in the host");
        };
        let addr = &inner[..end];
        if addr.is_empty()
            || !addr
                .chars()
                .all(|c| c.is_ascii_hexdigit() || c == ':' || c == '.')
        {
            return Err("the host in brackets is not an IPv6 address");
        }
        let after = &inner[end + 1..];
        let port = match after {
            "" => None,
            _ => Some(
                after
                    .strip_prefix(':')
                    .ok_or("text after the `]` of the host")?,
            ),
        };
        (format!("[{addr}]"), port)
    } else {
        match hostport.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), Some(p)),
            None => (hostport.to_string(), None),
        }
    };
    if !host.starts_with('[') {
        check_host(&host)?;
    }
    let port = match port {
        None => None,
        Some(p) => match p.parse::<u16>() {
            Ok(n) if n > 0 && p.chars().all(|c| c.is_ascii_digit()) => Some(n),
            _ => return Err("the port is not a port number (1 to 65535)"),
        },
    };
    Ok((host, port))
}

/// A host name: letters, digits, `-`, `.` and `_`, not starting with `-` or `.`.
fn check_host(host: &str) -> Result<(), &'static str> {
    if host.is_empty() {
        return Err("no host");
    }
    if host.starts_with(['-', '.'])
        || !host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_'))
    {
        return Err(
            "the host is not a host name (letters, digits, `-`, `.` and `_`, not starting with \
             `-` or `.`)",
        );
    }
    Ok(())
}

/// `s` (a URL, or anything a remote could be set to) without the credentials it may hold, and
/// with each control character as U+FFFD: what twr shows or logs of a URL it did not parse,
/// or that [`RemoteUrl::parse`] refused.
///
/// The credentials go first, then the query: since a password may hold any character (`?`,
/// `#`, `/`, `@`), everything up to the **last** `@` counts as user info, wherever it is
/// (after a `scheme://`; in any other string, when what precedes that `@` holds a `:`). So a
/// string that does not parse is redacted too much rather than too little: an `@` in a path
/// takes the host with it (`https://h/a@b` is `https://***@b`), which [`RemoteUrl::parse`]
/// refuses anyway. That user info keeps only an ssh user: `ssh://git:***@host` (the part
/// before the first `:`, when it is a plain user name), `ssh://git@host` (no `:`), and
/// `user:***@host:path` for an scp-like one; any other, an `https://` one included, is `***`
/// (as is all before the last `@` of an scp-like one with no `:` there but another `@`:
/// `***@host:path`). Then, in what follows, a query is dropped (`?***`), and so is anything
/// after a `#`.
pub fn redact(s: &str) -> String {
    let s: String = s
        .chars()
        .map(|c| if c.is_control() { '\u{fffd}' } else { c })
        .collect();
    // Only after a scheme ([`is_scheme`]): else what precedes a `://` may itself be credentials
    // (`me:pw://x@host:path`).
    let url = s.find("://").filter(|&i| is_scheme(&s[..i]));
    let (head, info, tail) = match url {
        Some(i) => {
            let scheme = &s[..i];
            let rest = &s[i + 3..];
            match rest.rfind('@') {
                Some(at) => {
                    let ssh = matches!(scheme, "ssh" | "git+ssh" | "ssh+git");
                    let info = match rest[..at].split_once(':') {
                        Some((user, _)) if ssh && plain_user(user) => format!("{user}:***"),
                        None if ssh && plain_user(&rest[..at]) => rest[..at].to_string(),
                        _ => "***".to_string(),
                    };
                    (format!("{scheme}://"), info, &rest[at..])
                }
                None => (format!("{scheme}://"), String::new(), rest),
            }
        }
        // scp-like `user:password@host:path`, or any other string.
        None => match s.rfind('@') {
            Some(at) if s[..at].contains(':') => {
                let user = s[..at].split(':').next().unwrap_or_default();
                let user = if plain_user(user) { user } else { "***" };
                (String::new(), format!("{user}:***"), &s[at..])
            }
            Some(at) if s[..at].contains('@') => (String::new(), "***".to_string(), &s[at..]),
            _ => (String::new(), String::new(), s.as_str()),
        },
    };
    let tail = match tail.find(['?', '#']) {
        Some(i) => format!("{}?***", &tail[..i]),
        None => tail.to_string(),
    };
    format!("{head}{info}{tail}")
}

/// A URL scheme as git reads one: a letter, then letters, digits, `+`, `-` and `.`.
fn is_scheme(s: &str) -> bool {
    s.starts_with(|c: char| c.is_ascii_alphabetic())
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

/// A user name [`redact`] may show: what [`check_user`] takes, without `?`, `#` or a space.
fn plain_user(user: &str) -> bool {
    check_user(user).is_ok() && !user.contains(['?', '#', ' '])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(s: &str) -> Identity {
        RemoteUrl::parse(s)
            .unwrap_or_else(|e| panic!("{s}: {e}"))
            .identity()
    }

    fn refused(s: &str) -> String {
        match RemoteUrl::parse(s) {
            Ok(u) => panic!("{s:?} parsed: {u:?}"),
            Err(e) => e,
        }
    }

    #[test]
    fn accepts_https_ssh_and_scp_like() {
        for (s, family, host) in [
            ("https://github.com/o/r.git", Family::Https, "github.com"),
            (
                "https://git.example.com:8443/team/r",
                Family::Https,
                "git.example.com",
            ),
            (
                "ssh://git@host.example/srv/r.git",
                Family::Ssh,
                "host.example",
            ),
            ("ssh://host:2222/r", Family::Ssh, "host"),
            ("git+ssh://me@h/r", Family::Ssh, "h"),
            ("ssh+git://me@h/r", Family::Ssh, "h"),
            ("ssh://[::1]:22/r", Family::Ssh, "[::1]"),
            ("https://[2001:db8::1]/r", Family::Https, "[2001:db8::1]"),
            ("git@github.com:o/r.git", Family::Ssh, "github.com"),
            ("host:repo", Family::Ssh, "host"),
            ("me@my_host-1.lan:/abs/r", Family::Ssh, "my_host-1.lan"),
        ] {
            let u = RemoteUrl::parse(s).unwrap_or_else(|e| panic!("{s}: {e}"));
            assert_eq!((u.family(), u.host()), (family, host), "{s}");
            // Nothing to redact: shown as written.
            assert_eq!(u.redacted(), s);
            assert_eq!(u.to_string(), s);
            assert!(!u.had_credentials());
        }
    }

    #[test]
    fn refuses_what_twr_does_not_clone() {
        for (s, says) in [
            ("", "empty"),
            ("file:///srv/r.git", "file://"),
            ("/srv/r.git", "local path"),
            ("./r", "local path"),
            ("../r", "local path"),
            ("~/r", "local path"),
            ("r", "local path"),
            ("dir/sub:x", "local path"),
            ("ext::sh -c touch% /tmp/x", "space"),
            ("ext::sh", "remote helper"),
            ("fd::3", "remote helper"),
            ("persistent-https::https://h/r", "remote helper"),
            ("http://h/r", "scheme is not one twr clones"),
            ("git://h/r", "scheme is not one twr clones"),
            ("ftp://h/r", "scheme is not one twr clones"),
            ("HTTPS://h/r", "scheme is not one twr clones"),
            ("-uhttps://h/r", "starts with `-`"),
            ("--upload-pack=x", "starts with `-`"),
            ("https://h/r\n", "control"),
            ("https://h/\u{1b}[2Jr", "control"),
            ("https://h/my repo", "space"),
            ("https://h/r?x=1", "query"),
            ("https://h/r#frag", "query"),
            ("https://h", "no path"),
            ("https://h/", "no path"),
            ("host:", "no path"),
            ("ssh://-oProxyCommand=x/r", "not a host"),
            ("ssh://-x@h/r", "not an ssh user"),
            ("ssh://@h/r", "empty user"),
            ("@h:r", "empty user"),
            ("-h:r", "starts with `-`"),
            ("h:-r", "path starts with `-`"),
            ("ho$t:r", "not a host"),
            ("ssh://h:x/r", "not a port"),
            ("ssh://h:0/r", "not a port"),
            ("ssh://h:70000/r", "not a port"),
            ("ssh://h:/r", "not a port"),
            ("[::1]:r", "brackets"),
            ("ssh://[zz]/r", "not an IPv6"),
            ("ssh://[::1/r", "unclosed"),
        ] {
            let e = refused(s);
            assert!(e.contains(says), "{s:?}: {e}");
        }
    }

    #[test]
    fn credentials_are_refused_and_never_shown() {
        for (s, shown_as) in [
            (
                "https://user:hunter2@github.com/o/r",
                "https://***@github.com/o/r",
            ),
            (
                "https://ghp_s3cret@github.com/o/r",
                "https://***@github.com/o/r",
            ),
            (
                "https://oauth2:s3cret@gitlab.com/o/r.git",
                "https://***@gitlab.com/o/r.git",
            ),
            ("ssh://git:hunter2@host/r", "ssh://git:***@host/r"),
            ("me:hunter2@host:r", "me:***@host:r"),
        ] {
            let e = refused(s);
            assert!(e.contains("password or a token"), "{s}: {e}");
            assert!(e.contains(shown_as), "{s}: {e}");
            assert!(!e.contains("hunter2") && !e.contains("s3cret"), "{e}");
            assert_eq!(redact(s), shown_as);
            // Compared without them.
            let u = RemoteUrl::parse_lenient(s).unwrap();
            assert!(u.had_credentials());
            assert!(!u.redacted().contains("hunter2") && !u.redacted().contains("s3cret"));
        }
        assert!(RemoteUrl::parse_lenient("https://tok@github.com/o/r")
            .unwrap()
            .same_repository(&RemoteUrl::parse("git@github.com:o/r.git").unwrap()));
        assert_eq!(
            RemoteUrl::parse_lenient("me:pw@host:r").unwrap().redacted(),
            "me@host:r"
        );
        // An ssh user is not a credential.
        assert_eq!(redact("ssh://git@host/r"), "ssh://git@host/r");
        assert_eq!(redact("git@host:r"), "git@host:r");
        // Refused URLs are redacted in their errors too.
        let e = refused("http://u:hunter2@h/r");
        assert!(!e.contains("hunter2"), "{e}");
        let e = refused("https://u:hunter2@h/r?token=s3cret");
        assert!(!e.contains("hunter2") && !e.contains("s3cret"), "{e}");
        // Whatever the string.
        assert_eq!(
            redact("https://h/r?access_token=s3cret#x"),
            "https://h/r?***"
        );
        assert_eq!(redact("x\ny\u{7}"), "x\u{fffd}y\u{fffd}");
        assert_eq!(redact("/local/path"), "/local/path");
        assert_eq!(redact("weird://a:b@c/d"), "weird://***@c/d");
    }

    /// A `?`, `#`, `/` or `@` inside the credentials does not hide them from [`redact`]: what
    /// cannot be told from them goes with them, in every form and every message.
    #[test]
    fn credentials_holding_url_punctuation_are_redacted() {
        for (s, shown_as) in [
            (
                "https://user:visible-secret?tail@host/repo",
                "https://***@host/repo",
            ),
            ("https://user:sec#ret@host/repo", "https://***@host/repo"),
            ("https://tok?en#x@host/r?q=1#f", "https://***@host/r?***"),
            ("https://user:pa/ss@host/repo", "https://***@host/repo"),
            ("https://user:1234/5678@host/r", "https://***@host/r"),
            ("https://u:p@q/r@host/x", "https://***@host/x"),
            ("ssh://git:se?cret@host/r", "ssh://git:***@host/r"),
            ("ssh://git:se#cret@host/r", "ssh://git:***@host/r"),
            ("ssh://to?ken@host/r", "ssh://***@host/r"),
            ("ssh://g?t:secret@host/r", "ssh://***@host/r"),
            ("me:visible-secret?tail@host:r", "me:***@host:r"),
            ("me:sec#ret@host:r", "me:***@host:r"),
            ("me:pa/ss@host:r", "me:***@host:r"),
            ("m?e:secret@host:r", "***:***@host:r"),
            // Not a scheme: what precedes the `://` may be credentials.
            ("me:secret://x@host:r", "me:***@host:r"),
        ] {
            assert_eq!(redact(s), shown_as, "{s}");
            // Refused, both ways, the message redacted.
            for e in [refused(s), RemoteUrl::parse_lenient(s).unwrap_err()] {
                assert!(e.contains(&format!("{shown_as:?}")), "{s}: {e}");
                for secret in ["visible", "secret", "sec#", "pa/ss", "1234", "5678", "tok?"] {
                    assert!(!e.contains(secret), "{s}: {e}");
                }
            }
        }
        // An `@` in a path takes the host with it (refused anyway).
        assert_eq!(redact("https://h/a@b"), "https://***@b");
        assert!(refused("https://h/a@b").contains("an `@` after the host"));
        assert!(refused("git@host:o/r@v").contains("an `@` after the host"));
        assert!(refused("ssh://git@host/r@v").contains("an `@` after the host"));
        // Parsed, a URL's own text holds none.
        let u = RemoteUrl::parse_lenient("me:hunter2@host:r").unwrap();
        assert_eq!(u.redacted(), "me@host:r");
    }

    /// A password holding `@` (and `:`, `?`, `#`, `/`) is refused, and no part of it is in a
    /// message: twr cannot tell which `@` ends it, so the reasons quote nothing of the text.
    #[test]
    fn several_at_signs_are_ambiguous_and_never_shown() {
        const TWICE: &str = "more than one `@` before the host";
        for (s, shown_as, why) in [
            ("me:SEC1@SEC2@host:repo", "me:***@host:repo", TWICE),
            ("me:SEC1@SEC2@SEC3@host:repo", "me:***@host:repo", TWICE),
            ("me:SEC1:SEC2@SEC3@host:r", "me:***@host:r", TWICE),
            ("SEC1@SEC2@host:r", "***@host:r", TWICE),
            ("a@SEC1:SEC2@host:r", "***:***@host:r", TWICE),
            ("https://u:SEC1@SEC2@host/r", "https://***@host/r", TWICE),
            (
                "https://SEC1@SEC2@host:8443/r",
                "https://***@host:8443/r",
                TWICE,
            ),
            (
                "https://u:SEC1:SEC2@SEC3@host/r",
                "https://***@host/r",
                TWICE,
            ),
            ("ssh://git:SEC1@SEC2@host/r", "ssh://git:***@host/r", TWICE),
            ("ssh://SEC1@SEC2@host/r", "ssh://***@host/r", TWICE),
            (
                "git+ssh://git:SEC1@SEC2@host:22/r",
                "git+ssh://git:***@host:22/r",
                TWICE,
            ),
            // With a `/` in it too: what follows it is a path.
            (
                "me:SEC1/SEC2@SEC3@host:r",
                "me:***@host:r",
                "an `@` after the host",
            ),
            (
                "https://u:SEC1@SEC2/SEC3@host/r",
                "https://***@host/r",
                "an `@` after the host",
            ),
            (
                "ssh://git:SEC1/SEC2@SEC3@host/r",
                "ssh://git:***@host/r",
                "an `@` after the host",
            ),
            // A `?` or a `#`: a query, refused first.
            ("me:SEC1?SEC2@SEC3@host:r", "me:***@host:r", "query"),
            ("me:SEC1#SEC2@SEC3@host:r", "me:***@host:r", "query"),
            (
                "https://u:SEC1?@SEC2#@host/r?q",
                "https://***@host/r?***",
                "query",
            ),
            (
                "ssh://git:SEC1#SEC2@SEC3@host/r",
                "ssh://git:***@host/r",
                "query",
            ),
        ] {
            assert_eq!(redact(s), shown_as, "{s}");
            for e in [refused(s), RemoteUrl::parse_lenient(s).unwrap_err()] {
                assert!(e.contains(why), "{s}: {e}");
                assert!(e.starts_with(&format!("{shown_as:?}: ")), "{s}: {e}");
                assert!(!e.contains("SEC"), "{s}: {e}");
            }
        }
        // One `@` with a `:` in the password parses (leniently), without it.
        let u = RemoteUrl::parse_lenient("me:SEC1:SEC2@host:r").unwrap();
        assert_eq!(u.redacted(), "me@host:r");
        assert!(refused("me:SEC1:SEC2@host:r").starts_with("me:***@host:r holds a password"));
        // A reason never quotes the text, whatever is wrong with it: the text is shown once,
        // redacted whole.
        for (s, why) in [
            ("ssh://h:SEC1/r", "not a port"),
            ("ssh://[SEC1]/r", "not an IPv6"),
            ("ssh://-SEC1@h/r", "not an ssh user"),
            ("ho$tSEC1:r", "not a host"),
            ("ssh://h$SEC1/r", "not a host"),
            ("SEC1://h/r", "scheme is not one"),
        ] {
            let e = refused(s);
            assert!(e.contains(why), "{s}: {e}");
            let shown = format!("{:?}: ", redact(s));
            assert!(e.starts_with(&shown), "{s}: {e}");
            assert!(!e[shown.len()..].contains("SEC1"), "{s}: {e}");
        }
    }

    #[test]
    fn identity_drops_case_ports_and_suffixes() {
        assert_eq!(
            id("https://Git.Example.COM/o/r.git/"),
            id("https://git.example.com/o/r")
        );
        assert_eq!(id("https://h:443/r"), id("https://h/r"));
        assert_ne!(id("https://h:8443/r"), id("https://h/r"));
        assert_eq!(id("ssh://h:22/srv/r"), id("ssh://h/srv/r"));
        assert_ne!(id("ssh://h:2222/srv/r"), id("ssh://h/srv/r"));
        assert_ne!(id("https://h/srv/r"), id("ssh://h/srv/r"));
        assert_ne!(id("https://h/o/r"), id("https://h/o/s"));
        assert_ne!(id("https://h/o/r"), id("https://other/o/r"));
        // The user does not count for an absolute path.
        assert_eq!(id("ssh://git@h/srv/r"), id("ssh://h/srv/r.git"));
        assert_eq!(id("me@h:/srv/r"), id("ssh://git@h/srv/r"));
        assert_eq!(id("git+ssh://h/srv/r"), id("ssh://h/srv/r"));
    }

    #[test]
    fn scp_relative_paths_stay_relative_on_a_generic_host() {
        // `git@host:repo` is the user's `~/repo`, not `/repo`.
        assert_ne!(id("git@host:repo"), id("ssh://git@host/repo"));
        assert_ne!(id("git@host:repo.git"), id("ssh://git@host/repo.git"));
        // `ssh://host/~/repo` is the user's home again.
        assert_eq!(id("git@host:repo"), id("ssh://git@host/~/repo"));
        assert_eq!(id("git@host:~/repo"), id("git@host:repo"));
        // Two users' homes differ.
        assert_ne!(id("alice@host:repo"), id("bob@host:repo"));
        assert_ne!(id("https://host/repo"), id("git@host:repo"));
    }

    #[test]
    fn hosting_services_alias_their_forms() {
        for host in ["github.com", "gitlab.com", "bitbucket.org", "GitHub.com"] {
            let lower = host.to_ascii_lowercase();
            let forms = [
                format!("git@{host}:o/r.git"),
                format!("ssh://git@{host}/o/r"),
                format!("ssh://{host}:22/o/r.git/"),
                format!("https://{host}/o/r.git"),
                format!("https://{lower}/o/r/"),
                format!("{host}:o/r"),
            ];
            for f in &forms {
                assert_eq!(id(f), id(&forms[0]), "{f}");
            }
            assert_ne!(id(&format!("git@{host}:o/other")), id(&forms[0]));
            assert_ne!(id(&format!("https://{host}:8443/o/r")), id(&forms[0]));
        }
        // Only those hosts.
        assert_ne!(
            id("git@github.example:o/r"),
            id("https://github.example/o/r")
        );
        assert_ne!(
            id("git@git.github.com:o/r"),
            id("ssh://git@git.github.com/o/r")
        );
    }
}

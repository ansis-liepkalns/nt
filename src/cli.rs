//! Command-line parsing: a pure function from arguments to a [`Cmd`], so it can be tested as a table.

use lexopt::prelude::*;

pub const HELP: &str = "\
nt — reconnectable terminal sessions

  nt [<host>]          start a new session (on <host> via ssh)
  nt -s [<host>]       open the session list
  nt ls [<host>]       print sessions
  nt restart [<host>]  restart the nt server after upgrading (ends sessions)

Inside nt:  C-] C-] sessions · C-] d detach · C-] ? keys
Config:     ~/.config/nt/config.toml   ssh port: nt -p 2200 <host>
            other ssh options: nt <host> -- -J jump
";

#[derive(Debug, PartialEq, Eq)]
pub enum Cmd {
    Help,
    Version,
    /// `nt [<host>]` (new session) or `nt -s [<host>]` (session list).
    Session { dest: Option<String>, ssh: Vec<String>, switcher: bool },
    Ls { dest: Option<String>, ssh: Vec<String> },
    Restart { dest: Option<String>, ssh: Vec<String> },
    Server { foreground: bool, restart: bool, port: Option<u16>, no_shell_integration: bool },
    Bootstrap,
    Namer { model: Option<String> },
    NameTest { files: Vec<String>, model: Option<String>, cwd: Option<String> },
}

/// Parse the arguments after the program name. The error is a one-line message.
pub fn parse<S: AsRef<str>>(args: &[S]) -> Result<Cmd, String> {
    let mut args: Vec<&str> = args.iter().map(|a| a.as_ref()).collect();
    // Everything after `--` belongs to ssh.
    let mut ssh: Vec<String> = match args.iter().position(|a| *a == "--") {
        Some(i) => args.split_off(i).into_iter().skip(1).map(String::from).collect(),
        None => Vec::new(),
    };
    let mut p = lexopt::Parser::from_args(args);
    let (mut sessions, mut help, mut version, mut foreground, mut restart, mut no_shell) = (false, false, false, false, false, false);
    let (mut port, mut ssh_port): (Option<u16>, Option<u16>) = (None, None);
    let (mut model, mut cwd): (Option<String>, Option<String>) = (None, None);
    let mut pos: Vec<String> = Vec::new();
    let e = |e: lexopt::Error| e.to_string();
    while let Some(arg) = p.next().map_err(e)? {
        match arg {
            Short('s') | Long("sessions") => sessions = true,
            Short('h') | Long("help") => help = true,
            Short('V') | Long("version") => version = true,
            Long("foreground") => foreground = true,
            Long("restart") => restart = true,
            Long("no-shell-integration") => no_shell = true,
            Short('p') => ssh_port = Some(p.value().map_err(e)?.parse().map_err(e)?),
            Long("port") => port = Some(p.value().map_err(e)?.parse().map_err(e)?),
            Long("model") => model = Some(p.value().map_err(e)?.string().map_err(e)?),
            Long("cwd") => cwd = Some(p.value().map_err(e)?.string().map_err(e)?),
            Value(v) => pos.push(v.string().map_err(e)?),
            Short(c) => return Err(format!("unknown option '-{c}'")),
            Long(l) => return Err(format!("unknown option '--{l}'")),
        }
    }
    let server_flags = foreground || no_shell || port.is_some();
    let name_flags = model.is_some() || cwd.is_some();
    let mut it = pos.into_iter();
    let first = it.next();
    let sub = first.as_deref().filter(|s| matches!(*s, "help" | "ls" | "restart" | "server" | "bootstrap" | "namer" | "name-test"));
    // Flags that only make sense for one subcommand.
    let plain = !server_flags && !name_flags && !restart;
    let ok = match sub {
        Some("server") => !sessions && !name_flags,
        Some("name-test") => !sessions && !server_flags && !restart,
        Some("namer") => !sessions && !server_flags && !restart && cwd.is_none(),
        Some(_) => !sessions && plain,
        None => plain,
    };
    let uses_ssh = matches!(sub, None | Some("ls") | Some("restart"));
    if !ok || (ssh_port.is_some() && !uses_ssh) {
        return Err("unexpected option for this command".into());
    }
    if let Some(n) = ssh_port {
        ssh.splice(0..0, ["-p".to_string(), n.to_string()]);
    }
    if help {
        return Ok(Cmd::Help);
    }
    if version {
        return Ok(Cmd::Version);
    }
    let one_host = |mut it: std::vec::IntoIter<String>| -> Result<Option<String>, String> {
        let h = it.next();
        match it.next() {
            Some(x) => Err(format!("unexpected argument '{x}'")),
            None if h.is_none() && ssh_port.is_some() => Err("-p needs a host".into()),
            None => Ok(h),
        }
    };
    let no_ssh = |c: Cmd| if ssh.is_empty() { Ok(c) } else { Err("'--' (ssh arguments) is not valid for this command".to_string()) };
    match sub {
        Some("help") => Ok(Cmd::Help),
        Some("ls") => Ok(Cmd::Ls { dest: one_host(it)?, ssh }),
        Some("restart") => Ok(Cmd::Restart { dest: one_host(it)?, ssh }),
        Some("server") => match it.next() {
            Some(x) => Err(format!("unexpected argument '{x}'")),
            None if !ssh.is_empty() => Err("'--' (ssh arguments) is not valid for this command".into()),
            None => Ok(Cmd::Server { foreground, restart, port, no_shell_integration: no_shell }),
        },
        Some("bootstrap") | Some("namer") => match it.next() {
            Some(x) => Err(format!("unexpected argument '{x}'")),
            None => no_ssh(if sub == Some("namer") { Cmd::Namer { model } } else { Cmd::Bootstrap }),
        },
        Some(_) => no_ssh(Cmd::NameTest { files: it.collect(), model, cwd }),
        None => {
            let dest = first;
            if let Some(x) = it.next() {
                return Err(format!("unexpected argument '{x}'"));
            }
            if dest.is_none() && ssh_port.is_some() {
                return Err("-p needs a host".into());
            }
            Ok(Cmd::Session { dest, ssh, switcher: sessions })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(dest: Option<&str>, ssh: &[&str], switcher: bool) -> Result<Cmd, String> {
        Ok(Cmd::Session { dest: dest.map(Into::into), ssh: ssh.iter().map(|s| s.to_string()).collect(), switcher })
    }

    #[test]
    fn table() {
        let ssh = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let cases: Vec<(&[&str], Result<Cmd, String>)> = vec![
            (&[], session(None, &[], false)),
            (&["box"], session(Some("box"), &[], false)),
            (&["me@box", "--", "-p", "2200"], session(Some("me@box"), &["-p", "2200"], false)),
            (&["-p", "9999", "me@box"], session(Some("me@box"), &["-p", "9999"], false)),
            (&["-p", "1", "box", "--", "-J", "j"], session(Some("box"), &["-p", "1", "-J", "j"], false)),
            (&["ls", "-p", "9999", "box"], Ok(Cmd::Ls { dest: Some("box".into()), ssh: ssh(&["-p", "9999"]) })),
            (&["restart", "-p", "9999", "box"], Ok(Cmd::Restart { dest: Some("box".into()), ssh: ssh(&["-p", "9999"]) })),
            (&["-s"], session(None, &[], true)),
            (&["--sessions", "box"], session(Some("box"), &[], true)),
            (&["box", "-s", "--", "-J", "j"], session(Some("box"), &["-J", "j"], true)),
            (&["ls"], Ok(Cmd::Ls { dest: None, ssh: vec![] })),
            (&["ls", "box", "--", "-p", "1"], Ok(Cmd::Ls { dest: Some("box".into()), ssh: ssh(&["-p", "1"]) })),
            (&["restart"], Ok(Cmd::Restart { dest: None, ssh: vec![] })),
            (&["restart", "box"], Ok(Cmd::Restart { dest: Some("box".into()), ssh: vec![] })),
            (&["-h"], Ok(Cmd::Help)),
            (&["--help"], Ok(Cmd::Help)),
            (&["help"], Ok(Cmd::Help)),
            (&["-V"], Ok(Cmd::Version)),
            (&["--version"], Ok(Cmd::Version)),
            (&["bootstrap"], Ok(Cmd::Bootstrap)),
            (&["namer"], Ok(Cmd::Namer { model: None })),
            (&["namer", "--model", "/m.gguf"], Ok(Cmd::Namer { model: Some("/m.gguf".into()) })),
            (&["server"], Ok(Cmd::Server { foreground: false, restart: false, port: None, no_shell_integration: false })),
            (
                &["server", "--foreground", "--port", "0", "--no-shell-integration"],
                Ok(Cmd::Server { foreground: true, restart: false, port: Some(0), no_shell_integration: true }),
            ),
            (&["server", "--restart"], Ok(Cmd::Server { foreground: false, restart: true, port: None, no_shell_integration: false })),
            (
                &["name-test", "a", "b", "--model", "m", "--cwd", "/x"],
                Ok(Cmd::NameTest { files: ssh(&["a", "b"]), model: Some("m".into()), cwd: Some("/x".into()) }),
            ),
            // removed or invalid
            (&["-n"], Err("unknown option '-n'".into())),
            (&["ls", "-l"], Err("unknown option '-l'".into())),
            (&["--bogus"], Err("unknown option '--bogus'".into())),
            (&["a", "b"], Err("unexpected argument 'b'".into())),
            (&["ls", "a", "b"], Err("unexpected argument 'b'".into())),
            (&["-s", "ls"], Err("unexpected option for this command".into())),
            (&["--foreground"], Err("unexpected option for this command".into())),
            (&["--restart"], Err("unexpected option for this command".into())),
            (&["ls", "--port", "1"], Err("unexpected option for this command".into())),
            (&["bootstrap", "--", "-p"], Err("'--' (ssh arguments) is not valid for this command".into())),
        ];
        for (args, want) in cases {
            assert_eq!(parse(args), want, "args {args:?}");
        }
        assert!(parse(&["-p"]).is_err());
        assert!(parse(&["-p", "x", "box"]).is_err());
        assert!(parse(&["-p", "70000", "box"]).is_err());
        assert!(parse(&["bootstrap", "-p", "1"]).is_err());
        assert!(parse(&["server", "-p", "1"]).is_err());
        assert!(parse(&["-p", "1"]).is_err());
        assert!(parse(&["-s", "-p", "1"]).is_err());
        assert!(parse(&["ls", "-p", "1"]).is_err());
        assert!(parse(&["restart", "-p", "1"]).is_err());
        assert!(parse(&["--port"]).is_err());
        assert!(parse(&["server", "--port", "x"]).is_err());
    }

    #[test]
    fn help_is_short() {
        assert!(HELP.lines().count() <= 12 && HELP.lines().all(|l| l.chars().count() <= 80));
    }
}

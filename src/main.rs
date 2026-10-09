mod cli;
mod client;
mod config;
mod proto;
mod server;

use anyhow::Result;
use cli::Cmd;

fn main() {
    let args: Vec<String> = match std::env::args_os().skip(1).map(|a| a.into_string()).collect() {
        Ok(a) => a,
        Err(_) => {
            eprintln!("nt: non-utf8 argument\nsee nt --help");
            std::process::exit(2);
        }
    };
    let cmd = match cli::parse(&args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("nt: {e}\nsee nt --help");
            std::process::exit(2);
        }
    };
    if let Err(e) = real_main(cmd) {
        eprintln!("nt: {e:#}");
        std::process::exit(1);
    }
}

fn real_main(cmd: Cmd) -> Result<()> {
    let mut cfg = config::Config::load();
    match cmd {
        Cmd::Help => print!("{}", cli::HELP),
        Cmd::Version => println!("nt {}", env!("CARGO_PKG_VERSION")),
        Cmd::Session { dest, ssh, switcher } => client::run(&cfg, client::Opts { switcher, dest, ssh_args: ssh })?,
        Cmd::Ls { dest, ssh } => client::ls(dest.as_deref(), &ssh)?,
        Cmd::Restart { dest: Some(d), ssh } => client::remote::ssh_restart(&d, &ssh)?,
        Cmd::Restart { dest: None, .. } => server::run(cfg, false, true)?,
        Cmd::Server { foreground, restart, port, no_shell_integration } => {
            if let Some(p) = port {
                cfg.port = p;
            }
            if no_shell_integration {
                cfg.shell_integration = false;
            }
            server::run(cfg, foreground, restart)?
        }
        Cmd::Bootstrap => client::bootstrap()?,
        Cmd::Namer { model } => {
            if let Some(m) = model {
                cfg.llm_model = m;
            }
            server::namer(&cfg)?
        }
        Cmd::NameTest { files, model, cwd } => {
            if let Some(m) = model {
                cfg.llm_model = m;
            }
            server::name_test(&cfg, &files, cwd.as_deref())?
        }
    }
    Ok(())
}

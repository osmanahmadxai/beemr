//! Command-line interface for beemr.

use std::io::{BufRead, IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use beemr::config::{default_device_name, Config};
use beemr::discovery::Dht;
use beemr::get::GetOptions;
use beemr::identity::{Contacts, DeviceId};
use beemr::profile::Profile;
use beemr::share::ShareOptions;
use beemr::{doctor, relay, service, util, Error, Result};

const USAGE: &str = "\
beemr: send files and  folders straight to another device.
No servers, no accounts, no setup. Everything is end-to-end encrypted.

Files:
  beemr share <file-or-folder> [options]   Share it and print a command for the other device
  beemr get <ticket> [-o <folder>]         Download something shared with you

This device:
  beemr setup                              Name this device
  beemr id                                 Show this device's ID (for --to)
  beemr name [new name]                    Show or change this device's name
  beemr contact add <name> <device-id>     Save someone's device under a name
  beemr contact list | remove <name>
  beemr relay [run|use <address>|list|remove <address>]   Relay for devices that can't connect directly
  beemr doctor                             Check how reachable this device is

Share options:
  --to <contact-or-id>    Only this device may download (repeat for several)
  -n, --downloads <n>     How many downloads to allow (default 1, 0 = unlimited)
  -e, --expires <time>    Stop accepting downloads after e.g. 30s, 10m, 2h or 1d
  -p, --port <port>       Listen on this port (default: random)
  --no-port-mapping       Don't ask the router to open a port (UPnP, PCP, NAT-PMP)
  --no-relay              Don't relay for other beemr users while sharing
  --copy                  Copy the beemr get command to the clipboard
";

#[tokio::main]
async fn main() -> ExitCode {
    // Diagnostics for troubleshooting, e.g. BEEMR_LOG=debug or
    // BEEMR_LOG=libp2p_relay=debug,beemr=debug.
    if let Ok(filter) = std::env::var("BEEMR_LOG") {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::new(filter))
            .with_writer(std::io::stderr)
            .try_init();
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run(args: &[String]) -> Result<()> {
    let Some((command, rest)) = args.split_first() else {
        print!("{USAGE}");
        return Ok(());
    };
    match command.as_str() {
        "share" | "send" => share(rest).await,
        "get" | "receive" => get(rest).await,
        "setup" => setup(rest),
        "id" => show_id(),
        "name" => name(rest),
        "contact" | "contacts" => contact(rest),
        // Versions before 0.3 had a background service for messages; its
        // autostart entries still run `beemr daemon run`. Remove them.
        "daemon" | "service" => remove_legacy_service(),
        "relay" => relay_command(rest).await,
        "doctor" => doctor::run(load_profile(true)?).await,
        // Developer tool: a standalone DHT node for private test networks.
        "dht-node" => dht_node().await,
        "help" | "-h" | "--help" => {
            print!("{USAGE}");
            Ok(())
        }
        "version" | "-V" | "--version" => {
            println!("beemr {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        other => Err(Error::new(format!(
            "unknown command '{other}' (run `beemr help` for usage)"
        ))),
    }
}

/// Load this device's profile. On first use, ask for a device name when a
/// person is at the keyboard.
fn load_profile(ask_name: bool) -> Result<Profile> {
    let config = Config::load()?;
    if ask_name && config.device_name()?.is_none() && std::io::stdin().is_terminal() {
        let name = prompt_name(&default_device_name())?;
        config.set_device_name(&name)?;
    }
    let profile = Profile::load_from(config)?;
    if profile.created {
        eprintln!(
            "Created this device's identity. Its ID is:\n\n    {}\n",
            profile.identity.id()
        );
    }
    Ok(profile)
}

fn prompt_name(default: &str) -> Result<String> {
    eprint!("Name this device (others will see it) [{default}]: ");
    std::io::stderr().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    let line = line.trim();
    Ok(if line.is_empty() { default } else { line }.to_string())
}

async fn share(args: &[String]) -> Result<()> {
    let profile = load_profile(true)?;
    let mut options = ShareOptions {
        path: PathBuf::new(),
        allowed: Vec::new(),
        max_downloads: Some(1),
        expires_in: None,
        port: 0,
        upnp: true,
        relay_for_others: true,
        copy: false,
    };
    let mut path = None;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--to" => options
                .allowed
                .push(profile.contacts.resolve(value(&mut args, arg)?)?),
            "-n" | "--downloads" => {
                let n: u32 = value(&mut args, arg)?
                    .parse()
                    .map_err(|_| Error::new("--downloads needs a whole number"))?;
                options.max_downloads = (n > 0).then_some(n);
            }
            "-e" | "--expires" => {
                let text = value(&mut args, arg)?;
                options.expires_in = Some(util::parse_duration(text).ok_or_else(|| {
                    Error::new(format!(
                        "can't understand --expires {text}; try 30s, 10m, 2h or 1d"
                    ))
                })?);
            }
            "-p" | "--port" => options.port = parse_port(value(&mut args, arg)?)?,
            "--no-port-mapping" | "--no-upnp" => options.upnp = false,
            "--no-relay" => options.relay_for_others = false,
            "--copy" => options.copy = true,
            flag if flag.starts_with('-') && flag.len() > 1 => {
                return Err(Error::new(format!("unknown option {flag}")))
            }
            _ if path.is_some() => {
                return Err(Error::new(
                    "share one file or folder at a time (put several in a folder)",
                ))
            }
            _ => path = Some(PathBuf::from(arg)),
        }
    }
    options.path = path
        .ok_or_else(|| Error::new("what should be shared? Usage: beemr share <file-or-folder>"))?;
    beemr::share::run(options, profile).await.map(drop)
}

async fn get(args: &[String]) -> Result<()> {
    let profile = load_profile(true)?;
    let mut ticket = None;
    let mut output_dir = PathBuf::from(".");
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-o" | "--output" => output_dir = PathBuf::from(value(&mut args, arg)?),
            _ if ticket.is_some() => return Err(Error::new("expected a single ticket")),
            _ => ticket = Some(arg.clone()),
        }
    }
    let ticket = ticket.ok_or_else(|| Error::new("missing ticket. Usage: beemr get <ticket>"))?;
    let saved = beemr::get::run(GetOptions { ticket, output_dir }, profile).await?;
    eprintln!("Saved to {}", saved.display());
    Ok(())
}

fn setup(args: &[String]) -> Result<()> {
    let config = Config::load()?;
    let mut name = None;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--name" => name = Some(value(&mut args, arg)?.to_string()),
            // Accepted for compatibility with older installers.
            "--no-service" => {}
            other => return Err(Error::new(format!("unknown option {other}"))),
        }
    }
    let current = config.device_name()?.unwrap_or_else(default_device_name);
    let name = match name {
        Some(name) => name,
        None if std::io::stdin().is_terminal() => prompt_name(&current)?,
        None => current,
    };
    let name = config.set_device_name(&name)?;
    let profile = Profile::load_from(config)?;
    let _ = service::remove_legacy(&profile.config);
    println!(
        "\nThis device is \"{name}\". Its ID is:\n\n    {}\n",
        profile.identity.id()
    );
    println!("Give this ID to people who want to send files only to this device (--to).");
    println!("\nTry it: beemr share <file>   ·   beemr help");
    Ok(())
}

/// Remove the background service installed by versions before 0.3.
fn remove_legacy_service() -> Result<()> {
    let config = Config::load()?;
    service::remove_legacy(&config)?;
    eprintln!(
        "beemr no longer runs a background service. Any service left by an older\n\
         version has been stopped and removed from startup."
    );
    Ok(())
}

fn show_id() -> Result<()> {
    let profile = load_profile(true)?;
    println!("{}", profile.identity.id());
    eprintln!(
        "\nThis is \"{}\". Share the ID above with people who want to send files only\n\
         to this device.",
        profile.name
    );
    Ok(())
}

fn name(args: &[String]) -> Result<()> {
    let config = Config::load()?;
    if args.is_empty() {
        println!(
            "{}",
            config.device_name()?.unwrap_or_else(default_device_name)
        );
        return Ok(());
    }
    let name = config.set_device_name(&args.join(" "))?;
    eprintln!("This device is now called \"{name}\".");
    Ok(())
}

fn contact(args: &[String]) -> Result<()> {
    let config = Config::load()?;
    let mut contacts = Contacts::load(&config)?;
    match args {
        [cmd, name @ .., id] if cmd == "add" && !name.is_empty() => {
            let id = DeviceId::parse(id)
                .ok_or_else(|| Error::new(format!("'{id}' is not a device ID")))?;
            let name = contacts.add(&name.join(" "), id)?;
            contacts.save()?;
            eprintln!("Saved {name}. Share with them using: beemr share <file> --to \"{name}\"");
        }
        [cmd, name @ ..] if (cmd == "remove" || cmd == "rm") && !name.is_empty() => {
            let name = name.join(" ");
            if !contacts.remove(&name) {
                return Err(Error::new(format!("no contact named '{name}'")));
            }
            contacts.save()?;
            eprintln!("Removed {name}.");
        }
        [cmd] if cmd == "list" || cmd == "ls" => list_contacts(&contacts),
        [] => list_contacts(&contacts),
        _ => {
            return Err(Error::new(
                "usage: beemr contact add <name> <device-id> | list | remove <name>",
            ))
        }
    }
    Ok(())
}

fn list_contacts(contacts: &Contacts) {
    if contacts.entries().is_empty() {
        eprintln!("No contacts yet. Add one with: beemr contact add <name> <device-id>");
    }
    for (name, id) in contacts.entries() {
        println!("{name}\t{id}");
    }
}

async fn relay_command(args: &[String]) -> Result<()> {
    let (sub, rest) = match args.split_first() {
        Some((s, r)) if !s.starts_with('-') => (s.as_str(), r),
        _ => ("run", args),
    };
    match sub {
        "run" => {
            let mut port = relay::DEFAULT_PORT;
            let mut public_listing = true;
            let mut args = rest.iter();
            while let Some(arg) = args.next() {
                match arg.as_str() {
                    "-p" | "--port" => port = parse_port(value(&mut args, arg)?)?,
                    "--private" => public_listing = false,
                    other => return Err(Error::new(format!("unknown option {other}"))),
                }
            }
            relay::run(load_profile(false)?, port, public_listing).await
        }
        "use" | "add" => {
            let [addr] = rest else {
                return Err(Error::new("usage: beemr relay use <address>"));
            };
            let addr: libp2p::Multiaddr = addr
                .parse()
                .map_err(|_| Error::new("that isn't a valid relay address"))?;
            if beemr::node::split_peer(&addr).is_none() {
                return Err(Error::new(
                    "the relay address must end in /p2p/<id> (copy it from `beemr relay run`)",
                ));
            }
            let config = Config::load()?;
            let mut relays = config.relays()?;
            if !relays.contains(&addr) {
                relays.push(addr);
            }
            config.set_relays(&relays)?;
            eprintln!("Saved. beemr will use this relay when a direct connection isn't possible.");
            Ok(())
        }
        "list" | "ls" => {
            let relays = Config::load()?.relays()?;
            if relays.is_empty() {
                eprintln!("No saved relays. Public beemr relays are found automatically.");
            }
            relays.iter().for_each(|r| println!("{r}"));
            Ok(())
        }
        "remove" | "rm" => {
            let [addr] = rest else {
                return Err(Error::new("usage: beemr relay remove <address>"));
            };
            let config = Config::load()?;
            let mut relays = config.relays()?;
            let before = relays.len();
            relays.retain(|r| r.to_string() != *addr);
            if relays.len() == before {
                return Err(Error::new("that relay isn't saved"));
            }
            config.set_relays(&relays)?;
            eprintln!("Removed.");
            Ok(())
        }
        other => Err(Error::new(format!(
            "unknown relay command '{other}' (run, use, list, remove)"
        ))),
    }
}

async fn dht_node() -> Result<()> {
    let network = beemr::config::NetworkSettings::from_env();
    let _dht =
        Dht::start(&network, true)?.ok_or_else(|| Error::new("couldn't start a DHT node"))?;
    eprintln!("DHT node running. Press Ctrl+C to stop.");
    tokio::signal::ctrl_c().await?;
    Ok(())
}

/// The value following an option, or an error naming the option.
fn value<'a>(args: &mut impl Iterator<Item = &'a String>, option: &str) -> Result<&'a str> {
    args.next()
        .map(String::as_str)
        .ok_or_else(|| Error::new(format!("{option} needs a value")))
}

fn parse_port(text: &str) -> Result<u16> {
    text.parse()
        .map_err(|_| Error::new("--port needs a number from 0 to 65535"))
}

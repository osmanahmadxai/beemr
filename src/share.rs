//! Sharing: serve a file or folder to devices that present the ticket.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use futures::io::{AsyncRead, AsyncWrite};
use futures::StreamExt;
use libp2p::{Multiaddr, PeerId, Stream};
use tokio::io::AsyncReadExt;
use tokio::sync::{watch, Notify};
use tokio::time::Instant;

use crate::discovery::{AddressRecord, Dht};
use crate::identity::DeviceId;
use crate::nat::NatKind;
use crate::node::{self, Mode, Node, NodeOptions, Status, UpnpState};
use crate::path::PathKind;
use crate::profile::Profile;
use crate::progress::Progress;
use crate::proto::{self, Binding, EntryKind, Framed, Message, Side, MAX_CHUNK};
use crate::ticket::{self, ShareSecret, Ticket};
use crate::tor::{self, Tor};
use crate::util::{format_bytes, format_duration};
use crate::{crypto, Context as _, Error, Result};

/// How long an unidentified connection may take to identify itself.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(20);
/// How long a transfer may stall before it is abandoned.
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(60);
/// Refresh the DHT record at least this often, even if nothing changed.
const REPUBLISH_EVERY: Duration = Duration::from_secs(15 * 60);
/// How long to wait for the network type before assuming it's hard to reach.
const CLASSIFY_FOR: Duration = Duration::from_secs(20);

pub struct ShareOptions {
    pub path: PathBuf,
    /// Devices allowed to download. Empty means anyone holding the ticket.
    pub allowed: Vec<DeviceId>,
    /// `None` means unlimited.
    pub max_downloads: Option<u32>,
    pub expires_in: Option<Duration>,
    pub port: u16,
    /// Ask the router to open a port (UPnP, PCP, NAT-PMP).
    pub upnp: bool,
    /// Relay for other beemr devices while sharing, when reachable.
    pub relay_for_others: bool,
    /// Copy the `beemr get <ticket>` command to the system clipboard.
    pub copy: bool,
}

/// Why sharing stopped.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    DownloadLimitReached,
    Expired { downloads: u32 },
}

/// Share `options.path` until the download limit is reached or the share expires.
pub async fn run(options: ShareOptions, profile: Profile) -> Result<Outcome> {
    let content = Content::scan(&options.path)?;
    let network = profile.network.clone();
    let node = Node::start(NodeOptions {
        mode: Mode::Share,
        port: options.port,
        public_network: network.public,
        upnp: network.public && options.upnp,
        relays: profile.config.relays()?,
        relay_for_others: options.relay_for_others,
        key_seed: None,
    })
    .await?;
    let dht = Dht::start(&network, false)?;

    eprintln!(
        "Sharing \"{}\" ({} file{}, {})",
        content.name,
        content.file_count,
        plural(content.file_count),
        format_bytes(content.total_bytes)
    );

    let secret: ShareSecret = crypto::random();
    let ticket = Ticket {
        device: profile.identity.id(),
        secret,
        port: node.port(),
        lan: node.lan_ips(!network.public),
    };
    let tor_dir = (network.public || tor::forced()).then(|| profile.config.dir().to_path_buf());
    let share = Arc::new(Share {
        node: node.clone(),
        secret,
        profile,
        content,
        allowed: options.allowed,
        max_downloads: options.max_downloads,
        deadline: options.expires_in.map(|d| Instant::now() + d),
        slots: Mutex::new(Slots::default()),
        changed: Notify::new(),
    });
    print_policy(&share, options.expires_in);
    let command = format!("beemr get {}", ticket.encode());
    eprintln!("\nOn the other device, run:\n");
    println!("    {command}");
    eprintln!();

    if options.copy {
        copy_to_clipboard(&command);
    }

    let (onion_tx, onion_rx) = watch::channel(None);
    if let Some(dht) = &dht {
        let name = share.profile.name.clone();
        let identity = share.profile.identity.clone();
        let salt = ticket::record_salt(&secret);
        tokio::spawn(publish_record(
            node.clone(),
            dht.clone(),
            identity,
            name,
            salt,
            !network.public,
            onion_rx,
        ));
        tokio::spawn(find_full_relays(node.clone(), dht.clone()));
        if options.relay_for_others {
            tokio::spawn(announce_relay_when_reachable(
                node.clone(),
                dht.clone(),
                network.public,
            ));
        }
    }

    let accept = share.accept_on(node.clone())?;
    let mut punch_requests = node
        .control()
        .accept(crate::punch::PROTOCOL)
        .map_err(|e| Error::new(e.to_string()))?;
    {
        // Receivers that can't reach us directly may ask for port prediction.
        let (share, node) = (Arc::clone(&share), node.clone());
        tokio::spawn(async move {
            while let Some((peer, stream)) = punch_requests.next().await {
                let (share, node) = (Arc::clone(&share), node.clone());
                tokio::spawn(async move {
                    match crate::punch::respond(&node, peer, stream).await {
                        // We were the strict side: serve over the new endpoint.
                        Ok(Some(endpoint)) => {
                            let _ = share.accept_on(endpoint);
                        }
                        Ok(None) => {}
                        Err(e) => tracing::debug!(error = %e, "port prediction"),
                    }
                });
            }
        });
    }

    eprintln!("Waiting for the other device… (Ctrl+C to stop sharing)\n");
    tokio::spawn(report_reachability(
        node.status(),
        network.public,
        options.relay_for_others,
    ));
    if let (Some(dir), Some(_)) = (tor_dir, &dht) {
        tokio::spawn(Arc::clone(&share).offer_tor_if_hard_to_reach(dir, onion_tx));
    }
    let outcome = tokio::select! {
        outcome = share.wait_until_finished() => outcome,
        _ = tokio::signal::ctrl_c() => {
            eprintln!("\nStopped sharing.");
            std::process::exit(130);
        }
    };
    accept.abort();
    match &outcome {
        Outcome::DownloadLimitReached => {
            eprintln!("\nDone: download limit reached, sharing stopped.")
        }
        Outcome::Expired { downloads } => eprintln!(
            "\nShare expired after {downloads} download{}; new connections are now refused.",
            plural(u64::from(*downloads))
        ),
    }
    Ok(outcome)
}

fn plural(n: u64) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

fn print_policy(share: &Share, expires_in: Option<Duration>) {
    let who = if share.allowed.is_empty() {
        "anyone with the command below".to_string()
    } else {
        let names: Vec<String> = share
            .allowed
            .iter()
            .map(|id| share.profile.contacts.describe(id, None))
            .collect();
        format!("only {}", names.join(", "))
    };
    let downloads = share
        .max_downloads
        .map_or("unlimited".to_string(), |n| n.to_string());
    let expires = expires_in.map_or("never".to_string(), |d| {
        format!("in {}", format_duration(d))
    });
    eprintln!("\nWho can download:  {who}");
    eprintln!("Downloads allowed: {downloads}");
    eprintln!("Expires:           {expires}");
}

/// Print a line each time this device becomes reachable in a new way.
pub async fn report_reachability(
    mut status: tokio::sync::watch::Receiver<Status>,
    public: bool,
    relay_for_others: bool,
) {
    let started = Instant::now();
    let (mut lan, mut direct, mut punch, mut full, mut warned, mut upnp_reported) =
        (false, false, false, false, false, false);
    let mut relaying = false;
    loop {
        {
            let s = status.borrow_and_update().clone();
            if !lan
                && s.listen
                    .iter()
                    .any(|a| node::ip_of(a).is_some_and(|ip| node::is_lan(&ip)))
            {
                lan = true;
                eprintln!("  ✓ Reachable on your local network");
            }
            if !direct && s.directly_reachable() {
                direct = true;
                match s.mapping_method() {
                    Some(how) => eprintln!(
                        "  ✓ Reachable from the internet: your router opened a port automatically ({how})"
                    ),
                    None => eprintln!("  ✓ Reachable from the internet: this device is directly reachable"),
                }
            }
            if relay_for_others && !relaying && s.directly_reachable() {
                relaying = true;
                eprintln!(
                    "  ✓ Relaying for other beemr users while sharing (encrypted; turn off with --no-relay)"
                );
            }
            if !punch && s.relayed.iter().any(|r| !r.full) {
                punch = true;
                eprintln!("  ✓ Hole punching ready (through firewalls, no setup needed)");
            }
            if !full && s.relayed.iter().any(|r| r.full) {
                full = true;
                eprintln!("  ✓ beemr relay available as a fallback");
            }
            if public
                && !upnp_reported
                && matches!(s.upnp, UpnpState::NotFound | UpnpState::NotRoutable)
            {
                upnp_reported = true;
            }
            if public
                && !warned
                && !direct
                && !punch
                && !full
                && started.elapsed() > Duration::from_secs(25)
            {
                warned = true;
                eprintln!(
                    "  … Still looking for a way through your network's firewall. Devices on\n    \
                     your local network can download now; others may need a moment."
                );
            }
        }
        tokio::select! {
            changed = status.changed() => if changed.is_err() { return },
            _ = tokio::time::sleep(Duration::from_secs(5)) => {}
        }
    }
}

/// While this sharer is reachable from the internet, announce it as a relay
/// on the DHT so devices that can't connect directly can use it.
async fn announce_relay_when_reachable(node: Node, dht: Dht, public: bool) {
    loop {
        let port = {
            let status = node.status();
            let status = status.borrow();
            status
                .external
                .iter()
                .filter(|a| node::ip_of(a).is_some_and(|ip| node::is_global(&ip) || !public))
                .find_map(port_of)
        };
        let wait = match port {
            Some(port) if dht.announce_relay(port).await.is_ok() => Duration::from_secs(20 * 60),
            _ => Duration::from_secs(30),
        };
        tokio::time::sleep(wait).await;
    }
}

fn port_of(addr: &Multiaddr) -> Option<u16> {
    addr.iter().find_map(|p| match p {
        libp2p::multiaddr::Protocol::Udp(port) | libp2p::multiaddr::Protocol::Tcp(port) => {
            Some(port)
        }
        _ => None,
    })
}

/// The addresses to publish, most useful first. Loopback addresses are only
/// useful in isolated test networks, where every device runs on one machine.
pub fn record_from_status(
    peer: PeerId,
    name: &str,
    status: &Status,
    include_loopback: bool,
    onion: Option<Multiaddr>,
) -> AddressRecord {
    let mut addrs: Vec<Multiaddr> = Vec::new();
    let mut push = |a: &Multiaddr| {
        if !addrs.contains(a) {
            addrs.push(a.clone());
        }
    };
    for addr in &status.listen {
        if node::ip_of(addr).is_some_and(|ip| ip.is_ipv6() && node::is_global(&ip)) {
            push(addr);
        }
    }
    status.external.iter().for_each(&mut push);
    for addr in &status.listen {
        if node::ip_of(addr).is_some_and(|ip| {
            node::is_global(&ip) || node::is_lan(&ip) || (include_loopback && ip.is_loopback())
        }) {
            push(addr);
        }
    }
    status
        .relayed
        .iter()
        .filter(|r| !r.full)
        .for_each(|r| push(&r.addr));
    AddressRecord {
        peer,
        name: name.to_string(),
        addrs,
        full_relays: status
            .relayed
            .iter()
            .filter(|r| r.full)
            .map(|r| r.addr.clone())
            .collect(),
        onion,
    }
}

/// Keep this node's signed address record on the DHT up to date.
pub async fn publish_record(
    node: Node,
    dht: Dht,
    identity: crate::identity::Identity,
    name: String,
    salt: Vec<u8>,
    include_loopback: bool,
    mut onion: watch::Receiver<Option<Multiaddr>>,
) {
    let mut status = node.status();
    let mut last: Option<(AddressRecord, Instant)> = None;
    loop {
        let record = record_from_status(
            node.peer_id(),
            &name,
            &status.borrow_and_update(),
            include_loopback,
            onion.borrow_and_update().clone(),
        );
        let stale = last
            .as_ref()
            .is_none_or(|(prev, at)| *prev != record || at.elapsed() > REPUBLISH_EVERY);
        let reachable =
            !record.addrs.is_empty() || !record.full_relays.is_empty() || record.onion.is_some();
        if stale && reachable && dht.publish(&identity, &salt, &record).await.is_ok() {
            last = Some((record, Instant::now()));
        }
        // Batch bursts of address changes into one publish.
        tokio::select! {
            changed = status.changed() => {
                if changed.is_err() { return }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            Ok(()) = onion.changed() => {}
            _ = tokio::time::sleep(Duration::from_secs(60)) => {}
        }
    }
}

/// Look for beemr relays on the DHT until we hold a full relay reservation.
pub async fn find_full_relays(node: Node, dht: Dht) {
    // Search often at first, then back off: relays announce themselves
    // shortly after starting, and the DHT may still be warming up.
    let mut wait = Duration::from_secs(5);
    loop {
        if node.status().borrow().relayed.iter().any(|r| r.full) {
            tokio::time::sleep(Duration::from_secs(10 * 60)).await;
            continue;
        }
        let relays = dht.find_relays(8).await;
        tracing::debug!(found = relays.len(), "searched the DHT for beemr relays");
        let addrs = relays
            .into_iter()
            .flat_map(|addr| node::addrs_for((*addr.ip()).into(), addr.port()))
            .collect();
        node.add_relay_candidates(addrs);
        tokio::time::sleep(wait).await;
        wait = (wait * 2).min(Duration::from_secs(2 * 60));
    }
}

/// The files and folders being shared, scanned up front.
struct Content {
    name: String,
    items: Vec<Item>,
    total_bytes: u64,
    file_count: u64,
}

struct Item {
    /// Path relative to the share's parent, `/`-separated, starting with the share's name.
    path: String,
    source: PathBuf,
    kind: EntryKind,
    size: u64,
    mode: u32,
}

impl Content {
    fn scan(path: &Path) -> Result<Content> {
        let meta = fs::metadata(path).context(path.display())?;
        let name = path
            .canonicalize()
            .context(path.display())?
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .context("can't share the root of a drive; pick a folder inside it")?;

        let mut items = Vec::new();
        if meta.is_file() {
            items.push(Item::new(name.clone(), path.to_path_buf(), &meta));
        } else if meta.is_dir() {
            items.push(Item::new(name.clone(), path.to_path_buf(), &meta));
            walk(path, &name, &mut items)?;
        } else {
            bail!("{} is not a regular file or folder", path.display());
        }

        let files = items.iter().filter(|i| i.kind == EntryKind::File);
        Ok(Content {
            total_bytes: files.clone().map(|i| i.size).sum(),
            file_count: files.count() as u64,
            name,
            items,
        })
    }
}

impl Item {
    fn new(path: String, source: PathBuf, meta: &fs::Metadata) -> Item {
        let kind = if meta.is_dir() {
            EntryKind::Dir
        } else {
            EntryKind::File
        };
        Item {
            path,
            source,
            kind,
            size: if meta.is_file() { meta.len() } else { 0 },
            mode: permission_bits(meta),
        }
    }
}

/// Recursively add a folder's contents. Symlinks and special files are skipped
/// so a share can never reach outside the chosen folder.
fn walk(dir: &Path, rel: &str, items: &mut Vec<Item>) -> Result<()> {
    let mut entries = fs::read_dir(dir)
        .context(dir.display())?
        .collect::<std::io::Result<Vec<_>>>()
        .context(dir.display())?;
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let meta = entry.metadata().context(entry.path().display())?;
        let path = format!("{rel}/{}", entry.file_name().to_string_lossy());
        if meta.is_dir() {
            items.push(Item::new(path.clone(), entry.path(), &meta));
            walk(&entry.path(), &path, items)?;
        } else if meta.is_file() {
            items.push(Item::new(path, entry.path(), &meta));
        } else {
            eprintln!("  (skipping {path}: links and special files aren't shared)");
        }
    }
    Ok(())
}

#[cfg(unix)]
fn permission_bits(meta: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o777
}

#[cfg(not(unix))]
fn permission_bits(meta: &fs::Metadata) -> u32 {
    if meta.is_dir() {
        0o755
    } else {
        0o644
    }
}

/// State shared by all connection tasks.
struct Share {
    node: Node,
    secret: ShareSecret,
    profile: Profile,
    content: Content,
    allowed: Vec<DeviceId>,
    max_downloads: Option<u32>,
    deadline: Option<Instant>,
    slots: Mutex<Slots>,
    changed: Notify,
}

#[derive(Default)]
struct Slots {
    active: u32,
    completed: u32,
}

impl Share {
    fn slots(&self) -> MutexGuard<'_, Slots> {
        self.slots.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn expired(&self) -> bool {
        self.deadline.is_some_and(|d| Instant::now() >= d)
    }

    fn authorize(&self, peer: &DeviceId) -> Result<(), &'static str> {
        if !self.allowed.is_empty() && !self.allowed.contains(peer) {
            return Err("this share is for a different device");
        }
        if self.expired() {
            return Err("this share has expired");
        }
        Ok(())
    }

    /// Claim one of the allowed downloads. Counting in-progress transfers
    /// guarantees the limit holds even with simultaneous receivers.
    fn reserve(self: &Arc<Self>) -> Result<Slot, &'static str> {
        let mut slots = self.slots();
        if let Some(max) = self.max_downloads {
            if slots.completed >= max {
                return Err("this share has reached its download limit");
            }
            if slots.active + slots.completed >= max {
                return Err("another device is downloading this right now");
            }
        }
        slots.active += 1;
        Ok(Slot {
            share: Arc::clone(self),
            succeeded: false,
        })
    }

    async fn wait_until_finished(&self) -> Outcome {
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let wait = {
                let slots = self.slots();
                if self.max_downloads.is_some_and(|max| slots.completed >= max) {
                    return Outcome::DownloadLimitReached;
                }
                match self.deadline {
                    Some(deadline) if Instant::now() >= deadline => {
                        if slots.active == 0 {
                            return Outcome::Expired {
                                downloads: slots.completed,
                            };
                        }
                        // Expired, but let transfers already underway finish.
                        Duration::from_secs(1)
                    }
                    Some(deadline) => deadline - Instant::now(),
                    None => Duration::from_secs(3600),
                }
            };
            tokio::select! {
                _ = notified => {}
                _ = tokio::time::sleep(wait) => {}
            }
        }
    }

    /// How `peer` is connected to us, including how our router opened a
    /// port when the connection came in over a mapped port.
    fn path_to(&self, node: &Node, peer: &PeerId) -> Option<PathKind> {
        match node.path_to(peer)? {
            PathKind::Internet { mapped: None } => Some(PathKind::Internet {
                mapped: self.node.status().borrow().mapping_method(),
            }),
            other => Some(other),
        }
    }

    /// Serve beemr streams arriving on `node` (the main node, or a second
    /// endpoint created by port prediction).
    fn accept_on(self: &Arc<Self>, node: Node) -> Result<tokio::task::JoinHandle<()>> {
        let mut incoming = node
            .control()
            .accept(proto::PROTOCOL)
            .map_err(|e| Error::new(e.to_string()))?;
        let share = Arc::clone(self);
        Ok(tokio::spawn(async move {
            while let Some((peer, stream)) = incoming.next().await {
                let (share, node) = (Arc::clone(&share), node.clone());
                tokio::spawn(async move { share.handle(&node, peer, stream).await });
            }
        }))
    }

    async fn handle(self: Arc<Self>, node: &Node, peer: PeerId, stream: Stream) {
        let binding = Binding::Libp2p {
            initiator: peer,
            responder: node.peer_id(),
        };
        // The receiver closes its other connections to us just before opening
        // this stream; let those closes arrive so we name the right path.
        let started = Instant::now();
        while node.connections_to(&peer).len() > 1 && started.elapsed() < Duration::from_secs(1) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let path = self.path_to(node, &peer);
        self.serve(stream, binding, path).await;
    }

    /// Serve a stream that arrived through our onion service.
    async fn handle_tor(self: Arc<Self>, mut stream: arti_client::DataStream, onion: [u8; 32]) {
        let mut nonce = [0u8; 32];
        let read = futures::io::AsyncReadExt::read_exact(&mut stream, &mut nonce);
        let read = tokio::time::timeout(HANDSHAKE_TIMEOUT, read);
        if let Ok(Ok(())) = read.await {
            let binding = Binding::Tor { onion, nonce };
            self.serve(stream, binding, Some(PathKind::Tor)).await;
        }
    }

    async fn serve<S: AsyncRead + AsyncWrite + Unpin>(
        self: Arc<Self>,
        stream: S,
        binding: Binding,
        path: Option<PathKind>,
    ) {
        let mut framed = Framed::new(stream);
        let profile = &self.profile;

        // Failures before the peer is identified are noise (scans, stale
        // tickets, losing parallel connection attempts) and are dropped silently.
        let identified = tokio::time::timeout(HANDSHAKE_TIMEOUT, async {
            let info = proto::handshake(
                &mut framed,
                &profile.identity,
                &profile.name,
                &binding,
                Side::Responder,
            )
            .await?;
            match framed.recv().await? {
                Message::Download { proof }
                    if proto::download_proof_matches(&self.secret, &binding, &proof) =>
                {
                    Ok(info)
                }
                Message::Download { .. } => {
                    let _ = framed.send(&Message::Fail("invalid ticket".into())).await;
                    bail!("invalid ticket")
                }
                _ => {
                    let _ = framed
                        .send(&Message::Fail(
                            "this device is sharing a file, not receiving messages".into(),
                        ))
                        .await;
                    bail!("unexpected request")
                }
            }
        })
        .await;
        let Ok(Ok(info)) = identified else {
            return;
        };

        let who = profile.contacts.describe(&info.device, Some(&info.name));
        let slot = self.authorize(&info.device).and_then(|()| self.reserve());
        let mut slot = match slot {
            Ok(slot) => slot,
            Err(reason) => {
                eprintln!("  ✗ Refused {who}: {reason}");
                let _ = framed.send(&Message::Fail(reason.to_string())).await;
                framed.finish(Duration::from_secs(2)).await;
                return;
            }
        };

        eprintln!("  → Sending to {who}");
        if let Some(path) = path {
            eprintln!("    How: {path}");
        }
        match send_content(&mut framed, &self.content).await {
            Ok(()) => {
                slot.succeeded = true;
                eprintln!("  ✓ {who} received everything");
            }
            Err(e) => {
                eprintln!("  ✗ Transfer to {who} failed: {e}");
                let _ = framed.send(&Message::Fail(e.to_string())).await;
            }
        }
        framed.close().await;
    }
    /// Start a Tor onion service as a last resort, but only when this device
    /// is hard to reach: behind a strict NAT (or one we couldn't identify)
    /// with no beemr relay to fall back on. Runs until the process exits.
    async fn offer_tor_if_hard_to_reach(
        self: Arc<Self>,
        dir: PathBuf,
        onion: watch::Sender<Option<Multiaddr>>,
    ) {
        if !tor::forced() && !self.hard_to_reach().await {
            return;
        }
        eprintln!("  … This network is hard to reach from outside; preparing Tor as a last resort");
        let service = async {
            let tor = Tor::start(&dir).await?;
            let (service, streams) = tor.launch()?;
            // Publishing the onion service's descriptor takes a little while.
            let started = Instant::now();
            while !service.is_running() && started.elapsed() < Duration::from_secs(120) {
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            Ok::<_, Error>((tor, service, streams))
        };
        let (_tor, service, streams) = match service.await {
            Ok(running) => running,
            Err(e) => {
                tracing::debug!(error = %e, "couldn't start Tor");
                return;
            }
        };
        let _ = onion.send(Some(service.multiaddr()));
        eprintln!("  ✓ Tor ready as a last resort (slowest, but gets through any firewall)");
        let id = service.id();
        let mut streams = std::pin::pin!(streams);
        while let Some(stream) = streams.next().await {
            tokio::spawn(Arc::clone(&self).handle_tor(stream, id));
        }
    }

    /// Wait until we know whether this device is hard to reach.
    async fn hard_to_reach(&self) -> bool {
        let started = Instant::now();
        let mut status = self.node.status();
        loop {
            {
                let s = status.borrow_and_update();
                if s.directly_reachable() || s.relayed.iter().any(|r| r.full) {
                    return false;
                }
                match s.nat {
                    NatKind::Symmetric => return true,
                    NatKind::Unknown if started.elapsed() >= CLASSIFY_FOR => return true,
                    NatKind::Cone | NatKind::Reachable if started.elapsed() >= CLASSIFY_FOR => {
                        return false
                    }
                    _ => {}
                }
            }
            tokio::select! {
                changed = status.changed() => if changed.is_err() { return false },
                _ = tokio::time::sleep(Duration::from_secs(1)) => {}
            }
        }
    }
}

/// A claimed download; released when dropped, counting it only if it succeeded.
struct Slot {
    share: Arc<Share>,
    succeeded: bool,
}

impl Drop for Slot {
    fn drop(&mut self) {
        let mut slots = self.share.slots();
        slots.active -= 1;
        if self.succeeded {
            slots.completed += 1;
        }
        drop(slots);
        self.share.changed.notify_waiters();
    }
}

async fn send_content(
    framed: &mut Framed<impl AsyncRead + AsyncWrite + Unpin>,
    content: &Content,
) -> Result<()> {
    with_timeout(framed.send(&Message::Header {
        name: content.name.clone(),
        total_bytes: content.total_bytes,
        file_count: content.file_count,
    }))
    .await?;
    let mut progress = Progress::new("Sending  ", content.total_bytes);
    for item in &content.items {
        with_timeout(framed.send(&Message::Entry {
            kind: item.kind,
            mode: item.mode,
            size: item.size,
            path: item.path.clone(),
        }))
        .await?;
        if item.kind == EntryKind::File {
            send_file(framed, item, &mut progress)
                .await
                .map_err(|e| e.context(&item.path))?;
        }
    }
    with_timeout(framed.send(&Message::End {
        total_bytes: content.total_bytes,
    }))
    .await?;
    progress.finish();

    match with_timeout(framed.recv()).await? {
        Message::Ack => Ok(()),
        Message::Fail(reason) => bail!("the receiver reported a problem: {reason}"),
        _ => bail!("protocol error: expected an acknowledgement"),
    }
}

async fn send_file(
    framed: &mut Framed<impl AsyncRead + AsyncWrite + Unpin>,
    item: &Item,
    progress: &mut Progress,
) -> Result<()> {
    let mut file = tokio::fs::File::open(&item.source).await?;
    let mut remaining = item.size;
    while remaining > 0 {
        let len = remaining.min(MAX_CHUNK as u64) as usize;
        let mut chunk = vec![0u8; len];
        file.read_exact(&mut chunk)
            .await
            .map_err(|_| Error::new("file changed size while being shared"))?;
        with_timeout(framed.send(&Message::Data(chunk))).await?;
        remaining -= len as u64;
        progress.add(len as u64);
    }
    Ok(())
}

async fn with_timeout<T>(future: impl std::future::Future<Output = Result<T>>) -> Result<T> {
    tokio::time::timeout(TRANSFER_TIMEOUT, future)
        .await
        .map_err(|_| Error::new("the other device stopped responding"))?
}

fn copy_to_clipboard(text: &str) {
    match try_copy_to_clipboard(text) {
        Ok(()) => eprintln!("  ✓ Copied the command to the clipboard"),
        Err(e) => eprintln!("  ✗ {e}"),
    }
}

fn try_copy_to_clipboard(text: &str) -> std::result::Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        run_clipboard_cmd("clip.exe", &[], text)
            .map_err(|_| "Couldn't copy to the clipboard".to_string())
    }

    #[cfg(target_os = "macos")]
    {
        run_clipboard_cmd("pbcopy", &[], text)
            .map_err(|_| "Couldn't copy to the clipboard".to_string())
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        let tools: &[(&str, &[&str])] = &[
            ("wl-copy", &[]),
            ("xclip", &["-selection", "clipboard"]),
            ("xsel", &["--clipboard", "--input"]),
        ];

        for (cmd, args) in tools {
            if run_clipboard_cmd(cmd, args, text).is_ok() {
                return Ok(());
            }
        }
        Err("Couldn't copy to the clipboard (install wl-clipboard, xclip or xsel)".to_string())
    }
}

fn run_clipboard_cmd(cmd: &str, args: &[&str], text: &str) -> std::result::Result<(), String> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let mut child = Command::new(cmd)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| e.to_string())?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(text.as_bytes())
            .map_err(|e| e.to_string())?;
    } // stdin is dropped here, so the tool sees end-of-file

    let status = child.wait().map_err(|e| e.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("exited with {status}"))
    }
}

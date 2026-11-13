//! An IRC account: a connection to an IRC bouncer such as soju, which stays on the networks and keeps
//! their history while the app is closed. The bouncer is reached at the account's address,
//!     irc://USER:PASSWORD@HOST:PORT  or  ircs://USER:PASSWORD@HOST:PORT?sha256=FINGERPRINT
//! (the user name and password percent-encoded; FINGERPRINT the hex SHA-256 of the bouncer's
//! certificate, which is taken in place of a certificate authority's, as a bouncer on a home network
//! has none). Only standard IRCv3 is spoken: SASL PLAIN to sign in, soju.im/bouncer-networks to list
//! the networks and bind a connection to each, draft/chathistory to fill in what came while away,
//! draft/read-marker to share what was read with the account's other clients, and echo-message with
//! server-time so that a message sent here gets the server's time, as its history copy has.
//!
//! A channel is a group conversation and a person a direct one, keyed by network and name
//! ("NETID/#channel", "NETID/nick"). Text that starts with /me, /join, /part, /msg or /nick is that
//! command; anything else is said to the conversation.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

use crate::db::NewMessage;
use crate::shared::{now_ms, Command, Event, Shared, State};

const DIRECT: i64 = 0;
const GROUP: i64 = 1;
const RETRY: Duration = Duration::from_secs(30);
/// How long the bouncer has to answer while a connection is opened, and to a PING.
const ANSWER: Duration = Duration::from_secs(30);
/// After this long without a line from the bouncer it is sent a PING, to find a connection that died
/// without closing (the bouncer's host restarting, a network gone).
const QUIET: Duration = Duration::from_secs(120);
/// How much history is asked for at once, and for a conversation with none held.
const HISTORY: usize = 300;
const CAPS: &[&str] = &["sasl", "server-time", "message-tags", "batch", "echo-message", "multi-prefix", "draft/chathistory", "draft/read-marker", "soju.im/bouncer-networks"];

/// Starts the account's thread and returns the channel its commands go to.
pub fn spawn(shared: Arc<Shared>, account: i64, address: String) -> mpsc::UnboundedSender<Command> {
    let (tx, rx) = mpsc::unbounded_channel();
    std::thread::Builder::new()
        .name(format!("irc-{account}"))
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("a runtime");
            tokio::task::LocalSet::new().block_on(&runtime, run(shared, account, address, rx));
        })
        .expect("a thread");
    tx
}

/// Where and how the bouncer is reached.
#[derive(Clone, Debug, PartialEq)]
pub struct Address {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: String,
    /// The SHA-256 of the certificate a TLS connection must present; None for a plain connection.
    pub pin: Option<[u8; 32]>,
}

pub fn parse_address(address: &str) -> Result<Address, String> {
    // (an address pasted from a file may bring its line ending, or spaces, with it)
    let address = address.trim();
    let (tls, rest) = if let Some(rest) = address.strip_prefix("ircs://") {
        (true, rest)
    } else if let Some(rest) = address.strip_prefix("irc://") {
        (false, rest)
    } else {
        return Err("the address starts with irc:// or ircs://".into());
    };
    let (rest, query) = rest.split_once('?').unwrap_or((rest, ""));
    let (login, place) = rest.rsplit_once('@').ok_or("the address names a user: USER:PASSWORD@HOST")?;
    let (user, password) = login.split_once(':').unwrap_or((login, ""));
    let place = place.trim_end_matches('/');
    let (host, port) = match place.rsplit_once(':') {
        Some((host, port)) => (host, port.parse().map_err(|_| "the port is a number")?),
        None => (place, if tls { 6697 } else { 6667 }),
    };
    let pin = query.split('&').find_map(|pair| pair.strip_prefix("sha256=")).map(|hex| {
        let bytes = hex::decode(hex.replace(':', "")).map_err(|_| "the fingerprint is hex")?;
        <[u8; 32]>::try_from(bytes).map_err(|_| "the fingerprint is a SHA-256, 32 bytes")
    });
    let pin = match (tls, pin) {
        (true, Some(pin)) => Some(pin?),
        (true, None) => return Err("an ircs:// address gives the bouncer's certificate fingerprint, ?sha256=...".into()),
        (false, _) => None,
    };
    Ok(Address { host: host.into(), port, user: decode(user), password: decode(password), pin })
}

fn decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Some(byte) = text.get(i + 1..i + 3).and_then(|hex| u8::from_str_radix(hex, 16).ok()) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// One IRC message: its tags, its source, its command and its parameters.
#[derive(Debug, Default, PartialEq)]
pub struct Line {
    pub tags: HashMap<String, String>,
    pub source: String,
    pub command: String,
    pub params: Vec<String>,
}

impl Line {
    pub fn parse(text: &str) -> Option<Line> {
        let mut rest = text.trim_end_matches(['\r', '\n']);
        let mut line = Line::default();
        if let Some(tags) = rest.strip_prefix('@') {
            let (tags, after) = tags.split_once(' ')?;
            for tag in tags.split(';') {
                let (key, value) = tag.split_once('=').unwrap_or((tag, ""));
                line.tags.insert(key.into(), unescape_tag(value));
            }
            rest = after.trim_start();
        }
        if let Some(source) = rest.strip_prefix(':') {
            let (source, after) = source.split_once(' ')?;
            line.source = source.into();
            rest = after.trim_start();
        }
        let (command, mut rest) = rest.split_once(' ').unwrap_or((rest, ""));
        line.command = command.to_ascii_uppercase();
        while !rest.is_empty() {
            if let Some(trailing) = rest.strip_prefix(':') {
                line.params.push(trailing.into());
                break;
            }
            let (param, after) = rest.split_once(' ').unwrap_or((rest, ""));
            if !param.is_empty() {
                line.params.push(param.into());
            }
            rest = after;
        }
        (!line.command.is_empty()).then_some(line)
    }

    /// The nick in the source, nick!user@host.
    pub fn nick(&self) -> &str {
        self.source.split('!').next().unwrap_or("")
    }

    /// The server's time for the message, in ms since 1970, from its time tag.
    pub fn time_ms(&self) -> Option<i64> {
        self.tags.get("time").and_then(|t| parse_time(t))
    }
}

fn unescape_tag(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some(':') => out.push(';'),
            Some('s') => out.push(' '),
            Some('r') => out.push('\r'),
            Some('n') => out.push('\n'),
            Some(other) => out.push(other),
            None => {},
        }
    }
    out
}

/// "2026-11-05T06:12:00.123Z" as ms since 1970.
pub fn parse_time(text: &str) -> Option<i64> {
    let b = text.as_bytes();
    if b.len() < 20 || b[4] != b'-' || b[10] != b'T' {
        return None;
    }
    let n = |r: std::ops::Range<usize>| text.get(r)?.parse::<i64>().ok();
    let (y, mo, d, h, mi, s) = (n(0..4)?, n(5..7)?, n(8..10)?, n(11..13)?, n(14..16)?, n(17..19)?);
    let ms = if b[19] == b'.' { text[20..].trim_end_matches('Z').get(..3).and_then(|f| format!("{f:0<3}").parse().ok()).unwrap_or(0) } else { 0 };
    // (days from 1970 to the date, by the civil-from-days method)
    let (y, mo) = if mo <= 2 { (y - 1, mo + 9) } else { (y, mo - 3) };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * mo + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Some(((days * 24 + h) * 60 + mi) * 60_000 + s * 1000 + ms)
}

pub fn format_time(ms: i64) -> String {
    let days = ms.div_euclid(86_400_000);
    let rem = ms.rem_euclid(86_400_000);
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + (mo <= 2) as i64;
    format!("{y:04}-{mo:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z", rem / 3_600_000, rem / 60_000 % 60, rem / 1000 % 60, rem % 1000)
}

/// IRC's formatting codes taken out of a text: the plain text, and its bold, italic, struck-through
/// and monospace stretches as the database keeps them, "STYLE START END" lines counted in characters
/// (Signal's numbers: bold 1, italic 2, strikethrough 4, monospace 5). Colours, underline and reverse
/// have no style here and are dropped.
pub fn formatted(text: &str) -> (String, String) {
    const STYLES: [(char, i32); 4] = [('\u{2}', 1), ('\u{1d}', 2), ('\u{1e}', 4), ('\u{11}', 5)];
    let mut plain = String::with_capacity(text.len());
    let (mut open, mut styles): ([Option<usize>; 4], String) = ([None; 4], String::new());
    let close = |open: &mut [Option<usize>; 4], i: usize, at: usize, styles: &mut String| {
        if let Some(start) = open[i].take().filter(|&start| start < at) {
            styles.push_str(&format!("{} {start} {at}\n", STYLES[i].1));
        }
    };
    let mut chars = text.chars().peekable();
    let mut at = 0;
    while let Some(c) = chars.next() {
        if let Some(i) = STYLES.iter().position(|s| s.0 == c) {
            if open[i].is_some() { close(&mut open, i, at, &mut styles) } else { open[i] = Some(at) }
            continue;
        }
        match c {
            '\u{f}' => (0..4).for_each(|i| close(&mut open, i, at, &mut styles)),
            '\u{1f}' | '\u{16}' => {},
            // (a colour: up to two digits, and a background after a comma)
            '\u{3}' | '\u{4}' => {
                // (two decimal digits for a colour number, six hex for a colour written out)
                let (width, digit): (usize, fn(&char) -> bool) = if c == '\u{3}' { (2, char::is_ascii_digit) } else { (6, char::is_ascii_hexdigit) };
                let skip = |chars: &mut std::iter::Peekable<std::str::Chars>| (0..width).take_while(|_| chars.next_if(digit).is_some()).count();
                if skip(&mut chars) > 0 && chars.peek() == Some(&',') && chars.clone().nth(1).is_some_and(|d| digit(&d)) {
                    chars.next();
                    skip(&mut chars);
                }
            },
            c => {
                plain.push(c);
                at += 1;
            },
        }
    }
    (0..4).for_each(|i| close(&mut open, i, at, &mut styles));
    (plain, styles)
}

/// Whether a text names a nick, as a word of its own in any case, as IRC clients highlight it.
pub fn names(text: &str, nick: &str) -> bool {
    let (text, nick) = (text.to_lowercase(), nick.to_lowercase());
    let word = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || "_-[]\\`^{}|".contains(c));
    !nick.is_empty() && text.match_indices(&nick).any(|(at, _)| !word(text[..at].chars().next_back()) && !word(text[at + nick.len()..].chars().next()))
}

fn is_channel(target: &str) -> bool {
    target.starts_with(['#', '&', '!', '+'])
}

/// What a line the user wrote asks for: words to a conversation, or a command.
#[derive(Debug, PartialEq)]
pub enum Said {
    Text(String),
    Action(String),
    Join(String),
    Part(String),
    Message(String, String),
    Nick(String),
}

pub fn said(text: &str) -> Said {
    let Some(command) = text.strip_prefix('/').filter(|t| !t.starts_with('/')) else {
        return Said::Text(text.strip_prefix('/').map(|t| t.to_owned()).filter(|_| text.starts_with("//")).unwrap_or_else(|| text.to_owned()));
    };
    let (word, rest) = command.split_once(' ').unwrap_or((command, ""));
    let rest = rest.trim();
    match word.to_ascii_lowercase().as_str() {
        "me" => Said::Action(rest.into()),
        "join" | "j" if !rest.is_empty() => Said::Join(rest.into()),
        "part" | "leave" => Said::Part(rest.into()),
        "msg" | "query" if rest.contains(' ') => {
            let (to, words) = rest.split_once(' ').unwrap();
            Said::Message(to.into(), words.trim().into())
        },
        "nick" if !rest.is_empty() => Said::Nick(rest.into()),
        _ => Said::Text(text.into()),
    }
}

type Conn = Box<dyn tokio::io::AsyncRead + Unpin>;
type Out = Box<dyn tokio::io::AsyncWrite + Unpin>;

async fn connect(address: &Address) -> Result<(Conn, Out), String> {
    let tcp = tokio::net::TcpStream::connect((address.host.as_str(), address.port)).await.map_err(|e| format!("{}:{}: {e}", address.host, address.port))?;
    let Some(pin) = address.pin else {
        let (r, w) = tcp.into_split();
        return Ok((Box::new(r), Box::new(w)));
    };
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(Pinned { pin, provider }))
        .with_no_client_auth();
    let name = rustls::pki_types::ServerName::try_from(address.host.clone()).map_err(|e| e.to_string())?;
    let tls = tokio_rustls::TlsConnector::from(Arc::new(config)).connect(name, tcp).await.map_err(|e| format!("TLS: {e}"))?;
    let (r, w) = tokio::io::split(tls);
    Ok((Box::new(r), Box::new(w)))
}

/// Accepts the one certificate whose SHA-256 is the pin, and checks that the server holds its key.
#[derive(Debug)]
struct Pinned {
    pin: [u8; 32],
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl rustls::client::danger::ServerCertVerifier for Pinned {
    fn verify_server_cert(&self, end: &rustls::pki_types::CertificateDer<'_>, _: &[rustls::pki_types::CertificateDer<'_>], _: &rustls::pki_types::ServerName<'_>, _: &[u8], _: rustls::pki_types::UnixTime) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        if Sha256::digest(end.as_ref())[..] == self.pin {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General("the bouncer's certificate is not the one in the address".into()))
        }
    }
    fn verify_tls12_signature(&self, message: &[u8], cert: &rustls::pki_types::CertificateDer<'_>, dss: &rustls::DigitallySignedStruct) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }
    fn verify_tls13_signature(&self, message: &[u8], cert: &rustls::pki_types::CertificateDer<'_>, dss: &rustls::DigitallySignedStruct) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

/// A connection's two halves: lines come in on one and are written to the other.
struct Link {
    lines: tokio::io::Lines<BufReader<Conn>>,
    out: Out,
}

impl Link {
    async fn send(&mut self, line: &str) -> Result<(), String> {
        self.out.write_all(format!("{line}\r\n").as_bytes()).await.map_err(|e| e.to_string())
    }

    async fn next(&mut self) -> Result<Line, String> {
        loop {
            match self.lines.next_line().await.map_err(|e| e.to_string())? {
                None => return Err("the bouncer closed the connection".into()),
                Some(text) => match Line::parse(&text) {
                    Some(line) if line.command == "PING" => self.send(&format!("PONG :{}", line.params.first().map(String::as_str).unwrap_or(""))).await?,
                    Some(line) => return Ok(line),
                    None => {},
                },
            }
        }
    }

    /// Opens a connection and signs in: negotiates the capabilities, authenticates with SASL PLAIN,
    /// binds to a network when one is named, and waits for the welcome. Returns the nick it has.
    async fn open(address: &Address, network: Option<&str>) -> Result<(Link, String), String> {
        let (reader, out) = connect(address).await?;
        let mut link = Link { lines: BufReader::new(reader).lines(), out };
        link.send("CAP LS 302").await?;
        let mut offered = String::new();
        loop {
            let line = link.next().await?;
            if line.command == "CAP" && line.params.get(1).is_some_and(|p| p == "LS") {
                let more = line.params.get(2).is_some_and(|p| p == "*");
                offered.push(' ');
                offered.push_str(line.params.last().map(String::as_str).unwrap_or(""));
                if !more {
                    break;
                }
            }
        }
        let offered: Vec<&str> = offered.split_whitespace().map(|cap| cap.split('=').next().unwrap_or(cap)).collect();
        let wanted: Vec<&str> = CAPS.iter().copied().filter(|cap| offered.contains(cap)).collect();
        if !wanted.contains(&"sasl") {
            return Err("the bouncer does not offer SASL".into());
        }
        link.send(&format!("CAP REQ :{}", wanted.join(" "))).await?;
        link.send("AUTHENTICATE PLAIN").await?;
        loop {
            let line = link.next().await?;
            match line.command.as_str() {
                "AUTHENTICATE" => {
                    use base64::Engine;
                    let token = base64::engine::general_purpose::STANDARD.encode(format!("{0}\0{0}\0{1}", address.user, address.password));
                    link.send(&format!("AUTHENTICATE {token}")).await?;
                },
                "903" => break,
                "902" | "904" | "905" | "906" => return Err(format!("the bouncer refused the sign-in: {}", line.params.last().cloned().unwrap_or_default())),
                _ => {},
            }
        }
        if let Some(network) = network {
            link.send(&format!("BOUNCER BIND {network}")).await?;
        }
        link.send(&format!("NICK {}", address.user.split('/').next().unwrap_or("gink"))).await?;
        link.send(&format!("USER {} 0 * :gink", address.user)).await?;
        link.send("CAP END").await?;
        loop {
            let line = link.next().await?;
            match line.command.as_str() {
                "001" => return Ok((link, line.params.first().cloned().unwrap_or_default())),
                "433" => link.send(&format!("NICK {}_", line.params.get(1).cloned().unwrap_or_default())).await?,
                "ERROR" => return Err(line.params.last().cloned().unwrap_or_default()),
                _ => {},
            }
        }
    }
}

/// Runs sessions until the app closes the core, which drops the command channel.
async fn run(shared: Arc<Shared>, account: i64, address: String, mut commands: mpsc::UnboundedReceiver<Command>) {
    loop {
        let error = match session(&shared, account, &address, &mut commands).await {
            Ok(()) => return,
            Err(error) => error,
        };
        shared.update(account, |s| (s.state, s.error) = (State::Offline, error));
        // (while offline a conversation the user reads is read here all the same; other commands are dropped)
        let wait = tokio::time::sleep(RETRY);
        tokio::pin!(wait);
        loop {
            tokio::select! {
                _ = &mut wait => break,
                command = commands.recv() => match command {
                    None => return,
                    Some(Command::MarkRead { conversation }) => {
                        if shared.db.mark_read(conversation, now_ms()).await.is_ok_and(|read| !read.is_empty()) {
                            shared.push(Event::Listed { account, conversation });
                        }
                    },
                    Some(_) => {},
                },
            }
        }
    }
}

/// A step of opening a connection, given ANSWER to finish.
async fn within<T>(what: &str, step: impl std::future::Future<Output = Result<T, String>>) -> Result<T, String> {
    tokio::time::timeout(ANSWER, step).await.map_err(|_| format!("the bouncer did not answer {what} within {} s", ANSWER.as_secs()))?
}

/// A message the server is to echo back, so that it takes the server's time.
struct Sent {
    message: i64,
    target: String,
    text: String,
}

/// One network's connection, and what it knows.
struct Network {
    id: String,
    link: Link,
    nick: String,
    /// Sent messages waiting for their echo, oldest first.
    sent: VecDeque<Sent>,
    /// Open batches of history: their reference, and the conversation they fill.
    batches: HashMap<String, Option<String>>,
    /// When a line last came from the bouncer, and whether it was sent a PING since.
    heard_at: tokio::time::Instant,
    pinged: bool,
}

enum Happened {
    Line(usize, Result<Line, String>),
    Command(Option<Command>),
    Quiet,
}

async fn session(shared: &Arc<Shared>, account: i64, address: &str, commands: &mut mpsc::UnboundedReceiver<Command>) -> Result<(), String> {
    let address = parse_address(address)?;
    shared.update(account, |s| (s.state, s.error) = (State::Connecting, String::new()));
    // (messages an earlier build kept under a nick as written, not its lowercase key, are named again)
    shared.db.lowercase_senders(account).await.map_err(|e| e.to_string())?;
    // (messages kept with IRC's formatting codes still in them, by an earlier build, are read again)
    for (message, text) in shared.db.coded_texts(account).await.map_err(|e| e.to_string())? {
        let (plain, styles) = formatted(&text);
        shared.db.set_text_and_styles(message, &plain, &styles).await.map_err(|e| e.to_string())?;
    }
    // The bouncer's networks, from a first connection; a server that is not such a bouncer is one network.
    let (mut control, _) = within("the sign-in", Link::open(&address, None)).await?;
    control.send("BOUNCER LISTNETWORKS").await?;
    let mut ids = Vec::new();
    let mut names = Vec::new();
    within("with its networks", async { loop {
        let line = control.next().await?;
        match line.command.as_str() {
            "BOUNCER" if line.params.first().is_some_and(|p| p == "NETWORK") => {
                if let Some(id) = line.params.get(1) {
                    ids.push(id.clone());
                    names.push(line.params.get(2).and_then(|attrs| attrs.split(';').find_map(|a| a.strip_prefix("name=").map(unescape_tag))).unwrap_or_else(|| id.clone()));
                }
            },
            "BATCH" if line.params.first().is_some_and(|p| p.starts_with('-')) => break,
            "FAIL" | "421" => break,
            _ => {},
        }
    } Ok(()) }).await?;
    drop(control);
    let mut networks = Vec::new();
    if ids.is_empty() {
        let (link, nick) = within("the sign-in", Link::open(&address, None)).await?;
        networks.push(Network { id: "0".into(), link, nick, sent: VecDeque::new(), batches: HashMap::new(), heard_at: tokio::time::Instant::now(), pinged: false });
    }
    for id in &ids {
        let (link, nick) = within("the sign-in", Link::open(&address, Some(id))).await?;
        networks.push(Network { id: id.clone(), link, nick, sent: VecDeque::new(), batches: HashMap::new(), heard_at: tokio::time::Instant::now(), pinged: false });
    }
    let label = networks.iter().zip(names.iter().map(String::as_str).chain(std::iter::repeat(address.host.as_str()))).map(|(n, name)| format!("{} on {name}", n.nick)).collect::<Vec<_>>().join(", ");
    shared.db.set_account_label(account, &label).await.map_err(|e| e.to_string())?;
    shared.update(account, |s| (s.state, s.label) = (State::CatchingUp, label.clone()));
    // (people written to while away, whom no JOIN names)
    for network in &mut networks {
        let since = format_time(shared.db.newest_ms_of(account).await.unwrap_or(0).max(1));
        network.link.send(&format!("CHATHISTORY TARGETS timestamp={since} timestamp={} {HISTORY}", format_time(now_ms() + 60_000))).await?;
    }
    shared.update(account, |s| s.state = State::Online);
    flush(shared, account, &mut networks).await?;
    loop {
        // (the earliest a network's quiet calls for a PING, or for giving up on one sent)
        let due = networks.iter().map(|n| n.heard_at + if n.pinged { QUIET + ANSWER } else { QUIET }).min().unwrap_or_else(|| tokio::time::Instant::now() + QUIET);
        let happened = {
            let mut reads: Vec<_> = networks.iter_mut().enumerate().map(|(i, n)| Box::pin(async move { (i, n.link.next().await) })).collect();
            tokio::select! {
                ((i, line), _, _) = futures::future::select_all(reads.iter_mut()) => Happened::Line(i, line),
                command = commands.recv() => Happened::Command(command),
                _ = tokio::time::sleep_until(due) => Happened::Quiet,
            }
        };
        match happened {
            Happened::Quiet => {
                let now = tokio::time::Instant::now();
                for network in &mut networks {
                    if network.pinged && now >= network.heard_at + QUIET + ANSWER {
                        return Err("the bouncer stopped answering".into());
                    }
                    if !network.pinged && now >= network.heard_at + QUIET {
                        network.link.send("PING :gink").await?;
                        network.pinged = true;
                    }
                }
            },
            Happened::Line(i, line) => {
                let line = line?;
                (networks[i].heard_at, networks[i].pinged) = (tokio::time::Instant::now(), false);
                let message = line.command == "PRIVMSG" || line.command == "NOTICE";
                heard(shared, account, &mut networks[i], line).await?;
                if message {
                    shared.update(account, |s| s.last_received_ms = now_ms());
                }
            },
            Happened::Command(None) => return Ok(()),
            Happened::Command(Some(Command::Flush)) => flush(shared, account, &mut networks).await?,
            Happened::Command(Some(Command::MarkRead { conversation })) => {
                // (read here at once, as the bouncer may not answer: it keeps no read marker for its
                // own service, BouncerServ)
                if !shared.db.mark_read(conversation, now_ms()).await.map_err(|e| e.to_string())?.is_empty() {
                    shared.push(Event::Listed { account, conversation });
                }
                if let Ok(Some((_, key, _))) = shared.db.conversation_key(conversation).await
                    && let Some((id, target)) = key.split_once('/')
                    && let Some(network) = networks.iter_mut().find(|n| n.id == id)
                {
                    let newest = shared.db.newest_ms(conversation).await.unwrap_or(0);
                    if newest > 0 {
                        network.link.send(&format!("MARKREAD {target} timestamp={}", format_time(newest))).await?;
                    }
                }
            },
            // (what Signal has and IRC does not: timers, removing members, requests, sessions, typing)
            Happened::Command(Some(_)) => {},
        }
    }
}

/// Messages a sender gives in one second are told apart by the milliseconds after it, up to this many.
const SAME_SECOND: i64 = 1000;

/// A time to the second: what a bouncer keeps of it, as the history it gives back has it.
fn second(ms: i64) -> i64 {
    ms - ms.rem_euclid(1000)
}

/// Keeps a message, at its second or the first millisecond after it free of the sender's others.
/// Returns its id; None when it is held already, as one with the same text from the same sender in
/// that second.
async fn store(shared: &Arc<Shared>, conversation: i64, outgoing: bool, sender: &str, sent_ms: i64, text: &str) -> Result<Option<i64>, String> {
    for at in sent_ms..sent_ms + SAME_SECOND {
        let message = NewMessage { conversation, outgoing, sender, sent_ms: at, received_ms: now_ms(), text, kind: 0, quote: None, expire_s: 0, expire_start_ms: outgoing.then_some(at) };
        if let Some(id) = shared.db.add_message(&message).await.map_err(|e| e.to_string())? {
            return Ok(Some(id));
        }
        if shared.db.text_at(conversation, sender, at).await.map_err(|e| e.to_string())?.as_deref() == Some(text) {
            return Ok(None);
        }
    }
    Ok(None)
}

/// A conversation for a target on a network, made if missing.
async fn conversation_for(shared: &Arc<Shared>, account: i64, network: &str, target: &str) -> Result<i64, String> {
    let kind = if is_channel(target) { GROUP } else { DIRECT };
    let key = format!("{network}/{}", target.to_lowercase());
    let (id, made) = shared.db.conversation(account, &key, kind, target).await.map_err(|e| e.to_string())?;
    if made {
        shared.push(Event::Conversations);
    }
    Ok(id)
}

/// Asks for what a conversation missed: what came after its newest message, or its latest.
async fn ask_history(shared: &Arc<Shared>, network: &mut Network, conversation: i64, target: &str) -> Result<(), String> {
    let newest = shared.db.newest_ms(conversation).await.unwrap_or(0);
    // (from the start of the newest message's second, which the bouncer keeps whole: what is held already is known again)
    let ask = if newest > 0 { format!("CHATHISTORY AFTER {target} timestamp={} {HISTORY}", format_time(second(newest) - 1)) } else { format!("CHATHISTORY LATEST {target} * {HISTORY}") };
    network.link.send(&ask).await
}

async fn heard(shared: &Arc<Shared>, account: i64, network: &mut Network, line: Line) -> Result<(), String> {
    let own = |nick: &str| nick.eq_ignore_ascii_case(&network.nick);
    match line.command.as_str() {
        "BATCH" => {
            let Some(reference) = line.params.first() else { return Ok(()) };
            if let Some(reference) = reference.strip_prefix('+') {
                network.batches.insert(reference.into(), line.params.get(2).cloned());
            } else if let Some(reference) = reference.strip_prefix('-') {
                network.batches.remove(reference);
            }
        },
        "PRIVMSG" | "NOTICE" => {
            let (Some(to), Some(text)) = (line.params.first(), line.params.get(1)) else { return Ok(()) };
            let from = line.nick().to_owned();
            // (a server's own notices, whose source is no one's nick!user@host, are not conversations;
            // history gives bare nicks)
            if !line.source.contains('!') && !line.tags.contains_key("batch") {
                return Ok(());
            }
            let outgoing = own(&from);
            let target = if is_channel(to) { to.clone() } else if outgoing { to.clone() } else { from.clone() };
            let text = match text.strip_prefix("\u{1}ACTION ").map(|t| t.trim_end_matches('\u{1}')) {
                Some(action) => format!("* {from} {action}"),
                None if text.starts_with('\u{1}') => return Ok(()),  // (other CTCP: versions, pings)
                None => text.clone(),
            };
            let (text, styles) = formatted(&text);
            let sent_ms = second(line.time_ms().unwrap_or_else(now_ms));
            let conversation = conversation_for(shared, account, &network.id, &target).await?;
            // The echo of a message sent here takes the server's time.
            if outgoing && let Some(at) = network.sent.iter().position(|s| s.target.eq_ignore_ascii_case(&target) && s.text == text) {
                let sent = network.sent.remove(at).unwrap();
                for at in sent_ms..sent_ms + SAME_SECOND {
                    if shared.db.set_sent(sent.message, at, &text).await.map_err(|e| e.to_string())? {
                        break;
                    }
                    // (the server's copy, read back as history before the echo came, is the message)
                    if shared.db.text_at(conversation, "", at).await.map_err(|e| e.to_string())?.as_deref() == Some(text.as_str()) {
                        shared.db.forget_message(sent.message).await.map_err(|e| e.to_string())?;
                        break;
                    }
                }
                shared.push(Event::ConversationChanged { account, conversation });
                return Ok(());
            }
            if !outgoing {
                let _ = shared.db.person(account, &from.to_lowercase(), &from).await;
            }
            // (nicks are the same person whatever their case: the key is lowercase, the name as written)
            let key = from.to_lowercase();
            let sender = if outgoing { "" } else { key.as_str() };
            if let Some(id) = store(shared, conversation, outgoing, sender, sent_ms, &text).await? {
                if !outgoing && names(&text, &network.nick) {
                    shared.db.set_mentions_me(id).await.map_err(|e| e.to_string())?;
                }
                if !styles.is_empty() {
                    shared.db.set_styles(id, &styles).await.map_err(|e| e.to_string())?;
                }
                // (history comes in a batch: told as a change to the conversation once it is read)
                if line.tags.contains_key("batch") {
                    shared.push(Event::ConversationChanged { account, conversation });
                } else {
                    shared.push(Event::Message { account, conversation, message: id });
                }
            }
        },
        "JOIN" if own(line.nick()) => {
            let Some(channel) = line.params.first() else { return Ok(()) };
            let conversation = conversation_for(shared, account, &network.id, channel).await?;
            ask_history(shared, network, conversation, channel).await?;
        },
        // (names, as each channel is joined: its members)
        "353" => {
            let (Some(channel), Some(names)) = (line.params.get(2), line.params.get(3)) else { return Ok(()) };
            let conversation = conversation_for(shared, account, &network.id, channel).await?;
            let mut members = shared.db.members(conversation).await.map(|m| m.into_iter().map(|(key, _)| key).collect::<Vec<_>>()).unwrap_or_default();
            for name in names.split_whitespace() {
                let nick = name.trim_start_matches(['~', '&', '@', '%', '+']);
                let _ = shared.db.person(account, &nick.to_lowercase(), nick).await;
                members.push(nick.to_lowercase());
            }
            members.sort();
            members.dedup();
            let _ = shared.db.set_members(conversation, &members).await;
        },
        "CHATHISTORY" if line.params.first().is_some_and(|p| p == "TARGETS") => {
            if let Some(target) = line.params.get(1).filter(|t| !is_channel(t)) {
                let conversation = conversation_for(shared, account, &network.id, target).await?;
                ask_history(shared, network, conversation, target).await?;
            }
        },
        "MARKREAD" => {
            let (Some(target), Some(time)) = (line.params.first(), line.params.get(1)) else { return Ok(()) };
            if let Some(until) = time.strip_prefix("timestamp=").and_then(parse_time) {
                let conversation = conversation_for(shared, account, &network.id, target).await?;
                if shared.db.mark_read_until(conversation, until, now_ms()).await.unwrap_or(false) {
                    shared.push(Event::Listed { account, conversation });
                }
            }
        },
        // (the bouncer's refusals are worth a line in the log: a history or read marker it could not give)
        "FAIL" => crate::log::line(&format!("irc: {}", line.params.join(" "))),
        code if code.len() == 3 && (code.starts_with('4') || code.starts_with('5')) && code != "433" => crate::log::line(&format!("irc: {code} {}", line.params.join(" "))),
        "NICK" if own(line.nick()) => {
            if let Some(nick) = line.params.first() {
                network.nick = nick.clone();
            }
        },
        _ => {},
    }
    Ok(())
}

/// Sends what waits in the database, each line to its network.
async fn flush(shared: &Arc<Shared>, account: i64, networks: &mut [Network]) -> Result<(), String> {
    for waiting in shared.db.waiting(account).await.map_err(|e| e.to_string())? {
        let Some((id, target)) = waiting.key.split_once('/') else { continue };
        let Some(network) = networks.iter_mut().find(|n| n.id == id) else { continue };
        let target = target.to_owned();
        let echo = |network: &mut Network, to: &str, text: String| network.sent.push_back(Sent { message: waiting.id, target: to.to_owned(), text });
        for text in waiting.text.lines().filter(|l| !l.is_empty()) {
            match said(text) {
                Said::Text(words) => {
                    network.link.send(&format!("PRIVMSG {target} :{words}")).await?;
                    echo(network, &target, words);
                },
                Said::Action(words) => {
                    network.link.send(&format!("PRIVMSG {target} :\u{1}ACTION {words}\u{1}")).await?;
                    echo(network, &target, format!("* {} {words}", network.nick));
                },
                Said::Message(to, words) => {
                    network.link.send(&format!("PRIVMSG {to} :{words}")).await?;
                },
                Said::Join(channels) => network.link.send(&format!("JOIN {channels}")).await?,
                Said::Part(rest) => network.link.send(&format!("PART {}", if rest.is_empty() { target.clone() } else { rest })).await?,
                Said::Nick(nick) => network.link.send(&format!("NICK {nick}")).await?,
            }
        }
        shared.db.mark_sent(waiting.id, 0, now_ms()).await.map_err(|e| e.to_string())?;
        shared.push(Event::MessageChanged { account, conversation: waiting.conversation, message: waiting.id });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_parse_with_tags_source_and_trailing() {
        let line = Line::parse("@time=2026-11-05T06:12:00.123Z;msgid=a\\sb :nick!u@h PRIVMSG #chan :hello there\r\n").unwrap();
        assert_eq!(line.command, "PRIVMSG");
        assert_eq!(line.nick(), "nick");
        assert_eq!(line.params, ["#chan", "hello there"]);
        assert_eq!(line.tags["msgid"], "a b");
        assert_eq!(line.time_ms(), Some(1_793_859_120_123));
        assert_eq!(Line::parse("PING :x").unwrap().params, ["x"]);
    }

    #[test]
    fn formatting_codes_become_styles() {
        // (as ChanServ sends a notice on MyAnonamouse's IRC)
        let (plain, styles) = formatted("To view the \u{2}\u{3}07\u{2}\u{2}General information\u{2}\u{3}\u{2}\u{2}, type: \u{3}03,01!info\u{3} \u{1d}now\u{f} done");
        assert_eq!(plain, "To view the General information, type: !info now done");
        assert_eq!(styles, "1 12 31\n2 45 48\n");
        assert_eq!(formatted("\u{4}ff00aaX\u{11}code").0, "Xcode");
        assert_eq!(formatted("\u{3},5 keeps a comma").0, ",5 keeps a comma");
        assert_eq!(formatted("plain").1, "");
    }

    #[test]
    fn a_nick_is_named_as_a_word() {
        assert!(names("downloader712: are you there?", "downloader712"));
        assert!(names("thanks Downloader712!", "downloader712"));
        assert!(!names("downloader7123 said", "downloader712"));
        assert!(!names("xdownloader712", "downloader712"));
    }

    #[test]
    fn times_go_both_ways() {
        for ms in [0, 1_793_859_120_123, 951_782_400_000, 4_102_444_799_999] {
            assert_eq!(parse_time(&format_time(ms)), Some(ms));
        }
        assert_eq!(format_time(1_793_859_120_123), "2026-11-05T06:12:00.123Z");
    }

    #[test]
    fn addresses_read() {
        let a = parse_address("ircs://mihai%2Fmam:p%40ss@192.168.2.45:6697?sha256=00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff").unwrap();
        assert_eq!((a.user.as_str(), a.password.as_str(), a.host.as_str(), a.port), ("mihai/mam", "p@ss", "192.168.2.45", 6697));
        assert_eq!(a.pin.unwrap()[1], 0x11);
        assert_eq!(parse_address("irc://u:p@localhost").unwrap().port, 6667);
        assert!(parse_address("ircs://u:p@host").is_err());
        assert_eq!(parse_address(" irc://u:p@localhost:1\r\n").unwrap().port, 1);
    }

    #[test]
    fn commands_are_told_from_words() {
        assert_eq!(said("hello"), Said::Text("hello".into()));
        assert_eq!(said("/me waves"), Said::Action("waves".into()));
        assert_eq!(said("/join #a,#b"), Said::Join("#a,#b".into()));
        assert_eq!(said("/msg NickServ REGAIN downloader712"), Said::Message("NickServ".into(), "REGAIN downloader712".into()));
        assert_eq!(said("//not a command"), Said::Text("/not a command".into()));
        assert_eq!(said("/shrug"), Said::Text("/shrug".into()));
    }
}

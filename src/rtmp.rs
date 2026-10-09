//! A small RTMP publisher: handshake, connect, publish, then FLV tags as RTMP
//! messages. Networking runs on its own thread so a slow connection can never
//! stall capture or encoding. When the network falls behind we drop video
//! until the next keyframe (like OBS does) and keep the audio; when the
//! connection breaks we reconnect.

use std::collections::{HashMap, VecDeque};
use std::io::{self, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::amf::{self, Value};
use crate::flv::{AUDIO, Sink, Tag, VIDEO};

/// Twitch's own "Default" ingest, which routes to the nearest server.
pub const TWITCH: &str = "rtmp://ingest.global-contribute.live-video.net/app";
pub const YOUTUBE: &str = "rtmp://a.rtmp.youtube.com/live2";

const CHUNK_SIZE: usize = 4096;
/// Once this much media is waiting to be sent, the network is falling behind.
const MAX_BACKLOG_MS: u32 = 1500;
/// Attempts before giving up when we have never been live (likely a wrong address or key).
const FIRST_ATTEMPTS: u32 = 3;
const TIMEOUT: Duration = Duration::from_secs(10);

// Chunk stream IDs.
const CS_CONTROL: u8 = 2;
const CS_COMMAND: u8 = 3;
const CS_AUDIO: u8 = 4;
const CS_VIDEO: u8 = 6;

// Message types (audio and video are the same numbers as FLV tag types).
const SET_CHUNK_SIZE: u8 = 1;
const ACK: u8 = 3;
const USER_CONTROL: u8 = 4;
const WINDOW_ACK_SIZE: u8 = 5;
const DATA_AMF0: u8 = 18;
const COMMAND_AMF0: u8 = 20;

pub struct Metadata {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub video_kbps: u32,
    pub audio_kbps: Option<u32>,
}

#[derive(Clone)]
pub enum Status {
    Connecting,
    Live,
    Reconnecting(String),
}

#[derive(Clone)]
struct Item {
    kind: u8,
    ms: u32,
    body: Vec<u8>,
    keyframe: bool,
    config: bool,
}

#[derive(Default)]
struct Queue {
    items: VecDeque<Item>,
    /// Latest decoder configs, re-sent after every (re)connect.
    configs: Vec<Item>,
    skip_until_keyframe: bool,
    closing: bool,
}

impl Queue {
    fn push(&mut self, item: Item) -> u64 {
        let mut dropped = 0;
        if item.kind == VIDEO && !item.config {
            if self.skip_until_keyframe && !item.keyframe {
                return 1;
            }
            if item.keyframe {
                self.skip_until_keyframe = false;
            }
        }
        self.items.push_back(item);
        if self.backlog_ms() > MAX_BACKLOG_MS {
            // The network can't keep up. Waiting video is useless without its
            // keyframe, so drop it all and resume at the next keyframe.
            let before = self.items.len();
            self.items.retain(|i| i.kind != VIDEO || i.config);
            dropped += (before - self.items.len()) as u64;
            self.skip_until_keyframe = true;
            // If even audio alone is too far behind, drop the oldest of it too.
            while self.backlog_ms() > MAX_BACKLOG_MS * 2 {
                match self.items.iter().position(|i| !i.config) {
                    Some(oldest) => drop(self.items.remove(oldest)),
                    None => break,
                }
            }
        }
        dropped
    }

    /// How much media time is waiting (decoder configs don't count).
    fn backlog_ms(&self) -> u32 {
        let first = self.items.iter().find(|i| !i.config);
        let last = self.items.iter().rev().find(|i| !i.config);
        match (first, last) {
            (Some(first), Some(last)) => last.ms.saturating_sub(first.ms),
            _ => 0,
        }
    }

    /// After a reconnect, stale media is worthless: restart from fresh audio and the next keyframe.
    fn reset_for_reconnect(&mut self) {
        self.items.retain(|i| i.config);
        self.skip_until_keyframe = true;
    }
}

struct Shared {
    queue: Mutex<Queue>,
    wake: Condvar,
    status: Mutex<Status>,
    sent_bytes: AtomicU64,
    dropped_frames: AtomicU64,
    want_keyframe: AtomicBool,
}

impl Shared {
    fn set_status(&self, status: Status) {
        *self.status.lock().unwrap() = status;
    }
}

/// Read-only view of the stream for the status line.
#[derive(Clone)]
pub struct Monitor(Arc<Shared>);

impl Monitor {
    pub fn status(&self) -> Status {
        self.0.status.lock().unwrap().clone()
    }
    pub fn sent_bytes(&self) -> u64 {
        self.0.sent_bytes.load(Ordering::Relaxed)
    }
    pub fn dropped_frames(&self) -> u64 {
        self.0.dropped_frames.load(Ordering::Relaxed)
    }
    /// True once after a reconnect, so the encoder can send a keyframe right away.
    pub fn take_keyframe_request(&self) -> bool {
        self.0.want_keyframe.swap(false, Ordering::Relaxed)
    }
}

pub struct Rtmp {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl Rtmp {
    /// Connects and starts publishing. Returns once the server accepted the
    /// stream, or with an error if it couldn't (wrong address, wrong key...).
    /// `server` is "twitch", "youtube" or an rtmp:// URL; the key is never printed.
    pub fn connect(server: &str, key: String, metadata: Metadata) -> Result<Self, String> {
        let target = Target::parse(server)?;
        let shared = Arc::new(Shared {
            queue: Mutex::new(Queue::default()),
            wake: Condvar::new(),
            status: Mutex::new(Status::Connecting),
            sent_bytes: AtomicU64::new(0),
            dropped_frames: AtomicU64::new(0),
            want_keyframe: AtomicBool::new(false),
        });
        let (ready_tx, ready_rx) = mpsc::channel();
        let thread_shared = shared.clone();
        let thread = std::thread::Builder::new()
            .name("rtmp".into())
            .spawn(move || run(target, key, metadata, thread_shared, ready_tx))
            .map_err(|e| e.to_string())?;
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self { shared, thread: Some(thread) }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => Err("the network thread stopped unexpectedly".into()),
        }
    }

    pub fn monitor(&self) -> Monitor {
        Monitor(self.shared.clone())
    }

    pub fn describe(server: &str) -> String {
        match server {
            "twitch" => format!("Twitch ({TWITCH})"),
            "youtube" => format!("YouTube ({YOUTUBE})"),
            other => other.to_string(),
        }
    }
}

impl Sink for Rtmp {
    fn write(&mut self, tag: &Tag) -> io::Result<()> {
        let item = Item { kind: tag.kind, ms: tag.ms, body: tag.body.to_vec(), keyframe: tag.keyframe, config: tag.config };
        let mut queue = self.shared.queue.lock().unwrap();
        if item.config {
            queue.configs.retain(|c| c.kind != item.kind);
            queue.configs.push(item.clone());
        }
        let dropped = queue.push(item);
        drop(queue);
        self.shared.dropped_frames.fetch_add(dropped, Ordering::Relaxed);
        self.shared.wake.notify_one();
        Ok(())
    }

    /// Sends whatever is still queued, says goodbye to the server and stops.
    fn finish(&mut self) -> io::Result<()> {
        self.shared.queue.lock().unwrap().closing = true;
        self.shared.wake.notify_all();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        Ok(())
    }
}

impl Drop for Rtmp {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

/// The network thread: connect, stream, reconnect, until asked to stop.
fn run(target: Target, key: String, metadata: Metadata, shared: Arc<Shared>, ready: mpsc::Sender<Result<(), String>>) {
    let mut ready = Some(ready);
    let mut failures = 0u32;
    loop {
        match Session::open(&target, &key) {
            Ok(mut session) => {
                failures = 0;
                if ready.is_none() {
                    // Back after a drop: skip what piled up meanwhile and restart
                    // at a fresh keyframe, so viewers rejoin at the live edge.
                    shared.queue.lock().unwrap().reset_for_reconnect();
                    shared.want_keyframe.store(true, Ordering::Relaxed);
                }
                shared.set_status(Status::Live);
                if let Some(ready) = ready.take() {
                    let _ = ready.send(Ok(()));
                }
                match session.stream(&metadata, &shared) {
                    Ok(()) => return, // finished on request
                    Err(e) => {
                        shared.set_status(Status::Reconnecting(e));
                        shared.queue.lock().unwrap().reset_for_reconnect();
                    }
                }
            }
            Err(e) => {
                failures += 1;
                if ready.is_some() && failures >= FIRST_ATTEMPTS {
                    let _ = ready.take().unwrap().send(Err(e));
                    return;
                }
                if ready.is_none() {
                    shared.set_status(Status::Reconnecting(e));
                }
            }
        }
        // Back off 1, 2, 4, 8, then 15 s; stop early if the user quits.
        let wait = Duration::from_secs((1u64 << failures.min(3)).min(15));
        let queue = shared.queue.lock().unwrap();
        let (queue, _) = shared.wake.wait_timeout_while(queue, wait, |q| !q.closing).unwrap();
        if queue.closing {
            if let Some(ready) = ready.take() {
                let _ = ready.send(Err("stopped before connecting".into()));
            }
            return;
        }
    }
}

struct Target {
    host: String,
    port: u16,
    app: String,
    tc_url: String,
}

impl Target {
    fn parse(server: &str) -> Result<Self, String> {
        let server = match server {
            "twitch" => TWITCH,
            "youtube" => YOUTUBE,
            other => other,
        };
        if server.starts_with("rtmps://") {
            return Err("rtmps:// isn't supported yet; use the platform's rtmp:// address".into());
        }
        let rest = server
            .strip_prefix("rtmp://")
            .ok_or("the server must be twitch, youtube or an rtmp:// address")?
            .trim_end_matches('/');
        let (authority, app) =
            rest.split_once('/').ok_or("the server address needs an app path, like rtmp://host/app")?;
        let (host, port) = match authority.rsplit_once(':') {
            Some((host, port)) => (host, port.parse().map_err(|_| format!("bad port in {server}"))?),
            None => (authority, 1935),
        };
        if host.is_empty() || app.is_empty() {
            return Err(format!("can't understand the server address {server}"));
        }
        Ok(Self { host: host.into(), port, app: app.into(), tc_url: format!("rtmp://{authority}/{app}") })
    }
}

struct Session {
    writer: Arc<Mutex<TcpStream>>,
    socket: TcpStream,
    alive: Arc<AtomicBool>,
    reader: Option<JoinHandle<()>>,
    stream_id: u32,
}

impl Session {
    fn open(target: &Target, key: &str) -> Result<Self, String> {
        let address = (target.host.as_str(), target.port)
            .to_socket_addrs()
            .map_err(|e| format!("can't find {}: {e}", target.host))?
            .next()
            .ok_or(format!("can't find {}", target.host))?;
        let socket = TcpStream::connect_timeout(&address, TIMEOUT)
            .map_err(|e| format!("can't connect to {}: {e}", target.host))?;
        let io_error = |e: io::Error| format!("connection to {} failed: {e}", target.host);
        socket.set_nodelay(true).map_err(io_error)?;
        socket.set_read_timeout(Some(TIMEOUT)).map_err(io_error)?;
        socket.set_write_timeout(Some(TIMEOUT)).map_err(io_error)?;
        handshake(&socket).map_err(io_error)?;

        let mut reader = ChunkReader::new(BufReader::new(socket.try_clone().map_err(io_error)?));
        let mut writer = socket.try_clone().map_err(io_error)?;
        writer.write_all(&message(CS_CONTROL, SET_CHUNK_SIZE, 0, 0, &(CHUNK_SIZE as u32).to_be_bytes())).map_err(io_error)?;

        let connect = Value::Object(vec![
            ("app".into(), Value::str(&target.app)),
            ("type".into(), Value::str("nonprivate")),
            ("flashVer".into(), Value::str("FMLE/3.0 (compatible; MilerCast)")),
            ("tcUrl".into(), Value::str(&target.tc_url)),
        ]);
        send_command(&mut writer, 0, "connect", 1, connect, vec![]).map_err(io_error)?;
        reply(&mut reader, &mut writer, 1, "connect")?;

        send_command(&mut writer, 0, "releaseStream", 2, Value::Null, vec![Value::str(key)]).map_err(io_error)?;
        send_command(&mut writer, 0, "FCPublish", 3, Value::Null, vec![Value::str(key)]).map_err(io_error)?;
        send_command(&mut writer, 0, "createStream", 4, Value::Null, vec![]).map_err(io_error)?;
        let created = reply(&mut reader, &mut writer, 4, "createStream")?;
        let stream_id = created.get(3).and_then(Value::as_number).ok_or("the server didn't create a stream")? as u32;

        send_command(&mut writer, stream_id, "publish", 5, Value::Null, vec![Value::str(key), Value::str("live")])
            .map_err(io_error)?;
        wait_for_publish(&mut reader, &mut writer)?;

        // From here on a separate thread answers the server's pings and acks.
        socket.set_read_timeout(None).map_err(io_error)?;
        let writer = Arc::new(Mutex::new(writer));
        let alive = Arc::new(AtomicBool::new(true));
        let reader = {
            let (writer, alive) = (writer.clone(), alive.clone());
            std::thread::Builder::new()
                .name("rtmp-read".into())
                .spawn(move || {
                    while let Ok(msg) = reader.read_message() {
                        let mut out = writer.lock().unwrap();
                        if handle_control(&msg, &mut reader, &mut *out).is_err() {
                            break;
                        }
                    }
                    alive.store(false, Ordering::Relaxed);
                })
                .map_err(|e| e.to_string())?
        };
        Ok(Self { writer, socket, alive, reader: Some(reader), stream_id })
    }

    /// Sends queued media until asked to stop (Ok) or the connection breaks (Err).
    fn stream(&mut self, metadata: &Metadata, shared: &Shared) -> Result<(), String> {
        self.send(CS_COMMAND, DATA_AMF0, 0, &metadata_message(metadata))?;
        let configs = shared.queue.lock().unwrap().configs.clone();
        for config in &configs {
            self.send_item(config, shared)?;
        }
        loop {
            let batch: Vec<Item> = {
                let queue = shared.queue.lock().unwrap();
                let (mut queue, _) = shared
                    .wake
                    .wait_timeout_while(queue, Duration::from_millis(500), |q| q.items.is_empty() && !q.closing)
                    .unwrap();
                if queue.items.is_empty() && queue.closing {
                    break;
                }
                queue.items.drain(..).collect()
            };
            if !self.alive.load(Ordering::Relaxed) {
                return Err("the server closed the connection".into());
            }
            for item in &batch {
                self.send_item(item, shared)?;
            }
        }
        // Stopping: tell the server, then hang up.
        let goodbye = |name: &str, txn: f64, args: Vec<Value>| {
            let mut out = self.writer.lock().unwrap();
            send_command(&mut *out, 0, name, txn, Value::Null, args)
        };
        let _ = goodbye("FCUnpublish", 6.0, vec![]);
        let _ = goodbye("deleteStream", 7.0, vec![Value::Number(self.stream_id as f64)]);
        Ok(())
    }

    fn send_item(&self, item: &Item, shared: &Shared) -> Result<(), String> {
        let chunk_stream = if item.kind == AUDIO { CS_AUDIO } else { CS_VIDEO };
        self.send(chunk_stream, item.kind, item.ms, &item.body)?;
        shared.sent_bytes.fetch_add(item.body.len() as u64, Ordering::Relaxed);
        Ok(())
    }

    fn send(&self, chunk_stream: u8, kind: u8, ms: u32, payload: &[u8]) -> Result<(), String> {
        let bytes = message(chunk_stream, kind, self.stream_id, ms, payload);
        let started = Instant::now();
        self.writer.lock().unwrap().write_all(&bytes).map_err(|e| {
            if started.elapsed() >= TIMEOUT {
                "the network stalled for 10 seconds".to_string()
            } else {
                format!("connection lost: {e}")
            }
        })
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.socket.shutdown(std::net::Shutdown::Both);
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

/// The simple (unencrypted) RTMP handshake: C0+C1, read S0+S1, echo S1 as C2, read S2.
fn handshake(socket: &TcpStream) -> io::Result<()> {
    let mut socket = socket;
    let mut c0c1 = vec![0u8; 1537];
    c0c1[0] = 3; // RTMP version
    // C1: time (4 bytes, zero is fine) + zeros (4) + 1528 bytes of noise.
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    let mut seed = now.as_nanos() as u64 | 1;
    for byte in &mut c0c1[9..] {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        *byte = seed as u8;
    }
    socket.write_all(&c0c1)?;
    let mut s0s1 = vec![0u8; 1537];
    socket.read_exact(&mut s0s1)?;
    if s0s1[0] != 3 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "not an RTMP server"));
    }
    socket.write_all(&s0s1[1..])?;
    let mut s2 = vec![0u8; 1536];
    socket.read_exact(&mut s2)
}

/// One RTMP message as chunks: a full (type 0) header, then type 3 continuations.
fn message(chunk_stream: u8, kind: u8, stream_id: u32, ms: u32, payload: &[u8]) -> Vec<u8> {
    let extended = ms >= 0xFF_FFFF;
    let mut out = Vec::with_capacity(payload.len() + 16 + payload.len() / CHUNK_SIZE * 5);
    out.push(chunk_stream);
    out.extend_from_slice(&(if extended { 0xFF_FFFF } else { ms }).to_be_bytes()[1..]);
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes()[1..]);
    out.push(kind);
    out.extend_from_slice(&stream_id.to_le_bytes());
    if extended {
        out.extend_from_slice(&ms.to_be_bytes());
    }
    for (i, chunk) in payload.chunks(CHUNK_SIZE).enumerate() {
        if i > 0 {
            out.push(0xC0 | chunk_stream);
            if extended {
                out.extend_from_slice(&ms.to_be_bytes());
            }
        }
        out.extend_from_slice(chunk);
    }
    out
}

fn send_command(out: &mut impl Write, stream_id: u32, name: &str, txn: impl Into<f64>, object: Value, args: Vec<Value>) -> io::Result<()> {
    let mut values = vec![Value::str(name), Value::Number(txn.into()), object];
    values.extend(args);
    out.write_all(&message(CS_COMMAND, COMMAND_AMF0, stream_id, 0, &amf::encode(&values)))
}

fn metadata_message(m: &Metadata) -> Vec<u8> {
    let mut props = vec![
        ("duration".into(), Value::Number(0.0)),
        ("width".into(), Value::Number(m.width as f64)),
        ("height".into(), Value::Number(m.height as f64)),
        ("framerate".into(), Value::Number(m.fps as f64)),
        ("videocodecid".into(), Value::Number(7.0)),
        ("videodatarate".into(), Value::Number(m.video_kbps as f64)),
        ("encoder".into(), Value::str(concat!("MilerCast ", env!("CARGO_PKG_VERSION")))),
    ];
    if let Some(kbps) = m.audio_kbps {
        props.extend([
            ("audiocodecid".into(), Value::Number(10.0)),
            ("audiodatarate".into(), Value::Number(kbps as f64)),
            ("audiosamplerate".into(), Value::Number(48_000.0)),
            ("audiosamplesize".into(), Value::Number(16.0)),
            ("stereo".into(), Value::Bool(true)),
        ]);
    }
    amf::encode(&[Value::str("@setDataFrame"), Value::str("onMetaData"), Value::EcmaArray(props)])
}

/// Waits for the server's `_result` to transaction `txn`.
fn reply(reader: &mut ChunkReader<impl Read>, writer: &mut impl Write, txn: u32, stage: &str) -> Result<Vec<Value>, String> {
    loop {
        let values = next_command(reader, writer, stage)?;
        let name = values.first().and_then(Value::as_str).unwrap_or_default();
        let id = values.get(1).and_then(Value::as_number).unwrap_or(-1.0);
        if id == txn as f64 {
            match name {
                "_result" => return Ok(values),
                "_error" => return Err(format!("the server refused: {}", describe(&values))),
                _ => {}
            }
        }
    }
}

fn wait_for_publish(reader: &mut ChunkReader<impl Read>, writer: &mut impl Write) -> Result<(), String> {
    loop {
        let values = next_command(reader, writer, "publish")?;
        if values.first().and_then(Value::as_str) != Some("onStatus") {
            continue;
        }
        let info = values.get(3);
        let code = info.and_then(|i| i.get("code")).and_then(Value::as_str).unwrap_or_default();
        if code == "NetStream.Publish.Start" {
            return Ok(());
        }
        if info.and_then(|i| i.get("level")).and_then(Value::as_str) == Some("error") {
            return Err(format!("the server refused the stream (check the stream key): {}", describe(&values)));
        }
    }
}

/// Reads until the next command message, handling protocol messages on the way.
/// `stage` names the step we're waiting on, so errors say where things went wrong.
fn next_command(reader: &mut ChunkReader<impl Read>, writer: &mut impl Write, stage: &str) -> Result<Vec<Value>, String> {
    loop {
        let msg = reader.read_message().map_err(|e| match (e.kind(), stage) {
            (io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock, _) => format!("the server stopped answering ({stage})"),
            // Twitch and YouTube simply hang up on an unknown stream key.
            (io::ErrorKind::UnexpectedEof, "publish") => "the server refused the stream key".to_string(),
            (io::ErrorKind::UnexpectedEof, _) => format!("the server hung up ({stage})"),
            _ => format!("connection lost ({stage}): {e}"),
        })?;
        handle_control(&msg, reader, writer).map_err(|e| format!("connection lost ({stage}): {e}"))?;
        if msg.kind == COMMAND_AMF0 {
            return Ok(amf::decode(&msg.payload));
        }
    }
}

fn describe(values: &[Value]) -> String {
    let info = values.iter().find(|v| v.get("code").is_some());
    let field = |key| info.and_then(|i| i.get(key)).and_then(Value::as_str).unwrap_or_default();
    match (field("code"), field("description")) {
        ("", "") => "no details given".into(),
        (code, "") => code.into(),
        (code, description) => format!("{code} ({description})"),
    }
}

/// Protocol housekeeping: chunk size changes, pings and acknowledgements.
fn handle_control(msg: &Message, reader: &mut ChunkReader<impl Read>, writer: &mut impl Write) -> io::Result<()> {
    let u32_at = |i: usize| msg.payload.get(i..i + 4).map(|b| u32::from_be_bytes(b.try_into().unwrap()));
    match msg.kind {
        SET_CHUNK_SIZE => {
            if let Some(size) = u32_at(0) {
                reader.chunk_size = (size & 0x7FFF_FFFF).max(1) as usize;
            }
        }
        WINDOW_ACK_SIZE => reader.ack_window = u32_at(0).unwrap_or(0) as u64,
        USER_CONTROL if msg.payload.get(..2) == Some(&[0, 6]) => {
            // Ping request: answer with a ping response carrying the same timestamp.
            let mut pong = vec![0, 7];
            pong.extend_from_slice(msg.payload.get(2..6).unwrap_or(&[0; 4]));
            writer.write_all(&message(CS_CONTROL, USER_CONTROL, 0, 0, &pong))?;
        }
        _ => {}
    }
    if reader.ack_window > 0 && reader.bytes_read - reader.last_ack >= reader.ack_window {
        reader.last_ack = reader.bytes_read;
        writer.write_all(&message(CS_CONTROL, ACK, 0, 0, &(reader.bytes_read as u32).to_be_bytes()))?;
    }
    Ok(())
}

struct Message {
    kind: u8,
    payload: Vec<u8>,
}

#[derive(Default)]
struct ChunkHeader {
    length: u32,
    kind: u8,
    extended: bool,
    payload: Vec<u8>,
}

/// Reassembles RTMP messages from the server's chunk stream.
struct ChunkReader<R: Read> {
    input: R,
    chunk_size: usize,
    headers: HashMap<u32, ChunkHeader>,
    bytes_read: u64,
    ack_window: u64,
    last_ack: u64,
}

impl<R: Read> ChunkReader<R> {
    fn new(input: R) -> Self {
        Self { input, chunk_size: 128, headers: HashMap::new(), bytes_read: 0, ack_window: 0, last_ack: 0 }
    }

    fn bytes<const N: usize>(&mut self) -> io::Result<[u8; N]> {
        let mut buf = [0u8; N];
        self.input.read_exact(&mut buf)?;
        self.bytes_read += N as u64;
        Ok(buf)
    }

    fn read_message(&mut self) -> io::Result<Message> {
        loop {
            let [first] = self.bytes::<1>()?;
            let format = first >> 6;
            let chunk_stream = match first & 0x3F {
                0 => 64 + self.bytes::<1>()?[0] as u32,
                1 => {
                    let [low, high] = self.bytes::<2>()?;
                    64 + low as u32 + high as u32 * 256
                }
                id => id as u32,
            };
            let mut header = self.headers.remove(&chunk_stream).unwrap_or_default();
            if format <= 2 {
                let [a, b, c] = self.bytes::<3>()?;
                let time = u32::from_be_bytes([0, a, b, c]);
                if format <= 1 {
                    let [a, b, c] = self.bytes::<3>()?;
                    header.length = u32::from_be_bytes([0, a, b, c]);
                    header.kind = self.bytes::<1>()?[0];
                }
                if format == 0 {
                    self.bytes::<4>()?; // message stream ID
                }
                header.extended = time == 0xFF_FFFF;
                if header.extended {
                    self.bytes::<4>()?;
                }
            } else if header.extended {
                self.bytes::<4>()?;
            }
            // We never need the server's timestamps, only the message boundaries.
            let wanted = (header.length as usize).saturating_sub(header.payload.len()).min(self.chunk_size);
            let start = header.payload.len();
            header.payload.resize(start + wanted, 0);
            self.input.read_exact(&mut header.payload[start..])?;
            self.bytes_read += wanted as u64;
            if header.payload.len() >= header.length as usize {
                let msg = Message { kind: header.kind, payload: std::mem::take(&mut header.payload) };
                self.headers.insert(chunk_stream, header);
                return Ok(msg);
            }
            self.headers.insert(chunk_stream, header);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_server_addresses() {
        let t = Target::parse("youtube").unwrap();
        assert_eq!((t.host.as_str(), t.port, t.app.as_str()), ("a.rtmp.youtube.com", 1935, "live2"));
        let t = Target::parse("rtmp://127.0.0.1:1940/live/").unwrap();
        assert_eq!((t.host.as_str(), t.port, t.app.as_str()), ("127.0.0.1", 1940, "live"));
        assert_eq!(t.tc_url, "rtmp://127.0.0.1:1940/live");
        assert!(Target::parse("rtmps://x/app").is_err());
        assert!(Target::parse("rtmp://hostonly").is_err());
    }

    #[test]
    fn chunks_big_messages_and_reads_them_back() {
        let payload: Vec<u8> = (0..10_000u32).map(|i| i as u8).collect();
        let bytes = message(CS_VIDEO, VIDEO, 1, 1234, &payload);
        // 12-byte header + 10000 bytes + 2 continuation headers (4096-byte chunks).
        assert_eq!(bytes.len(), 12 + 10_000 + 2);
        let mut reader = ChunkReader::new(&bytes[..]);
        reader.chunk_size = CHUNK_SIZE;
        let msg = reader.read_message().unwrap();
        assert_eq!((msg.kind, msg.payload), (VIDEO, payload));
    }

    #[test]
    fn extended_timestamps() {
        let bytes = message(CS_AUDIO, AUDIO, 1, 0x0100_0000, &[1, 2, 3]);
        assert_eq!(&bytes[1..4], &[0xFF, 0xFF, 0xFF]);
        assert_eq!(&bytes[12..16], &0x0100_0000u32.to_be_bytes());
        let msg = ChunkReader::new(&bytes[..]).read_message().unwrap();
        assert_eq!(msg.payload, vec![1, 2, 3]);
    }

    fn item(kind: u8, ms: u32, keyframe: bool) -> Item {
        Item { kind, ms, body: vec![0; 10], keyframe, config: false }
    }

    #[test]
    fn congestion_drops_video_until_the_next_keyframe() {
        let mut q = Queue::default();
        q.push(Item { config: true, ..item(VIDEO, 0, true) });
        q.push(item(VIDEO, 0, true));
        for ms in (33..1500).step_by(33) {
            q.push(item(VIDEO, ms, false));
            q.push(item(AUDIO, ms, false));
        }
        // This frame pushes the backlog past the limit: queued video goes.
        let dropped = q.push(item(VIDEO, 1600, false));
        assert!(dropped > 40);
        assert!(q.items.iter().all(|i| i.kind == AUDIO || i.config));
        q.items.retain(|i| i.config); // the network catches up and sends the audio
        // Until a keyframe arrives, new P-frames are dropped too.
        assert_eq!(q.push(item(VIDEO, 1633, false)), 1);
        assert_eq!(q.push(item(VIDEO, 1666, true)), 0);
        assert_eq!(q.push(item(VIDEO, 1700, false)), 0);
    }
}

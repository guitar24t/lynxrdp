//! Private, read-only WebDAV endpoint for macOS's built-in filesystem.
//! Metadata methods never call Source::contents. Only GET can request bytes.
//!
//! The listener is loopback TCP, which every local uid can reach, and the URL
//! is public knowledge on the machine: mount_webdav records it as the mount's
//! `f_mntfromname` and execs webdavfs_agent with it in argv, both of which
//! any user can read. So the URL is not the capability. Every request must
//! carry HTTP Basic credentials whose password is generated here from OS
//! randomness and reaches webdavfs_agent through a file descriptor rather than
//! its command line; anything without them is answered 401 before the path is
//! even looked at. The random path prefix stays as a second layer.
use crate::source::Source;
use anyhow::{Context, Result};
use std::{
    io::{BufRead, BufReader, Read, Seek, SeekFrom, Write},
    net::{TcpListener, TcpStream},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

/// The Basic-auth user name presented for every mount. It is not a secret;
/// the per-mount password is.
pub(crate) const USER: &str = "lynxrdp";

/// Whole request head and body must arrive within this much of the accept.
/// The per-read socket timeout alone restarts on every byte, so one
/// connection trickling a byte at a time could hold a worker for hours.
const REQUEST_DEADLINE: Duration = Duration::from_secs(10);

/// PROPFIND bodies are a few hundred bytes; anything larger is refused.
const BODY_LIMIT: u64 = 64 * 1024;

pub(crate) struct Server {
    pub url: String,
    pub password: String,
    stop: Arc<AtomicBool>,
}

/// Everything a worker needs to answer one request.
struct Endpoint {
    source: Arc<Source>,
    prefix: String,
    /// The exact `Authorization` token a request must carry: the base64 of
    /// `user:password`, compared as bytes so no decoder runs on attacker input.
    credential: String,
    deadline: Duration,
}

impl Server {
    pub fn start(source: Arc<Source>) -> Result<Self> {
        Self::start_with_deadline(source, REQUEST_DEADLINE)
    }

    fn start_with_deadline(source: Arc<Source>, deadline: Duration) -> Result<Self> {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
        listener.set_nonblocking(true)?;
        // TempDir uses a random component; add independent randomness for the URL prefix.
        let secret = tempfile::Builder::new()
            .prefix("token-")
            .rand_bytes(24)
            .tempdir_in(source.directory.path())?;
        let prefix = format!("/{}/", secret.path().file_name().unwrap().to_string_lossy());
        let url = format!("http://{}{}", listener.local_addr()?, prefix);
        let password = password()?;
        let endpoint = Arc::new(Endpoint {
            source,
            prefix,
            credential: base64(format!("{USER}:{password}").as_bytes()),
            deadline,
        });
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        std::thread::Builder::new()
            .name("clipboard-webdav".into())
            .spawn(move || {
                let _secret = secret;
                // A small worker pool prevents an idle socket from blocking metadata.
                let (tx, rx) = crossbeam_channel::bounded::<(TcpStream, Instant)>(16);
                for _ in 0..4 {
                    let rx = rx.clone();
                    let endpoint = endpoint.clone();
                    std::thread::spawn(move || {
                        while let Ok((stream, accepted)) = rx.recv() {
                            if let Err(e) = serve(stream, accepted, &endpoint) {
                                log::debug!("clipboard WebDAV request: {e:#}");
                            }
                        }
                    });
                }
                while !stopping.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let _ = tx.try_send((stream, Instant::now()));
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(10))
                        }
                        // ECONNABORTED, or EMFILE under launchd's 256-descriptor
                        // limit, clears once the pressure does. Leaving the loop
                        // would keep the volume mounted with nothing answering
                        // it, so only the stop flag ends the listener.
                        Err(e) => {
                            log::debug!("clipboard WebDAV accept: {e}");
                            std::thread::sleep(Duration::from_millis(100))
                        }
                    }
                }
            })?;
        Ok(Self {
            url,
            password,
            stop,
        })
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}

/// 32 characters of 6 bits each from the OS generator. tempfile's names come
/// from fastrand, whose state can be recovered from one output, and one output
/// (the URL prefix) is in the mount table for every local user to read.
fn password() -> Result<String> {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes)
        .map_err(|e| anyhow::anyhow!("Generating a clipboard mount password: {e}"))?;
    Ok(bytes
        .iter()
        .map(|b| ALPHABET[(b & 63) as usize] as char)
        .collect())
}

pub(crate) fn base64(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(chunk.get(1).copied().unwrap_or(0)) << 8)
            | u32::from(chunk.get(2).copied().unwrap_or(0));
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// A loopback peer can time a comparison, so the token check must not stop
/// at the first differing byte.
fn authorized(header: Option<&str>, credential: &str) -> bool {
    let Some((scheme, token)) = header
        .and_then(|value| value.trim().split_once(char::is_whitespace))
        .map(|(scheme, token)| (scheme, token.trim()))
    else {
        return false;
    };
    if !scheme.eq_ignore_ascii_case("Basic") || token.len() != credential.len() {
        return false;
    }
    token
        .bytes()
        .zip(credential.bytes())
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

fn encode(name: &str) -> String {
    let mut out = String::new();
    for b in name.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// macOS's WebDAV client escapes with CFURL, which leaves `(`, `)`, `'` and
/// `!` literal where `encode` would escape them, so a request is matched by
/// its decoded bytes rather than by re-encoding the name. A `%` that does not
/// start two hex digits is refused rather than passed through; nothing that
/// legitimately talks to this server sends one.
fn decode(component: &str) -> Option<Vec<u8>> {
    let bytes = component.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let high = (*bytes.get(i + 1)? as char).to_digit(16)?;
            let low = (*bytes.get(i + 2)? as char).to_digit(16)?;
            out.push((high * 16 + low) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    Some(out)
}
fn xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}
fn prop(href: &str, name: &str, size: Option<u64>) -> String {
    let kind = if size.is_none() {
        "<D:collection/>"
    } else {
        ""
    };
    format!("<D:response><D:href>{}</D:href><D:propstat><D:prop><D:displayname>{}</D:displayname><D:resourcetype>{kind}</D:resourcetype><D:getcontentlength>{}</D:getcontentlength><D:creationdate>2026-01-01T00:00:00Z</D:creationdate><D:getcontenttype>application/octet-stream</D:getcontenttype><D:getlastmodified>Thu, 01 Jan 2026 00:00:00 GMT</D:getlastmodified><D:getetag>\"{}\"</D:getetag><D:supportedlock/></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>",xml(href),xml(name),size.unwrap_or(0),size.unwrap_or(0))
}
fn header(stream: &mut TcpStream, status: &str, len: u64, extra: &str) -> Result<()> {
    write!(stream,"HTTP/1.1 {status}\r\nContent-Length: {len}\r\nConnection: close\r\nDAV: 1\r\nAllow: OPTIONS, PROPFIND, HEAD, GET\r\n{extra}\r\n")?;
    Ok(())
}
/// HTTP byte ranges may extend beyond EOF. Native uncached reads use a
/// buffer-sized range, so rejecting those prevents even requesting the file.
fn byte_range(range: &str, size: u64) -> Option<(u64, u64)> {
    let last = size.checked_sub(1)?;
    let (start, end) = range.strip_prefix("bytes=")?.split_once('-')?;
    if start.is_empty() {
        let count = end.parse::<u64>().ok()?;
        return (count > 0).then_some((size.saturating_sub(count), last));
    }
    let start = start.parse::<u64>().ok()?;
    let end = if end.is_empty() {
        last
    } else {
        end.parse::<u64>().ok()?.min(last)
    };
    (start <= end && start < size).then_some((start, end))
}

/// Re-arms the socket's read timeout with what is left of the request's
/// budget, so that the budget rather than the last byte bounds the wait.
fn arm_read_timeout(stream: &TcpStream, deadline: Instant) -> Result<()> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    anyhow::ensure!(!remaining.is_zero(), "Request did not arrive in time");
    stream.set_read_timeout(Some(remaining))?;
    Ok(())
}

/// Drains a bounded body within what is left of the request's budget.
/// Leaving unread socket data can reset the response on macOS's HTTP client.
fn drain(
    stream: &TcpStream,
    reader: &mut BufReader<TcpStream>,
    deadline: Instant,
    length: u64,
) -> Result<()> {
    arm_read_timeout(stream, deadline)?;
    let consumed = std::io::copy(&mut reader.take(length), &mut std::io::sink())?;
    anyhow::ensure!(consumed == length, "Incomplete HTTP body");
    Ok(())
}

fn serve(mut stream: TcpStream, accepted: Instant, endpoint: &Endpoint) -> Result<()> {
    let Endpoint {
        source,
        prefix,
        credential,
        deadline,
    } = endpoint;
    let prefix = prefix.as_str();
    let deadline = accepted + *deadline;
    // macOS inherits the listener's nonblocking mode on accepted sockets.
    // Workers use timed blocking reads, including between fragmented headers;
    // otherwise a perfectly ordinary pause is WouldBlock and resets the copy.
    stream.set_nonblocking(false)?;
    stream.set_write_timeout(Some(Duration::from_secs(30)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut data = Vec::new();
    loop {
        arm_read_timeout(&stream, deadline)?;
        let before = data.len();
        (&mut reader)
            .take((16 * 1024 - data.len()) as u64)
            .read_until(b'\n', &mut data)?;
        anyhow::ensure!(
            data.len() > before && data.len() < 16 * 1024,
            "Invalid HTTP headers"
        );
        if data.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    let text = std::str::from_utf8(&data)?;
    let mut lines = text.split("\r\n");
    let mut request = lines.next().context("Missing request")?.split_whitespace();
    let method = request.next().unwrap_or("");
    let path = request.next().unwrap_or("");
    let fields: Vec<_> = lines.filter_map(|line| line.split_once(':')).collect();
    let field = |name: &str| {
        fields
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.trim())
    };
    let chunked = field("Transfer-Encoding").is_some();
    let expects_continue = field("Expect").is_some_and(|v| v.eq_ignore_ascii_case("100-continue"));
    let length = match field("Content-Length") {
        None => Some(0),
        Some(value) => value.parse::<u64>().ok(),
    };
    if !authorized(field("Authorization"), credential) {
        // Refused before the path is examined, so a probe cannot tell a real
        // prefix from a guess. The body is still drained when it is bounded and
        // already on its way: a client that asked for 100-continue never sends
        // one after a 401, and the reset that unread bytes cause would reach
        // webdavfs before the challenge does.
        if !chunked && !expects_continue {
            if let Some(length) = length.filter(|&length| length <= BODY_LIMIT) {
                drain(&stream, &mut reader, deadline, length)?;
            }
        }
        return header(
            &mut stream,
            "401 Unauthorized",
            0,
            "WWW-Authenticate: Basic realm=\"LynxRDP copied files\"\r\n",
        );
    }
    let root = path == prefix || path == prefix.trim_end_matches('/');
    let index = if root {
        None
    } else {
        path.strip_prefix(prefix)
            .and_then(decode)
            .filter(|name| !name.contains(&b'/') && !name.contains(&0))
            .and_then(|name| source.names.iter().position(|n| n.as_bytes() == name))
    };
    if !root && index.is_none() {
        return header(&mut stream, "404 Not Found", 0, "");
    }
    if chunked {
        return header(&mut stream, "400 Bad Request", 0, "");
    }
    if expects_continue {
        stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n")?;
    }
    let length = length.context("Invalid Content-Length")?;
    if length > BODY_LIMIT {
        return header(&mut stream, "413 Content Too Large", 0, "");
    }
    drain(&stream, &mut reader, deadline, length)?;
    match method {
        "OPTIONS" => header(&mut stream, "200 OK", 0, ""),
        "PROPFIND" => {
            let mut body = String::from(
                "<?xml version=\"1.0\" encoding=\"utf-8\"?><D:multistatus xmlns:D=\"DAV:\">",
            );
            if root {
                body.push_str(&prop(prefix, "Copied files", None));
                if field("Depth") != Some("0") {
                    for (i, name) in source.names.iter().enumerate() {
                        body.push_str(&prop(
                            &format!("{prefix}{}", encode(name)),
                            name,
                            Some(source.files[i].size),
                        ));
                    }
                }
            } else {
                let i = index.unwrap();
                body.push_str(&prop(
                    &format!("{prefix}{}", encode(&source.names[i])),
                    &source.names[i],
                    Some(source.files[i].size),
                ));
            }
            body.push_str("</D:multistatus>");
            header(
                &mut stream,
                "207 Multi-Status",
                body.len() as u64,
                "Content-Type: application/xml; charset=utf-8\r\n",
            )?;
            stream.write_all(body.as_bytes())?;
            Ok(())
        }
        "HEAD" => header(
            &mut stream,
            "200 OK",
            index.map(|i| source.files[i].size).unwrap_or(0),
            "Accept-Ranges: bytes\r\n",
        ),
        "GET" if index.is_some() => {
            let i = index.unwrap();
            let size = source.files[i].size;
            let range = field("Range");
            let (start, end) = if let Some(range) = range {
                let parsed = byte_range(range, size);
                let Some(r) = parsed else {
                    return header(
                        &mut stream,
                        "416 Range Not Satisfiable",
                        0,
                        &format!("Content-Range: bytes */{size}\r\n"),
                    );
                };
                r
            } else {
                (0, size.saturating_sub(1))
            };
            let path = match source.contents(i) {
                Ok(p) => p,
                Err(e) => {
                    log::warn!("paste could not fetch file: {e:#}");
                    return header(&mut stream, "503 Service Unavailable", 0, "");
                }
            };
            let mut file = std::fs::File::open(path)?;
            file.seek(SeekFrom::Start(start))?;
            let len = if size == 0 { 0 } else { end - start + 1 };
            let extra = if range.is_some() {
                format!("Content-Range: bytes {start}-{end}/{size}\r\n")
            } else {
                String::new()
            };
            header(
                &mut stream,
                if range.is_some() {
                    "206 Partial Content"
                } else {
                    "200 OK"
                },
                len,
                &extra,
            )?;
            std::io::copy(&mut file.take(len), &mut stream)?;
            Ok(())
        }
        _ => header(&mut stream, "405 Method Not Allowed", 0, ""),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What a client that holds the mount's credential sends.
    #[derive(Clone)]
    struct Client {
        url: String,
        authorization: String,
    }
    impl Client {
        fn of(server: &Server) -> Self {
            Self {
                url: server.url.clone(),
                authorization: format!(
                    "Authorization: Basic {}\r\n",
                    base64(format!("{USER}:{}", server.password).as_bytes())
                ),
            }
        }
        fn request(&self, method: &str, suffix: &str, extra: &str) -> String {
            raw_request(
                &self.url,
                method,
                suffix,
                &format!("{}{extra}", self.authorization),
            )
        }
    }
    fn raw_request(url: &str, method: &str, suffix: &str, extra: &str) -> String {
        let rest = url.strip_prefix("http://").unwrap();
        let (addr, path) = rest.split_once('/').unwrap();
        let mut socket = TcpStream::connect(addr).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        write!(
            socket,
            "{method} /{path}{suffix} HTTP/1.1\r\nHost: {addr}\r\n{extra}\r\n"
        )
        .unwrap();
        let mut response = String::new();
        socket.read_to_string(&mut response).unwrap();
        response
    }
    type Served = (
        tempfile::TempDir,
        Server,
        crossbeam_channel::Receiver<crate::Fetch>,
    );
    fn serve_one(name: &str, size: u64) -> Served {
        let dir = tempfile::tempdir().unwrap();
        let (source, requests) = Source::new(
            dir.path(),
            &[lynxrdp_proto::FileEntry {
                path: name.into(),
                size,
            }],
        )
        .unwrap();
        (dir, Server::start(source).unwrap(), requests)
    }

    #[test]
    fn base64_matches_the_reference_vectors() {
        assert_eq!(base64(b"lynxrdp:pw"), "bHlueHJkcDpwdw==");
        assert_eq!(base64(b"a"), "YQ==");
        assert_eq!(base64(b"ab"), "YWI=");
        assert_eq!(base64(b"abc"), "YWJj");
        assert_eq!(base64(b""), "");
    }

    #[test]
    fn decode_refuses_malformed_escapes_and_keeps_literals() {
        assert_eq!(decode("x%20(2).txt").unwrap(), b"x (2).txt");
        assert_eq!(decode("it's!.txt").unwrap(), b"it's!.txt");
        assert_eq!(decode("a%2Fb").unwrap(), b"a/b");
        assert!(decode("100%").is_none());
        assert!(decode("%4").is_none());
        assert!(decode("%+4").is_none());
        assert!(decode("%zz").is_none());
    }

    #[test]
    fn a_connection_can_wait_for_fragmented_request_headers() {
        let (_dir, server, requests) = serve_one("small.txt", 5);
        let client = Client::of(&server);
        let (addr, path) = server
            .url
            .strip_prefix("http://")
            .unwrap()
            .split_once('/')
            .unwrap();
        let mut socket = TcpStream::connect(addr).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        // A TCP accept need not arrive with request bytes. Leave time for a
        // worker to begin reading, then split the headers across two writes.
        std::thread::sleep(Duration::from_millis(50));
        write!(socket, "HEAD /{path}small.txt HTTP/1.1\r\n").unwrap();
        std::thread::sleep(Duration::from_millis(50));
        write!(socket, "Host: {addr}\r\n{}\r\n", client.authorization).unwrap();
        let mut response = String::new();
        socket.read_to_string(&mut response).unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(response.contains("Content-Length: 5\r\n"));
        assert!(requests.is_empty(), "metadata must not fetch file contents");
    }

    #[test]
    fn the_url_alone_is_refused_with_a_challenge() {
        let (_dir, server, requests) = serve_one("/remote/secret.txt", 5);
        for method in ["OPTIONS", "PROPFIND", "HEAD", "GET"] {
            let response = raw_request(&server.url, method, "secret.txt", "");
            assert!(
                response.starts_with("HTTP/1.1 401 Unauthorized\r\n"),
                "{method}: {response}"
            );
            assert!(response.contains("WWW-Authenticate: Basic realm="));
            assert!(!response.contains("secret.txt"));
        }
        // A PROPFIND body travels with its headers; refusing it must still
        // read it so the challenge, not a reset, is what the client sees.
        let body = "<?xml version=\"1.0\"?><D:propfind xmlns:D=\"DAV:\"><D:allprop/></D:propfind>";
        let response = raw_request(
            &server.url,
            "PROPFIND",
            "",
            // The helper ends every request with CRLF; count it as body so no
            // unread byte is left behind to turn the close into a reset.
            &format!(
                "Depth: 1\r\nContent-Length: {}\r\n\r\n{body}",
                body.len() + 2
            ),
        );
        assert!(response.starts_with("HTTP/1.1 401"));
        // The wrong password and the wrong scheme are the same as none, and a
        // guessed path is answered no differently from the real one.
        let wrong = format!(
            "Authorization: Basic {}\r\n",
            base64(format!("{USER}:{}x", server.password).as_bytes())
        );
        assert!(raw_request(&server.url, "GET", "secret.txt", &wrong).starts_with("HTTP/1.1 401"));
        let right = Client::of(&server);
        let bearer = right.authorization.replace("Basic", "Bearer");
        assert!(raw_request(&server.url, "GET", "secret.txt", &bearer).starts_with("HTTP/1.1 401"));
        assert!(raw_request(&server.url, "GET", "nope.txt", "").starts_with("HTTP/1.1 401"));
        assert!(requests.is_empty(), "a refused request must not fetch");
        assert!(right
            .request("HEAD", "secret.txt", "")
            .starts_with("HTTP/1.1 200"));
        assert!(right
            .request("GET", "nope.txt", "")
            .starts_with("HTTP/1.1 404"));
        assert!(requests.is_empty());
    }

    #[test]
    fn metadata_is_lazy_and_get_fetches_once() {
        let (_dir, server, requests) = serve_one("/remote/a & b.txt", 5);
        let client = Client::of(&server);
        for method in ["OPTIONS", "PROPFIND", "HEAD"] {
            assert!(client
                .request(method, "", "Depth: 1\r\n")
                .contains("HTTP/1.1 20"));
            assert!(requests.is_empty());
        }
        let listing = client.request("PROPFIND", "", "");
        assert!(listing.contains("a%20%26%20b.txt"));
        assert!(listing.contains("a &amp; b.txt"));
        assert!(client.request("GET", "../escape", "").contains("404"));
        assert!(requests.is_empty());
        assert!(client.request("PUT", "a%20%26%20b.txt", "").contains("405"));
        assert!(requests.is_empty());
        let reader = client.clone();
        let read = std::thread::spawn(move || {
            reader.request("GET", "a%20%26%20b.txt", "Range: bytes=1-3\r\n")
        });
        let fetch = requests.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(fetch.remote, "/remote/a & b.txt");
        std::fs::write(&fetch.destination, b"hello").unwrap();
        fetch
            .result
            .send(crate::FetchReply::Done(fetch.destination))
            .unwrap();
        assert!(read.join().unwrap().ends_with("ell"));
        assert!(client
            .request("GET", "a%20%26%20b.txt", "")
            .ends_with("hello"));
        assert!(requests.is_empty());
    }

    #[test]
    fn names_are_matched_as_the_macos_client_escapes_them() {
        let dir = tempfile::tempdir().unwrap();
        let (source, requests) = Source::new(
            dir.path(),
            &[
                lynxrdp_proto::FileEntry {
                    path: "/a/x.txt".into(),
                    size: 1,
                },
                lynxrdp_proto::FileEntry {
                    path: "/b/x.txt".into(),
                    size: 2,
                },
                lynxrdp_proto::FileEntry {
                    path: "/c/it's!.txt".into(),
                    size: 3,
                },
            ],
        )
        .unwrap();
        assert_eq!(source.names[1], "x (2).txt");
        let server = Server::start(source).unwrap();
        let client = Client::of(&server);
        // CFURL leaves ( ) ' ! literal and escapes only the space.
        let single = client.request("PROPFIND", "x%20(2).txt", "Depth: 0\r\n");
        assert!(single.starts_with("HTTP/1.1 207"), "{single}");
        assert!(single.contains("x%20%282%29.txt"));
        let head = client.request("HEAD", "x%20(2).txt", "");
        assert!(head.contains("Content-Length: 2\r\n"), "{head}");
        let head = client.request("HEAD", "it's!.txt", "");
        assert!(head.contains("Content-Length: 3\r\n"), "{head}");
        // The fully escaped spelling this server itself lists still resolves.
        let head = client.request("HEAD", "x%20%282%29.txt", "");
        assert!(head.contains("Content-Length: 2\r\n"), "{head}");
        for bad in [
            "x%20(2).txt/",
            "x%2F(2).txt",
            "x%00(2).txt",
            "x%20(2).txt%",
            "x%zz(2).txt",
        ] {
            assert!(
                client.request("HEAD", bad, "").starts_with("HTTP/1.1 404"),
                "{bad}"
            );
        }
        assert!(requests.is_empty());
        let reader = client.clone();
        let read = std::thread::spawn(move || reader.request("GET", "it's!.txt", ""));
        let fetch = requests.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(fetch.remote, "/c/it's!.txt");
        std::fs::write(&fetch.destination, b"abc").unwrap();
        fetch
            .result
            .send(crate::FetchReply::Done(fetch.destination))
            .unwrap();
        assert!(read.join().unwrap().ends_with("abc"));
    }

    #[test]
    fn native_reads_can_extend_past_eof_and_read_suffixes() {
        let (_dir, server, requests) = serve_one("small.txt", 5);
        let client = Client::of(&server);
        let reader = client.clone();
        let worker = std::thread::spawn(move || {
            reader.request("GET", "small.txt", "Range: bytes=0-4095\r\n")
        });
        let fetch = requests
            .recv_timeout(Duration::from_secs(2))
            .expect("valid read must request contents");
        std::fs::write(&fetch.destination, b"hello").unwrap();
        fetch
            .result
            .send(crate::FetchReply::Done(fetch.destination))
            .unwrap();
        let response = worker.join().unwrap();
        assert!(response.starts_with("HTTP/1.1 206"));
        assert!(response.contains("Content-Range: bytes 0-4/5\r\n"));
        assert!(response.ends_with("hello"));
        assert!(client
            .request("GET", "small.txt", "Range: bytes=-2\r\n")
            .ends_with("lo"));
        assert!(client
            .request("GET", "small.txt", "Range: bytes=5-\r\n")
            .starts_with("HTTP/1.1 416"));
        assert!(requests.is_empty());
    }

    #[test]
    fn disconnect_releases_pending_get() {
        let (_dir, server, requests) = serve_one("a", 1);
        let client = Client::of(&server);
        let read = std::thread::spawn(move || client.request("GET", "a", ""));
        let fetch = requests.recv_timeout(Duration::from_secs(5)).unwrap();
        drop(fetch);
        drop(requests);
        drop(server);
        assert!(read.join().unwrap().contains("503 Service Unavailable"));
    }

    #[test]
    fn a_trickled_request_head_is_cut_off_at_the_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let (source, _requests) = Source::new(
            dir.path(),
            &[lynxrdp_proto::FileEntry {
                path: "a".into(),
                size: 1,
            }],
        )
        .unwrap();
        let deadline = Duration::from_millis(600);
        let server = Server::start_with_deadline(source, deadline).unwrap();
        let (addr, _) = server
            .url
            .strip_prefix("http://")
            .unwrap()
            .split_once('/')
            .unwrap();
        let mut socket = TcpStream::connect(addr).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let started = Instant::now();
        // Every byte lands well inside a per-read timeout; only a budget
        // measured from the accept can end this.
        let mut closed = false;
        for _ in 0..25 {
            if socket.write_all(b"G").is_err() {
                closed = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let mut response = String::new();
        let read = socket.read_to_string(&mut response);
        assert!(
            closed || matches!(read, Ok(0)) || read.is_err(),
            "connection should have been closed: {read:?} {response:?}"
        );
        let elapsed = started.elapsed();
        assert!(elapsed >= deadline, "{elapsed:?}");
        assert!(elapsed < Duration::from_secs(4), "{elapsed:?}");
    }
}

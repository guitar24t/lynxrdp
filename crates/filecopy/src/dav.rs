//! Private, read-only WebDAV endpoint for macOS's built-in filesystem.
//! Metadata methods never call Source::contents. Only GET can request bytes.
use crate::source::Source;
use anyhow::{Context, Result};
use std::{
    io::{BufRead, BufReader, Read, Seek, SeekFrom, Write},
    net::{TcpListener, TcpStream},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

pub(crate) struct Server {
    pub url: String,
    stop: Arc<AtomicBool>,
}
impl Server {
    pub fn start(source: Arc<Source>) -> Result<Self> {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
        listener.set_nonblocking(true)?;
        // TempDir uses a random component; add independent randomness for the URL capability.
        let secret = tempfile::Builder::new()
            .prefix("token-")
            .rand_bytes(24)
            .tempdir_in(source.directory.path())?;
        let prefix = format!("/{}/", secret.path().file_name().unwrap().to_string_lossy());
        let url = format!("http://{}{}", listener.local_addr()?, prefix);
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        std::thread::Builder::new()
            .name("clipboard-webdav".into())
            .spawn(move || {
                let _secret = secret;
                // A small worker pool prevents an idle socket from blocking metadata.
                let (tx, rx) = crossbeam_channel::bounded::<TcpStream>(16);
                for _ in 0..4 {
                    let rx = rx.clone();
                    let source = source.clone();
                    let prefix = prefix.clone();
                    std::thread::spawn(move || {
                        while let Ok(stream) = rx.recv() {
                            if let Err(e) = serve(stream, &source, &prefix) {
                                log::debug!("clipboard WebDAV request: {e:#}");
                            }
                        }
                    });
                }
                while !stopping.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let _ = tx.try_send(stream);
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(10))
                        }
                        Err(_) => break,
                    }
                }
            })?;
        Ok(Self { url, stop })
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
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

fn serve(mut stream: TcpStream, source: &Source, prefix: &str) -> Result<()> {
    // macOS inherits the listener's nonblocking mode on accepted sockets.
    // Workers use timed blocking reads, including between fragmented headers;
    // otherwise a perfectly ordinary pause is WouldBlock and resets the copy.
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(30)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut data = Vec::new();
    loop {
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
    let root = path == prefix || path == prefix.trim_end_matches('/');
    let index = source
        .names
        .iter()
        .position(|name| path == format!("{prefix}{}", encode(name)));
    if !root && index.is_none() {
        return header(&mut stream, "404 Not Found", 0, "");
    }
    let fields: Vec<_> = lines.filter_map(|line| line.split_once(':')).collect();
    let field = |name: &str| {
        fields
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.trim())
    };
    if field("Transfer-Encoding").is_some() {
        return header(&mut stream, "400 Bad Request", 0, "");
    }
    if field("Expect").is_some_and(|v| v.eq_ignore_ascii_case("100-continue")) {
        stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n")?;
    }
    // Drain bounded PROPFIND XML bodies before closing the connection. Leaving
    // unread socket data can reset the response on macOS's HTTP client.
    let length = field("Content-Length").unwrap_or("0").parse::<u64>()?;
    if length > 64 * 1024 {
        return header(&mut stream, "413 Content Too Large", 0, "");
    }
    let consumed = std::io::copy(&mut reader.take(length), &mut std::io::sink())?;
    anyhow::ensure!(consumed == length, "Incomplete HTTP body");
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
                body.push_str(&prop(path, &source.names[i], Some(source.files[i].size)));
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
    fn request(url: &str, method: &str, suffix: &str, extra: &str) -> String {
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

    #[test]
    fn a_connection_can_wait_for_fragmented_request_headers() {
        let dir = tempfile::tempdir().unwrap();
        let (source, requests) = Source::new(
            dir.path(),
            &[lynxrdp_proto::FileEntry {
                path: "small.txt".into(),
                size: 5,
            }],
        )
        .unwrap();
        let server = Server::start(source).unwrap();
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
        write!(socket, "Host: {addr}\r\n\r\n").unwrap();
        let mut response = String::new();
        socket.read_to_string(&mut response).unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(response.contains("Content-Length: 5\r\n"));
        assert!(requests.is_empty(), "metadata must not fetch file contents");
    }

    #[test]
    fn metadata_is_lazy_and_get_fetches_once() {
        let dir = tempfile::tempdir().unwrap();
        let (source, requests) = Source::new(
            dir.path(),
            &[lynxrdp_proto::FileEntry {
                path: "/remote/a & b.txt".into(),
                size: 5,
            }],
        )
        .unwrap();
        let server = Server::start(source).unwrap();
        for method in ["OPTIONS", "PROPFIND", "HEAD"] {
            assert!(request(&server.url, method, "", "Depth: 1\r\n").contains("HTTP/1.1 20"));
            assert!(requests.is_empty());
        }
        let listing = request(&server.url, "PROPFIND", "", "");
        assert!(listing.contains("a%20%26%20b.txt"));
        assert!(listing.contains("a &amp; b.txt"));
        assert!(request(&server.url, "GET", "../escape", "").contains("404"));
        assert!(requests.is_empty());
        assert!(request(&server.url, "PUT", "a%20%26%20b.txt", "").contains("405"));
        assert!(requests.is_empty());
        let url = server.url.clone();
        let read = std::thread::spawn(move || {
            request(&url, "GET", "a%20%26%20b.txt", "Range: bytes=1-3\r\n")
        });
        let fetch = requests.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(fetch.remote, "/remote/a & b.txt");
        std::fs::write(&fetch.destination, b"hello").unwrap();
        fetch.result.send(Some(fetch.destination)).unwrap();
        assert!(read.join().unwrap().ends_with("ell"));
        assert!(request(&server.url, "GET", "a%20%26%20b.txt", "").ends_with("hello"));
        assert!(requests.is_empty());
    }
    #[test]
    fn native_reads_can_extend_past_eof_and_read_suffixes() {
        let dir = tempfile::tempdir().unwrap();
        let (source, requests) = Source::new(
            dir.path(),
            &[lynxrdp_proto::FileEntry {
                path: "small.txt".into(),
                size: 5,
            }],
        )
        .unwrap();
        let server = Server::start(source).unwrap();
        let url = server.url.clone();
        let worker = std::thread::spawn(move || {
            request(&url, "GET", "small.txt", "Range: bytes=0-4095\r\n")
        });
        let fetch = requests
            .recv_timeout(Duration::from_secs(2))
            .expect("valid read must request contents");
        std::fs::write(&fetch.destination, b"hello").unwrap();
        fetch.result.send(Some(fetch.destination)).unwrap();
        let response = worker.join().unwrap();
        assert!(response.starts_with("HTTP/1.1 206"));
        assert!(response.contains("Content-Range: bytes 0-4/5\r\n"));
        assert!(response.ends_with("hello"));
        assert!(request(&server.url, "GET", "small.txt", "Range: bytes=-2\r\n").ends_with("lo"));
        assert!(
            request(&server.url, "GET", "small.txt", "Range: bytes=5-\r\n")
                .starts_with("HTTP/1.1 416")
        );
        assert!(requests.is_empty());
    }

    #[test]
    fn disconnect_releases_pending_get() {
        let dir = tempfile::tempdir().unwrap();
        let (source, requests) = Source::new(
            dir.path(),
            &[lynxrdp_proto::FileEntry {
                path: "a".into(),
                size: 1,
            }],
        )
        .unwrap();
        let server = Server::start(source).unwrap();
        let url = server.url.clone();
        let read = std::thread::spawn(move || request(&url, "GET", "a", ""));
        let fetch = requests.recv_timeout(Duration::from_secs(5)).unwrap();
        drop(fetch);
        drop(requests);
        drop(server);
        assert!(read.join().unwrap().contains("503 Service Unavailable"));
    }
}

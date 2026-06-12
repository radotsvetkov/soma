//! Minimal HTTP/1.1 client over `TcpStream` — plain `http://` only.
//!
//! This exists for one reason: talking to a local Ollama on 127.0.0.1 without
//! pulling in a TLS stack (R9/R18). Cloud HTTPS calls go through the system
//! `curl` instead (see models.rs), so TLS roots and proxies stay the OS's
//! problem. Supports Content-Length and chunked responses; nothing more.

use crate::util::R;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

#[derive(Debug)]
pub struct HttpResponse {
    pub status: u16,
    pub body: String,
}

/// Split `http://host[:port]/path` → (host, port, path).
pub fn parse_url(url: &str) -> R<(String, u16, String)> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| format!("only http:// urls are supported here (got {url})"))?;
    let (hostport, path) = match rest.find('/') {
        Some(i) => (&rest[..i], rest[i..].to_string()),
        None => (rest, "/".to_string()),
    };
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) => (
            h.to_string(),
            p.parse::<u16>().map_err(|_| format!("bad port in {url}"))?,
        ),
        None => (hostport.to_string(), 80),
    };
    if host.is_empty() {
        return Err(format!("no host in url {url}"));
    }
    Ok((host, port, path))
}

fn request(method: &str, url: &str, body: Option<&str>, timeout_s: u64) -> R<HttpResponse> {
    let (host, port, path) = parse_url(url)?;
    let timeout = Duration::from_secs(timeout_s.max(1));
    let addr = format!("{host}:{port}")
        .to_socket_addrs()
        .map_err(|e| format!("resolve {host}:{port}: {e}"))?
        .next()
        .ok_or_else(|| format!("no address for {host}:{port}"))?;
    let mut stream = TcpStream::connect_timeout(&addr, timeout)
        .map_err(|e| format!("connect {host}:{port}: {e}"))?;
    stream.set_read_timeout(Some(timeout)).ok();
    stream.set_write_timeout(Some(timeout)).ok();

    let body_bytes = body.unwrap_or("").as_bytes();
    // Single write: a peer that replies after one read() must not race our
    // second write into a reset connection.
    let mut req = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host}:{port}\r\nUser-Agent: soma/{}\r\nAccept: application/json\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        crate::project::SOMA_VERSION,
        body_bytes.len()
    )
    .into_bytes();
    req.extend_from_slice(body_bytes);
    stream
        .write_all(&req)
        .map_err(|e| format!("send request: {e}"))?;

    let mut raw = Vec::new();
    stream
        .read_to_end(&mut raw)
        .map_err(|e| format!("read response: {e}"))?;
    parse_response(&raw)
}

fn parse_response(raw: &[u8]) -> R<HttpResponse> {
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or("malformed http response (no header terminator)")?;
    let head = String::from_utf8_lossy(&raw[..split]).to_string();
    let body_raw = &raw[split + 4..];
    let mut lines = head.lines();
    let status_line = lines.next().ok_or("empty http response")?;
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| format!("bad status line: {status_line}"))?;
    let chunked = lines.any(|l| {
        let l = l.to_lowercase();
        l.starts_with("transfer-encoding") && l.contains("chunked")
    });
    let body_bytes = if chunked {
        decode_chunked(body_raw)?
    } else {
        body_raw.to_vec()
    };
    Ok(HttpResponse {
        status,
        body: String::from_utf8_lossy(&body_bytes).to_string(),
    })
}

fn decode_chunked(mut raw: &[u8]) -> R<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let line_end = raw
            .windows(2)
            .position(|w| w == b"\r\n")
            .ok_or("chunked: missing size line")?;
        let size_str = String::from_utf8_lossy(&raw[..line_end]);
        let size = usize::from_str_radix(size_str.trim().split(';').next().unwrap_or("0"), 16)
            .map_err(|_| format!("chunked: bad size '{size_str}'"))?;
        raw = &raw[line_end + 2..];
        if size == 0 {
            break;
        }
        if raw.len() < size {
            return Err("chunked: truncated body".into());
        }
        out.extend_from_slice(&raw[..size]);
        raw = &raw[size..];
        if raw.starts_with(b"\r\n") {
            raw = &raw[2..];
        }
    }
    Ok(out)
}

pub fn post_json(url: &str, body: &str, timeout_s: u64) -> R<HttpResponse> {
    request("POST", url, Some(body), timeout_s)
}

pub fn get(url: &str, timeout_s: u64) -> R<HttpResponse> {
    request("GET", url, None, timeout_s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    /// One-shot fake server: accepts a single connection, returns `response`.
    fn serve_once(response: &'static str) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((mut sock, _)) = listener.accept() {
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf); // consume request
                let _ = sock.write_all(response.as_bytes());
            }
        });
        port
    }

    #[test]
    fn url_parsing() {
        assert_eq!(
            parse_url("http://127.0.0.1:11434/api/tags").unwrap(),
            ("127.0.0.1".into(), 11434, "/api/tags".into())
        );
        assert_eq!(
            parse_url("http://localhost").unwrap(),
            ("localhost".into(), 80, "/".into())
        );
        assert!(parse_url("https://secure.example").is_err());
        assert!(parse_url("ftp://x").is_err());
    }

    #[test]
    fn content_length_response() {
        let port = serve_once(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 15\r\n\r\n{\"response\":42}",
        );
        let resp = post_json(&format!("http://127.0.0.1:{port}/x"), "{}", 5).unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body, "{\"response\":42}");
    }

    #[test]
    fn chunked_response() {
        let port = serve_once(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n7\r\n{\"ok\":t\r\n4\r\nrue}\r\n0\r\n\r\n",
        );
        let resp = get(&format!("http://127.0.0.1:{port}/"), 5).unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body, "{\"ok\":true}");
    }

    #[test]
    fn connection_refused_is_fast_clean_error() {
        // port 1 is essentially never listening
        let started = std::time::Instant::now();
        let err = get("http://127.0.0.1:1/", 2).unwrap_err();
        assert!(err.contains("connect"));
        assert!(started.elapsed().as_secs() < 5);
    }
}

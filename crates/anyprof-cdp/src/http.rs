use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use crate::error::Error;

const MAX_BODY: usize = 8 * 1024 * 1024;

pub(crate) fn http_get(host: &str, port: u16, path: &str) -> Result<Vec<u8>, Error> {
    let addr = (host, port)
        .to_socket_addrs()
        .map_err(|err| Error::message(format!("resolving {host}:{port}: {err}")))?
        .next()
        .ok_or_else(|| Error::message(format!("no address for {host}:{port}")))?;
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2)).map_err(|err| {
        Error::message(format!("connecting to http://{host}:{port}{path}: {err}"))
    })?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\nAccept: application/json\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(request.as_bytes())?;
    let mut buf = Vec::new();
    let mut tmp = [0_u8; 8192];
    loop {
        if let Some(body) =
            complete_body(&buf).map_err(|err| Error::message(format!("{host}:{port}: {err}")))?
        {
            return Ok(body);
        }
        if buf.len() >= MAX_BODY {
            return Err(Error::message(format!(
                "response from {host}:{port} exceeds {MAX_BODY} bytes"
            )));
        }
        match stream.read(&mut tmp) {
            Ok(0) => {
                return split_http_body(&buf)
                    .map_err(|err| Error::message(format!("{host}:{port}: {err}")));
            }
            Ok(n) => {
                let room = MAX_BODY - buf.len();
                if n > room {
                    return Err(Error::message(format!(
                        "response from {host}:{port} exceeds {MAX_BODY} bytes"
                    )));
                }
                buf.extend_from_slice(&tmp[..n]);
            }
            Err(err)
                if err.kind() == std::io::ErrorKind::WouldBlock
                    || err.kind() == std::io::ErrorKind::TimedOut =>
            {
                if let Some(body) = complete_body(&buf)
                    .map_err(|err| Error::message(format!("{host}:{port}: {err}")))?
                {
                    return Ok(body);
                }
                return Err(Error::message(format!(
                    "timed out reading http://{host}:{port}{path}"
                )));
            }
            Err(err) => return Err(err.into()),
        }
    }
}

/// Node's inspector speaks HTTP/1.0, sends `Content-Length`, and leaves the
/// socket open. Once that many body bytes have arrived the response is complete.
fn complete_body(bytes: &[u8]) -> Result<Option<Vec<u8>>, String> {
    let Some(header_end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") else {
        return Ok(None);
    };
    let header = std::str::from_utf8(&bytes[..header_end])
        .map_err(|_| "HTTP headers are not utf-8".to_string())?;
    let Some(length) = content_length(header) else {
        return Ok(None);
    };
    let body_start = header_end + 4;
    if bytes.len() < body_start + length {
        return Ok(None);
    }
    split_http_body(&bytes[..body_start + length]).map(Some)
}

fn content_length(header: &str) -> Option<usize> {
    header.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        if name.eq_ignore_ascii_case("content-length") {
            value.trim().parse().ok()
        } else {
            None
        }
    })
}

fn split_http_body(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let header_end = bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| "HTTP response has no header terminator".to_string())?;
    let header = std::str::from_utf8(&bytes[..header_end])
        .map_err(|_| "HTTP headers are not utf-8".to_string())?;
    let status = header
        .lines()
        .next()
        .ok_or_else(|| "HTTP response is empty".to_string())?;
    let code = status
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| format!("bad status line `{status}`"))?;
    if code != "200" {
        return Err(format!("HTTP {code} from inspector"));
    }
    Ok(bytes[header_end + 4..].to_vec())
}

#[cfg(test)]
mod tests {
    use super::{complete_body, split_http_body};

    #[test]
    fn splits_a_json_body() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n[]";
        assert_eq!(split_http_body(raw).unwrap(), b"[]");
    }

    #[test]
    fn rejects_non_ok_status() {
        let raw = b"HTTP/1.1 404 Not Found\r\n\r\nnope";
        assert!(split_http_body(raw).is_err());
    }

    #[test]
    fn content_length_completes_without_eof() {
        let raw = b"HTTP/1.0 200 OK\r\nContent-Length: 2\r\n\r\n[]";
        assert_eq!(complete_body(raw).unwrap().unwrap(), b"[]");
        assert!(
            complete_body(b"HTTP/1.0 200 OK\r\nContent-Length: 4\r\n\r\n[]")
                .unwrap()
                .is_none()
        );
    }
}

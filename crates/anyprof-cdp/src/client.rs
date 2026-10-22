use std::io::ErrorKind;
use std::net::TcpStream;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tungstenite::client::connect_with_config;
use tungstenite::protocol::WebSocketConfig;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Error as WsError, Message};

use crate::error::Error;

/// Run the V8 CPU profiler for `duration` and return the `.cpuprofile` JSON object.
pub fn capture_cpu_profile(
    websocket_url: &str,
    duration: Duration,
    interval_us: u32,
) -> Result<Vec<u8>, Error> {
    if duration.is_zero() {
        return Err(Error::message("profile duration must be greater than zero"));
    }
    let config = WebSocketConfig::default()
        .max_message_size(Some(256 * 1024 * 1024))
        .max_frame_size(Some(256 * 1024 * 1024));
    let (mut socket, _response) = connect_with_config(websocket_url, Some(config), 0)
        .map_err(|err| Error::message(format!("websocket {websocket_url}: {err}")))?;
    prepare_socket(socket.get_mut())?;

    let mut rpc = Rpc { next_id: 1 };
    rpc.call(&mut socket, "Profiler.enable", json!({}))?;
    rpc.call(
        &mut socket,
        "Profiler.setSamplingInterval",
        json!({ "interval": interval_us }),
    )?;
    rpc.call(&mut socket, "Profiler.start", json!({}))?;
    tracing::info!(%websocket_url, ?duration, interval_us, "CPU profiler started");
    wait_for_samples(&mut socket, duration)?;
    let stopped = rpc.call(&mut socket, "Profiler.stop", json!({}))?;
    let _ = rpc.call(&mut socket, "Profiler.disable", json!({}));
    let profile = stopped
        .get("profile")
        .cloned()
        .ok_or_else(|| Error::message("Profiler.stop did not return a profile"))?;
    serde_json::to_vec(&profile)
        .map_err(|err| Error::message(format!("encoding cpuprofile JSON: {err}")))
}

fn prepare_socket(stream: &mut MaybeTlsStream<TcpStream>) -> Result<(), Error> {
    let Some(tcp) = plain_tcp(stream) else {
        return Ok(());
    };
    tcp.set_read_timeout(Some(Duration::from_secs(30)))?;
    tcp.set_write_timeout(Some(Duration::from_secs(10)))?;
    Ok(())
}

fn plain_tcp(stream: &mut MaybeTlsStream<TcpStream>) -> Option<&mut TcpStream> {
    match stream {
        MaybeTlsStream::Plain(tcp) => Some(tcp),
        _ => None,
    }
}

fn wait_for_samples(
    socket: &mut tungstenite::WebSocket<MaybeTlsStream<TcpStream>>,
    duration: Duration,
) -> Result<(), Error> {
    if let Some(tcp) = plain_tcp(socket.get_mut()) {
        tcp.set_nonblocking(true)?;
    }
    let started = Instant::now();
    while started.elapsed() < duration {
        // Bind the read so the message's drop order does not depend on the edition.
        let message = socket.read();
        match message {
            Ok(Message::Ping(payload)) => {
                socket
                    .send(Message::Pong(payload))
                    .map_err(|err| Error::message(format!("websocket pong: {err}")))?;
            }
            Ok(Message::Close(_)) => {
                return Err(Error::message(
                    "inspector closed the websocket while sampling",
                ));
            }
            Ok(_) => {}
            Err(WsError::Io(err))
                if matches!(
                    err.kind(),
                    ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
                ) =>
            {
                thread::sleep(Duration::from_millis(50));
            }
            Err(err) => return Err(Error::message(format!("websocket read: {err}"))),
        }
    }
    if let Some(tcp) = plain_tcp(socket.get_mut()) {
        tcp.set_nonblocking(false)?;
        tcp.set_read_timeout(Some(Duration::from_secs(30)))?;
    }
    Ok(())
}

struct Rpc {
    next_id: u64,
}

impl Rpc {
    fn call(
        &mut self,
        socket: &mut tungstenite::WebSocket<MaybeTlsStream<TcpStream>>,
        method: &str,
        params: Value,
    ) -> Result<Value, Error> {
        let id = self.next_id;
        self.next_id += 1;
        let payload = json!({ "id": id, "method": method, "params": params });
        socket
            .send(Message::text(payload.to_string()))
            .map_err(|err| Error::message(format!("{method}: send failed: {err}")))?;
        for _ in 0..10_000 {
            let message = socket
                .read()
                .map_err(|err| Error::message(format!("{method}: read failed: {err}")))?;
            match message {
                Message::Text(text) => {
                    let value: Value = serde_json::from_str(text.as_str()).map_err(|err| {
                        Error::message(format!("{method}: response is not JSON: {err}"))
                    })?;
                    if value.get("id").and_then(Value::as_u64) != Some(id) {
                        continue;
                    }
                    if let Some(error) = value.get("error") {
                        let message = error
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("inspector error");
                        return Err(Error::message(format!("{method}: {message}")));
                    }
                    return Ok(value.get("result").cloned().unwrap_or_else(|| json!({})));
                }
                Message::Ping(payload) => {
                    socket
                        .send(Message::Pong(payload))
                        .map_err(|err| Error::message(format!("websocket pong: {err}")))?;
                }
                Message::Close(_) => {
                    return Err(Error::message(format!(
                        "{method}: inspector closed the websocket"
                    )));
                }
                Message::Binary(_) | Message::Pong(_) | Message::Frame(_) => {}
            }
        }
        Err(Error::message(format!(
            "{method}: gave up waiting for a response"
        )))
    }
}

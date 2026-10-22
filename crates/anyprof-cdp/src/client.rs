use std::io::ErrorKind;
use std::net::TcpStream;
use std::thread;
use std::time::{Duration, Instant};

use anyprof_core::StopGate;
use serde_json::{Value, json};
use tungstenite::client::connect_with_config;
use tungstenite::protocol::WebSocketConfig;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Error as WsError, Message};

use crate::error::Error;

/// Largest `Profiler.stop` reply accepted. At the 100us default a busy target
/// produces about 2.5 MiB of JSON per minute, so this covers well over an hour.
const MAX_MESSAGE_BYTES: usize = 256 * 1024 * 1024;

/// Run the V8 CPU profiler and return the `.cpuprofile` JSON object.
///
/// `expected_pid`, when set, is checked with `Runtime.evaluate` of `process.pid`
/// before sampling starts. A mismatch is an error: the websocket often belongs
/// to a different Node process that is already listening on port 9229.
///
/// `interval_us` is V8's sampling interval in microseconds (`Profiler.setSamplingInterval`
/// is called before `Profiler.start`). Sampling ends when `duration` elapses, or
/// sooner when `stop` is signaled. `None` waits until `stop`. `Profiler.stop`
/// and `Profiler.disable` run on every path after `Profiler.start`, including
/// errors, so the target is not left with the profiler attached.
pub fn capture_cpu_profile(
    websocket_url: &str,
    duration: Option<Duration>,
    interval_us: u32,
    expected_pid: Option<u32>,
    stop: &StopGate,
) -> Result<Vec<u8>, Error> {
    if duration.is_some_and(|duration| duration.is_zero()) {
        return Err(Error::message("profile duration must be greater than zero"));
    }
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE_BYTES))
        .max_frame_size(Some(MAX_MESSAGE_BYTES));
    let (mut socket, _response) = connect_with_config(websocket_url, Some(config), 0)
        .map_err(|err| Error::message(format!("websocket {websocket_url}: {err}")))?;
    prepare_socket(socket.get_mut())?;

    let mut rpc = Rpc { next_id: 1 };
    if let Some(expected) = expected_pid {
        let actual = node_process_pid(&mut rpc, &mut socket)?;
        if actual != expected {
            return Err(Error::message(format!(
                "inspector {websocket_url} is node pid {actual}, not the requested pid {expected}. Refusing to profile a different process (a stale inspector on 127.0.0.1:9229 is the usual cause)"
            )));
        }
        tracing::info!(pid = actual, %websocket_url, "inspector process.pid matches");
    }
    // V8 ignores a sampling interval set after Profiler.start. The unit is microseconds.
    rpc.call(
        &mut socket,
        "Profiler.setSamplingInterval",
        json!({ "interval": interval_us }),
    )?;
    rpc.call(&mut socket, "Profiler.enable", json!({}))?;
    rpc.call(&mut socket, "Profiler.start", json!({}))?;
    tracing::info!(%websocket_url, ?duration, interval_us, "CPU profiler started");
    let waited = wait_for_samples(&mut socket, duration, stop);
    // Stop even when the wait failed, and before returning, so Ctrl+C and
    // websocket errors both detach the profiler.
    stop.begin_stopping();
    let stopped = rpc.call(&mut socket, "Profiler.stop", json!({}));
    let _ = rpc.call(&mut socket, "Profiler.disable", json!({}));
    waited?;
    let stopped = stopped?;
    let profile = stopped
        .get("profile")
        .cloned()
        .ok_or_else(|| Error::message("Profiler.stop did not return a profile"))?;
    serde_json::to_vec(&profile)
        .map_err(|err| Error::message(format!("encoding cpuprofile JSON: {err}")))
}

fn node_process_pid(
    rpc: &mut Rpc,
    socket: &mut tungstenite::WebSocket<MaybeTlsStream<TcpStream>>,
) -> Result<u32, Error> {
    let result = rpc.call(
        socket,
        "Runtime.evaluate",
        json!({
            "expression": "process.pid",
            "returnByValue": true,
        }),
    )?;
    if result.get("exceptionDetails").is_some() {
        let text = result
            .pointer("/result/description")
            .and_then(Value::as_str)
            .unwrap_or("evaluation failed");
        return Err(Error::message(format!(
            "could not read process.pid from the inspector: {text}"
        )));
    }
    let value = result.pointer("/result/value").ok_or_else(|| {
        Error::message("inspector did not return process.pid (the target may not be Node.js)")
    })?;
    let pid = value.as_u64().or_else(|| {
        value.as_f64().and_then(|number| {
            if number >= 0.0 && number <= f64::from(u32::MAX) && number.fract() == 0.0 {
                Some(number as u64)
            } else {
                None
            }
        })
    });
    let Some(pid) = pid else {
        return Err(Error::message(format!(
            "process.pid was not an integer ({value})"
        )));
    };
    u32::try_from(pid)
        .map_err(|_| Error::message(format!("process.pid {pid} does not fit in a pid")))
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
    duration: Option<Duration>,
    stop: &StopGate,
) -> Result<(), Error> {
    if let Some(tcp) = plain_tcp(socket.get_mut()) {
        tcp.set_nonblocking(true)?;
    }
    let started = Instant::now();
    let result = (|| {
        loop {
            if !keep_sampling(started.elapsed(), duration, stop.stop_requested()) {
                return Ok(());
            }
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
    })();
    if let Some(tcp) = plain_tcp(socket.get_mut()) {
        let _ = tcp.set_nonblocking(false);
        let _ = tcp.set_read_timeout(Some(Duration::from_secs(30)));
    }
    result
}

/// Whether the sampling loop should take another sample tick.
pub(crate) fn keep_sampling(
    elapsed: Duration,
    limit: Option<Duration>,
    stop_requested: bool,
) -> bool {
    if stop_requested {
        return false;
    }
    match limit {
        Some(limit) => elapsed < limit,
        None => true,
    }
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
            let message = socket.read().map_err(|err| match err {
                WsError::Capacity(_) => Error::message(format!(
                    "{method}: {err}. The profile is larger than {} MiB; record a shorter window or sample less often, for example --interval 1ms",
                    MAX_MESSAGE_BYTES / (1024 * 1024)
                )),
                err => Error::message(format!("{method}: read failed: {err}")),
            })?;
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

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

/// JavaScript expression that closes the Node.js inspector across CommonJS and
/// ES module processes without throwing into user code.
const CLOSE_INSPECTOR_EXPR: &str = r#"(() => {
  try {
    const mod = typeof process === "object" && process !== null
      ? (typeof process.getBuiltinModule === "function"
        ? process.getBuiltinModule("node:inspector")
        : (typeof require === "function"
          ? require("node:inspector")
          : (process.mainModule && typeof process.mainModule.require === "function"
            ? process.mainModule.require("node:inspector")
            : null)))
      : null;
    if (mod && typeof mod.close === "function") {
      mod.close();
      return true;
    }
    import("node:inspector").then((m) => m.close()).catch(() => {});
    return false;
  } catch (_) {
    return false;
  }
})()"#;

/// Active background V8 CPU profiling session over the Chrome DevTools Protocol.
///
/// Created by [`start_cpu_profile`] after `Profiler.start` succeeds. Dropping
/// the session without calling [`Self::stop`] signals the background worker and
/// waits for `Profiler.stop` / `Profiler.disable` (and optional inspector close)
/// so the target process is never left with an active profiler attached.
pub struct CdpSession {
    session_stop: std::sync::Arc<StopGate>,
    worker: Option<thread::JoinHandle<Result<Vec<u8>, Error>>>,
}

impl CdpSession {
    /// End sampling, detach the V8 profiler, and return the raw `.cpuprofile` JSON.
    pub fn stop(mut self) -> Result<Vec<u8>, Error> {
        self.session_stop.request_stop();
        let handle = self
            .worker
            .take()
            .expect("worker handle present until stop or drop");
        handle
            .join()
            .map_err(|_| Error::message("CDP profiler background thread panicked"))?
    }
}

impl Drop for CdpSession {
    fn drop(&mut self) {
        if let Some(handle) = self.worker.take() {
            self.session_stop.request_stop();
            let _ = handle.join();
        }
    }
}

/// Connect to `websocket_url`, verify `expected_pid` when set, start V8's CPU
/// profiler synchronously, and spawn the background reader/timer session.
pub fn start_cpu_profile(
    websocket_url: &str,
    duration: Option<Duration>,
    interval_us: u32,
    expected_pid: Option<u32>,
    close_inspector: bool,
    stop: std::sync::Arc<StopGate>,
) -> Result<CdpSession, Error> {
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
    let started_result = (|| -> Result<(), Error> {
        // V8 ignores a sampling interval set after Profiler.start. The unit is microseconds.
        rpc.call(
            &mut socket,
            "Profiler.setSamplingInterval",
            json!({ "interval": interval_us }),
        )?;
        rpc.call(&mut socket, "Profiler.enable", json!({}))?;
        rpc.call(&mut socket, "Profiler.start", json!({}))?;
        Ok(())
    })();
    if let Err(err) = started_result {
        if close_inspector {
            close_node_inspector(&mut rpc, &mut socket, websocket_url);
        }
        return Err(err);
    }
    tracing::info!(%websocket_url, ?duration, interval_us, "CPU profiler started");

    let session_stop = StopGate::new();
    let worker_session_stop = std::sync::Arc::clone(&session_stop);
    let url_owned = websocket_url.to_string();
    let worker = thread::Builder::new()
        .name("anyprof-cdp".to_string())
        .spawn(move || -> Result<Vec<u8>, Error> {
            let waited = wait_for_samples(&mut socket, duration, &stop, &worker_session_stop);
            // Stop even when the wait failed, and before returning, so Ctrl+C and
            // websocket errors both detach the profiler.
            stop.begin_stopping();
            let stopped = rpc.call(&mut socket, "Profiler.stop", json!({}));
            let _ = rpc.call(&mut socket, "Profiler.disable", json!({}));
            if close_inspector {
                close_node_inspector(&mut rpc, &mut socket, &url_owned);
            }
            waited?;
            let stopped = stopped?;
            let profile = stopped
                .get("profile")
                .cloned()
                .ok_or_else(|| Error::message("Profiler.stop did not return a profile"))?;
            serde_json::to_vec(&profile)
                .map_err(|err| Error::message(format!("encoding cpuprofile JSON: {err}")))
        })
        .map_err(|err| Error::message(format!("spawning CDP profiler thread: {err}")))?;

    Ok(CdpSession {
        session_stop,
        worker: Some(worker),
    })
}

/// Run the V8 CPU profiler synchronously for `duration` (or until `stop`) and
/// return the `.cpuprofile` JSON object.
pub fn capture_cpu_profile(
    websocket_url: &str,
    duration: Option<Duration>,
    interval_us: u32,
    expected_pid: Option<u32>,
    close_inspector: bool,
    stop: &StopGate,
) -> Result<Vec<u8>, Error> {
    let proxy_stop = StopGate::new();
    let session = start_cpu_profile(
        websocket_url,
        duration,
        interval_us,
        expected_pid,
        close_inspector,
        std::sync::Arc::clone(&proxy_stop),
    )?;
    let started = Instant::now();
    while keep_sampling(started.elapsed(), duration, stop.stop_requested()) {
        thread::sleep(Duration::from_millis(20));
    }
    if stop.stop_requested() {
        proxy_stop.request_stop();
    }
    let res = session.stop();
    stop.begin_stopping();
    res
}

fn close_node_inspector(
    rpc: &mut Rpc,
    socket: &mut tungstenite::WebSocket<MaybeTlsStream<TcpStream>>,
    websocket_url: &str,
) {
    if let Some(tcp) = plain_tcp(socket.get_mut()) {
        let _ = tcp.set_read_timeout(Some(Duration::from_millis(2000)));
        let _ = tcp.set_write_timeout(Some(Duration::from_millis(2000)));
    }
    // `inspector.close()` stops the listening port and closes active inspector
    // sessions immediately, typically dropping the websocket before sending a
    // JSON-RPC reply.
    let _ = rpc.call(
        socket,
        "Runtime.evaluate",
        json!({
            "expression": CLOSE_INSPECTOR_EXPR,
            "returnByValue": true,
        }),
    );
    let _ = socket.close(None);
    wait_for_inspector_port_closed(websocket_url);
}

fn wait_for_inspector_port_closed(websocket_url: &str) {
    use std::net::ToSocketAddrs;

    let Some(rest) = websocket_url
        .strip_prefix("ws://")
        .or_else(|| websocket_url.strip_prefix("wss://"))
    else {
        return;
    };
    let authority = rest.split('/').next().unwrap_or("");
    let Ok(mut addrs) = authority.to_socket_addrs() else {
        return;
    };
    let Some(addr) = addrs.next() else {
        return;
    };
    let deadline = Instant::now() + Duration::from_secs(1);
    while Instant::now() < deadline {
        if TcpStream::connect_timeout(&addr, Duration::from_millis(25)).is_err() {
            break;
        }
        thread::sleep(Duration::from_millis(15));
    }
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
    session_stop: &StopGate,
) -> Result<(), Error> {
    if let Some(tcp) = plain_tcp(socket.get_mut()) {
        tcp.set_nonblocking(true)?;
    }
    let started = Instant::now();
    let result = (|| {
        loop {
            let stop_requested = stop.stop_requested() || session_stop.stop_requested();
            if !keep_sampling(started.elapsed(), duration, stop_requested) {
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
                    thread::sleep(Duration::from_millis(20));
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

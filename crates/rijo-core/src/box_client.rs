//! `box.v1` tool-socket client (ADR 0011).
//!
//! Rijo Code talks to a running Rijo Box through the local tool socket served by
//! `rijo-box serve`. One flat JSON object per line; a call returns the tool result JSON or a
//! `box.v1` error object. See the box repo's `docs/p0-box-tools.md`.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::time::Duration;

/// Where the box is and how long to wait. `addr` is `host:port` (P0: TCP loopback).
#[derive(Debug, Clone)]
pub struct BoxClient {
    addr: String,
    timeout: Duration,
}

impl BoxClient {
    pub fn new(addr: impl Into<String>) -> Self {
        Self {
            addr: addr.into(),
            timeout: Duration::from_secs(30),
        }
    }

    /// The address from `RIJO_BOX_ADDR` — this is what **enables** the `box` toolset.
    pub fn from_env() -> Option<Self> {
        std::env::var("RIJO_BOX_ADDR")
            .ok()
            .filter(|s| !s.is_empty())
            .map(Self::new)
    }

    /// `RIJO_BOX_ADDR` first, then `[box] addr` in the config.
    pub fn from_config(cfg: &crate::config::Config) -> Option<Self> {
        Self::from_env().or_else(|| {
            cfg.box_config
                .addr
                .clone()
                .filter(|s| !s.is_empty())
                .map(Self::new)
        })
    }

    pub fn addr(&self) -> &str {
        &self.addr
    }

    /// Send one request line and read one response line. Errors are strings, never panics.
    pub fn call(&self, request_line: &str) -> Result<String, String> {
        let mut stream = TcpStream::connect(self.addr.as_str())
            .map_err(|e| format!("cannot reach the box at {}: {e}", self.addr))?;
        stream.set_read_timeout(Some(self.timeout)).ok();
        stream.set_write_timeout(Some(self.timeout)).ok();
        let reader_stream = stream.try_clone().map_err(|e| e.to_string())?;
        writeln!(stream, "{request_line}").map_err(|e| format!("box write: {e}"))?;
        stream.flush().ok();
        let mut reader = BufReader::new(reader_stream);
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .map_err(|e| format!("box read: {e}"))?;
        if line.trim().is_empty() {
            return Err("the box closed the connection without a response".to_string());
        }
        Ok(line.trim_end().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn calls_over_the_socket_and_reads_one_response() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let server = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(s.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            assert!(line.contains("box_status"), "server saw: {line}");
            writeln!(s, "{{\"ok\": true}}").unwrap();
        });

        let client = BoxClient::new(addr);
        let out = client.call("{\"tool\":\"box_status\"}").unwrap();
        assert_eq!(out, "{\"ok\": true}");
        server.join().unwrap();
    }

    #[test]
    fn a_dead_endpoint_is_an_error_not_a_panic() {
        // Port 1 is essentially never listening; a refused connection must be a Result, not a panic.
        let client = BoxClient::new("127.0.0.1:1");
        assert!(client.call("{\"tool\":\"box_status\"}").is_err());
    }

    /// Opt-in integration: run against a live `rijo-box serve` (the box repo) with
    /// `RIJO_BOX_ADDR` set and `--ignored`. Skipped otherwise.
    #[test]
    #[ignore]
    fn end_to_end_against_a_live_box() {
        let Ok(addr) = std::env::var("RIJO_BOX_ADDR") else {
            return;
        };
        let client = BoxClient::new(addr);
        let out = client.call("{\"tool\":\"box_status\"}").unwrap();
        assert!(out.contains("\"box_id\""), "{out}");
        // Through the same socket, box_exec boots the guest and runs the in-guest agent.
        let exec = client
            .call("{\"tool\":\"box_exec\",\"cmd\":\"uname\"}")
            .unwrap();
        assert!(exec.contains("exit_code"), "{exec}");
    }
}

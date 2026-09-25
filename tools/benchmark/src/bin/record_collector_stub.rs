//! A stand-in for the record collector, for benchmarking the quote path with the emitter on.
//!
//! Two modes, which are the two ways a collector costs the pod something:
//!
//! - `accept` answers `202 Accepted` to everything, the healthy case.
//! - `blackhole` completes the TCP handshake and then never answers, which is what finds a
//!   blocking send: a pod that waits on the collector stalls here, one that does not carries on
//!   and counts `quote_records_dropped_total{reason="collector_timeout"}`.
//!
//! Neither mode parses the body beyond the length it has to read to keep the connection in step.
//!
//! ```text
//! record-collector-stub --mode accept --port 8081
//! ```

use std::net::SocketAddr;

use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

#[derive(Debug, Clone, Copy, ValueEnum)]
enum Mode {
    /// Answer `202 Accepted` to every request.
    Accept,
    /// Accept the connection, read what arrives, and never answer.
    Blackhole,
}

#[derive(Parser, Debug)]
#[command(about = "A stub record collector for emitter benchmarks")]
struct Args {
    /// How the stub answers.
    #[arg(long, value_enum, default_value = "accept")]
    mode: Mode,

    /// Port to listen on.
    #[arg(long, default_value_t = 8081)]
    port: u16,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let addr = SocketAddr::from(([127, 0, 0, 1], args.port));
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind {addr}"))?;
    println!("stub collector listening on http://{addr} in {:?} mode", args.mode);

    loop {
        let (stream, _) = listener.accept().await?;
        tokio::spawn(async move {
            if let Err(error) = serve(stream, args.mode).await {
                eprintln!("connection ended: {error}");
            }
        });
    }
}

/// Serves one connection until the peer closes it.
async fn serve(mut stream: TcpStream, mode: Mode) -> Result<()> {
    loop {
        let Some(request) = read_request(&mut stream).await? else {
            return Ok(());
        };
        match mode {
            // Holding the connection open with nothing written is the whole point: the emitter's
            // own timeout is what has to end the wait.
            Mode::Blackhole => {
                drop(request);
                std::future::pending::<()>().await;
            }
            Mode::Accept => {
                stream
                    .write_all(b"HTTP/1.1 202 Accepted\r\ncontent-length: 0\r\n\r\n")
                    .await?;
            }
        }
    }
}

/// Reads one whole request, headers and body. `None` when the peer closed the connection.
///
/// The body has to be drained even when nothing looks at it, or the next request on a reused
/// connection starts mid-body.
async fn read_request(stream: &mut TcpStream) -> Result<Option<Vec<u8>>> {
    let mut request = Vec::new();
    let mut byte = [0u8; 1];
    let headers_end = loop {
        let read = stream.read(&mut byte).await?;
        if read == 0 {
            return Ok(None);
        }
        request.push(byte[0]);
        if request.ends_with(b"\r\n\r\n") {
            break request.len();
        }
    };

    let body_len = content_length(&request[..headers_end]);
    request.resize(headers_end + body_len, 0);
    stream
        .read_exact(&mut request[headers_end..])
        .await?;
    Ok(Some(request))
}

/// The `Content-Length` of a request, or zero when it carries none.
fn content_length(headers: &[u8]) -> usize {
    String::from_utf8_lossy(headers)
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().ok())?
        })
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_content_length_reads_the_header() {
        let headers = b"POST /v1/records HTTP/1.1\r\nContent-Length: 42\r\n\r\n";
        assert_eq!(content_length(headers), 42);
    }

    #[test]
    fn test_content_length_ignores_case_and_spacing() {
        let headers = b"POST / HTTP/1.1\r\ncontent-length:7\r\n\r\n";
        assert_eq!(content_length(headers), 7);
    }

    #[test]
    fn test_content_length_defaults_to_zero() {
        assert_eq!(content_length(b"GET / HTTP/1.1\r\nhost: x\r\n\r\n"), 0);
    }
}

//! Cancellation must release silent provider connections and their readers.

#![cfg(target_os = "linux")]
#![allow(clippy::expect_used, clippy::panic)]

use std::io::{BufRead as _, Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use rune_net::chat_completions::ChatCompletions;
use rune_net::error::FailureKind;
use rune_net::provider::RequestPlan;
use rune_net::transport::{Endpoint, agent, stream_completion};

fn socket_count() -> usize {
    std::fs::read_dir("/proc/self/fd")
        .expect("file descriptors")
        .filter_map(|entry| std::fs::read_link(entry.ok()?.path()).ok())
        .filter(|target| target.to_string_lossy().starts_with("socket:"))
        .count()
}

fn reader_count() -> usize {
    std::fs::read_dir("/proc/self/task")
        .expect("threads")
        .filter_map(|entry| std::fs::read_to_string(entry.ok()?.path().join("comm")).ok())
        // Linux truncates thread names to 15 bytes.
        .filter(|name| name.trim() == "rune-stream-rea")
        .count()
}

fn accept_request(listener: &TcpListener) -> TcpStream {
    let started = Instant::now();
    let mut stream = loop {
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(
                    started.elapsed() < Duration::from_secs(10),
                    "fixture received no request"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(error) => panic!("accept: {error}"),
        }
    };
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("read timeout");
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .expect("write timeout");
    let mut reader = std::io::BufReader::new(&mut stream);
    let mut length = 0;
    loop {
        let mut line = String::new();
        assert!(reader.read_line(&mut line).expect("request headers") > 0);
        if line == "\r\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse().expect("content length");
        }
    }
    reader
        .read_exact(&mut vec![0; length])
        .expect("request body");
    drop(reader);
    stream
        .write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
        )
        .expect("response headers");
    stream
}

#[test]
fn cancelling_fifty_silent_streams_returns_readers_and_sockets_to_baseline() {
    const STREAMS: usize = 50;
    let baseline_readers = reader_count();
    let baseline_sockets = socket_count();
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let address = listener.local_addr().expect("address");
    let (ready, received) = std::sync::mpsc::channel();
    let server = std::thread::spawn(move || {
        let mut streams: Vec<_> = (0..STREAMS).map(|_| accept_request(&listener)).collect();
        ready.send(()).expect("ready");
        // Keep every body silent and open until its client closes it.
        for stream in &mut streams {
            assert_eq!(stream.read(&mut [0]).expect("client must close socket"), 0);
        }
    });
    let client = agent();
    let cancelled = Arc::new(AtomicBool::new(false));
    let workers: Vec<_> = (0..STREAMS)
        .map(|_| {
            let client = client.clone();
            let cancelled = cancelled.clone();
            std::thread::spawn(move || {
                stream_completion(
                    &client,
                    &Endpoint::new(format!("http://{address}"), "fixture"),
                    &ChatCompletions,
                    &RequestPlan::new("fixture"),
                    Duration::from_secs(30),
                    &|| cancelled.load(Ordering::Acquire),
                )
            })
        })
        .collect();
    received
        .recv_timeout(Duration::from_secs(10))
        .expect("all streams received headers");
    let started = Instant::now();
    while reader_count() != baseline_readers + STREAMS {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "all readers must start"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(socket_count(), baseline_sockets + 1 + 2 * STREAMS);
    cancelled.store(true, Ordering::Release);
    for worker in workers {
        let error = worker
            .join()
            .expect("request thread")
            .expect_err("cancelled");
        assert_eq!(error.kind(), FailureKind::Cancelled, "{error}");
    }
    server.join().expect("server observed every client close");
    let started = Instant::now();
    while reader_count() != baseline_readers || socket_count() != baseline_sockets {
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "resources did not return to baseline: readers={}, sockets={}",
            reader_count(),
            socket_count()
        );
        std::thread::sleep(Duration::from_millis(5));
    }

    // The same agent can still receive an answer after a quiet interval.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let address = listener.local_addr().expect("address");
    let server = std::thread::spawn(move || {
        let mut stream = accept_request(&listener);
        std::thread::sleep(Duration::from_millis(200));
        stream
            .write_all(b"data: {\"choices\":[{\"delta\":{\"content\":\"after silence\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n")
            .expect("answer");
    });
    let outcome = stream_completion(
        &client,
        &Endpoint::new(format!("http://{address}"), "fixture"),
        &ChatCompletions,
        &RequestPlan::new("fixture"),
        Duration::from_secs(5),
        &|| false,
    )
    .expect("uncancelled stream survives silence");
    assert_eq!(outcome.text(), "after silence");
    server.join().expect("answer server");
}

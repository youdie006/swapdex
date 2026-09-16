use super::*;
use std::io::{Cursor, Write};
use std::net::{Shutdown, TcpStream};
use std::time::Duration;

fn connect(server: &Server) -> TcpStream {
    let client = TcpStream::connect(server.server_addr()).unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    client
        .set_write_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    client
}

fn receive_request(server: &Server) -> Request {
    server
        .requests
        .lock()
        .unwrap()
        .recv_timeout(Duration::from_secs(3))
        .unwrap()
        .unwrap()
}

fn read_headers(client: &mut TcpStream) -> Vec<u8> {
    let mut headers = Vec::new();
    while !headers.ends_with(b"\r\n\r\n") {
        let mut byte = [0];
        client.read_exact(&mut byte).unwrap();
        headers.push(byte[0]);
    }
    headers
}

fn stream_response(bytes: &[u8]) -> Response<Cursor<Vec<u8>>> {
    Response::new(
        tiny_http::StatusCode(200),
        vec![Header::from_bytes("content-type", "text/event-stream").unwrap()],
        Cursor::new(bytes.to_vec()),
        None,
        None,
    )
}

#[test]
fn http_10_streams_are_delimited_by_connection_close() {
    let server = Server::http(("127.0.0.1", 0)).unwrap();
    let mut client = connect(&server);
    let reader = std::thread::spawn(move || {
        client
            .write_all(b"GET /stream HTTP/1.0\r\nHost: localhost\r\n\r\n")
            .unwrap();
        let mut wire = String::new();
        client.read_to_string(&mut wire).unwrap();
        wire
    });
    receive_request(&server)
        .respond(stream_response(b"data: hello\n\n"))
        .unwrap();
    let wire = reader.join().unwrap();
    assert!(wire.starts_with("HTTP/1.0 200 OK\r\n"));
    let (headers, body) = wire.split_once("\r\n\r\n").unwrap();
    assert!(!headers.to_ascii_lowercase().contains("transfer-encoding"));
    assert_eq!(body, "data: hello\n\n");
}

#[test]
fn expect_continue_allows_the_client_to_upload_its_body() {
    let server = Server::http(("127.0.0.1", 0)).unwrap();
    let mut client = connect(&server);
    let reader = std::thread::spawn(move || {
        client.write_all(b"POST /upload HTTP/1.1\r\nHost: localhost\r\nExpect: 100-continue\r\nContent-Length: 4\r\nConnection: close\r\n\r\n").unwrap();
        assert!(String::from_utf8(read_headers(&mut client))
            .unwrap()
            .starts_with("HTTP/1.1 100 Continue\r\n"));
        client.write_all(b"body").unwrap();
        let mut wire = String::new();
        client.read_to_string(&mut wire).unwrap();
        wire
    });
    let mut request = receive_request(&server);
    assert_eq!(request.take_body(), b"body");
    request.respond(Response::from_string("uploaded")).unwrap();
    let wire = reader.join().unwrap();
    assert!(wire.starts_with("HTTP/1.1 200 OK\r\n"));
    assert!(wire.ends_with("\r\n\r\nuploaded"));
}

#[test]
fn a_malformed_request_does_not_stop_the_next_connection() {
    let server = Server::http(("127.0.0.1", 0)).unwrap();
    let mut malformed = connect(&server);
    malformed.write_all(b"POST / HTTP/1.1\r\nHost: localhost\r\nContent-Length: invalid\r\nConnection: close\r\n\r\n").unwrap();
    let mut rejected = String::new();
    malformed.read_to_string(&mut rejected).unwrap();
    assert!(rejected.starts_with("HTTP/1.1 400 "));
    let mut client = connect(&server);
    let reader = std::thread::spawn(move || {
        client
            .write_all(b"GET /healthy HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .unwrap();
        let mut wire = String::new();
        client.read_to_string(&mut wire).unwrap();
        wire
    });
    receive_request(&server)
        .respond(Response::from_string("healthy"))
        .unwrap();
    assert!(reader.join().unwrap().ends_with("\r\n\r\nhealthy"));
}

#[test]
fn pipelined_requests_receive_complete_responses_in_order() {
    let server = Server::http(("127.0.0.1", 0)).unwrap();
    let mut client = connect(&server);
    let reader = std::thread::spawn(move || {
        client.write_all(b"POST /first HTTP/1.1\r\nHost: localhost\r\nContent-Length: 2\r\n\r\n{}GET /second HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").unwrap();
        let mut wire = String::new();
        client.read_to_string(&mut wire).unwrap();
        wire
    });
    let mut first = receive_request(&server);
    assert_eq!(first.url(), "/first");
    assert_eq!(first.take_body(), b"{}");
    first.respond(stream_response(b"data: first\n\n")).unwrap();
    let second = receive_request(&server);
    assert_eq!(second.url(), "/second");
    second.respond(Response::from_string("second")).unwrap();
    let wire = reader.join().unwrap();
    assert_eq!(wire.matches("HTTP/1.1 200 OK\r\n").count(), 2);
    assert!(wire.contains("data: first\n\n\r\n0\r\n\r\nHTTP/1.1 200 OK\r\n"));
    assert!(wire.ends_with("\r\n\r\nsecond"));
}

#[test]
fn dropping_the_listener_closes_an_active_connection() {
    let server = Server::http(("127.0.0.1", 0)).unwrap();
    let mut client = connect(&server);
    let reader = std::thread::spawn(move || {
        client
            .write_all(b"GET /held HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .unwrap();
        let mut wire = Vec::new();
        client.read_to_end(&mut wire)
    });
    let request = receive_request(&server);
    drop(server);
    drop(request);
    assert!(reader.join().unwrap().is_ok());
}

#[test]
fn a_disconnected_idle_stream_does_not_block_other_clients() {
    struct HeldBody(mpsc::Receiver<()>);
    impl Read for HeldBody {
        fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
            self.0.recv_timeout(Duration::from_secs(3)).unwrap();
            Ok(0)
        }
    }

    let server = Server::http(("127.0.0.1", 0)).unwrap();
    let mut idle = connect(&server);
    idle.write_all(b"GET /idle HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .unwrap();
    let request = receive_request(&server);
    let (release, held) = mpsc::channel();
    let producer = std::thread::spawn(move || {
        request.respond(Response::new(
            tiny_http::StatusCode(200),
            Vec::new(),
            HeldBody(held),
            None,
            None,
        ))
    });
    assert!(String::from_utf8(read_headers(&mut idle))
        .unwrap()
        .starts_with("HTTP/1.1 200 OK\r\n"));
    idle.shutdown(Shutdown::Both).unwrap();
    drop(idle);
    let mut healthy = connect(&server);
    let reader = std::thread::spawn(move || {
        healthy
            .write_all(b"GET /healthy HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .unwrap();
        let mut wire = String::new();
        healthy.read_to_string(&mut wire).unwrap();
        wire
    });
    receive_request(&server)
        .respond(Response::from_string("healthy"))
        .unwrap();
    let wire = reader.join().unwrap();
    // The synchronous upstream read cannot be cancelled while it is idle.
    // Release it explicitly and reap it before checking the other response.
    release.send(()).unwrap();
    producer.join().unwrap().ok();
    assert!(wire.ends_with("\r\n\r\nhealthy"));
}

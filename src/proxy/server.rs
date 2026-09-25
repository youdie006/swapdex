//! HTTP framing with a bounded bridge to the synchronous account handlers.
//!
//! Each available fragment can reach the socket immediately. A failed body
//! closes its connection through Hyper instead of emitting a successful EOF or
//! releasing a partially written response for another request to reuse.

use http_body_util::BodyExt;
use hyper::body::{Body as HttpBody, Bytes, Frame, Incoming, SizeHint};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use std::io::{self, Read};
use std::net::{SocketAddr, TcpListener, ToSocketAddrs};
use std::pin::Pin;
use std::sync::{mpsc, Mutex};
use std::task::{Context, Poll};
use std::thread::JoinHandle;
use tiny_http::{Header, Method, Response};
use tokio::sync::{mpsc as body_channel, oneshot};

#[cfg(test)]
mod tests;

pub(super) struct Server {
    addr: SocketAddr,
    requests: Mutex<mpsc::Receiver<io::Result<Request>>>,
    stop: Option<oneshot::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Server {
    pub fn http(addr: impl ToSocketAddrs) -> io::Result<Self> {
        Self::start(TcpListener::bind(addr)?, None)
    }

    /// The same listener speaking TLS. Requests from it report `is_tls()`.
    pub fn tls(
        addr: impl ToSocketAddrs,
        config: std::sync::Arc<tokio_rustls::rustls::ServerConfig>,
    ) -> io::Result<Self> {
        Self::start(
            TcpListener::bind(addr)?,
            Some(tokio_rustls::TlsAcceptor::from(config)),
        )
    }

    fn start(listener: TcpListener, tls: Option<tokio_rustls::TlsAcceptor>) -> io::Result<Self> {
        let addr = listener.local_addr()?;
        listener.set_nonblocking(true)?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let (requests, receiver) = mpsc::channel();
        let (stop, stopped) = oneshot::channel();
        let thread = std::thread::Builder::new()
            .name("swapdex-http".into())
            .spawn(move || {
                runtime.block_on(async move {
                    tokio::select! {
                        _ = stopped => {}
                        _ = accept_loop(listener, tls, requests) => {}
                    }
                });
            })?;
        Ok(Self {
            addr,
            requests: Mutex::new(receiver),
            stop: Some(stop),
            thread: Some(thread),
        })
    }

    pub fn server_addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn recv(&self) -> io::Result<Request> {
        self.requests
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .recv()
            .map_err(|_| io::Error::other("HTTP listener stopped"))?
    }
}

async fn accept_loop(
    listener: TcpListener,
    tls: Option<tokio_rustls::TlsAcceptor>,
    requests: mpsc::Sender<io::Result<Request>>,
) {
    let listener = match tokio::net::TcpListener::from_std(listener) {
        Ok(listener) => listener,
        Err(error) => {
            let _ = requests.send(Err(error));
            return;
        }
    };
    let secure = tls.is_some();
    loop {
        let (socket, _) = match listener.accept().await {
            Ok(pair) => pair,
            Err(error) => {
                let _ = requests.send(Err(error));
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                continue;
            }
        };
        let requests = requests.clone();
        let tls = tls.clone();
        tokio::spawn(async move {
            let service = service_fn(move |request| receive(request, requests.clone(), secure));
            // Flush when a producer is waiting for more bytes.
            // Half-close permits a client to finish uploading
            // before waiting for its streamed response.
            let mut builder = http1::Builder::new();
            builder
                .timer(TokioTimer::new())
                .header_read_timeout(std::time::Duration::from_secs(30))
                .half_close(true)
                .keep_alive(true)
                .pipeline_flush(false);
            match tls {
                Some(acceptor) => {
                    // A handshake that fails - a client that does not trust
                    // this certificate, say - is that client's problem alone.
                    if let Ok(stream) = acceptor.accept(socket).await {
                        let _ = builder
                            .serve_connection(TokioIo::new(stream), service)
                            .await;
                    }
                }
                None => {
                    let _ = builder
                        .serve_connection(TokioIo::new(socket), service)
                        .await;
                }
            }
        });
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

pub(super) struct Request {
    method: Method,
    path: String,
    headers: Vec<Header>,
    body: Vec<u8>,
    response: Option<oneshot::Sender<hyper::Response<Body>>>,
    secure: bool,
}

async fn receive(
    request: hyper::Request<Incoming>,
    requests: mpsc::Sender<io::Result<Request>>,
    secure: bool,
) -> io::Result<hyper::Response<Body>> {
    let (parts, body) = request.into_parts();
    let body = body.collect().await.map_err(io::Error::other)?.to_bytes();
    let (response, received) = oneshot::channel();
    let request = Request {
        method: parts
            .method
            .as_str()
            .parse()
            .map_err(|_| io::Error::other("invalid HTTP request method"))?,
        path: parts
            .uri
            .path_and_query()
            .map_or("/", |p| p.as_str())
            .to_owned(),
        headers: parts
            .headers
            .iter()
            .filter_map(|(name, value)| {
                Header::from_bytes(name.as_str().as_bytes(), value.as_bytes()).ok()
            })
            .collect(),
        body: body.to_vec(),
        response: Some(response),
        secure,
    };
    requests
        .send(Ok(request))
        .map_err(|_| io::Error::other("HTTP handler stopped"))?;
    received
        .await
        .map_err(|_| io::Error::other("HTTP handler stopped before responding"))
}

impl Request {
    pub fn method(&self) -> &Method {
        &self.method
    }
    pub fn url(&self) -> &str {
        &self.path
    }
    pub fn headers(&self) -> &[Header] {
        &self.headers
    }
    /// Arrived on the TLS listener - the one Codex's `chatgpt_base_url` names.
    pub fn is_tls(&self) -> bool {
        self.secure
    }
    pub fn take_body(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.body)
    }

    pub fn respond<R: Read>(mut self, response: Response<R>) -> io::Result<()> {
        let (sender, receiver) = body_channel::channel(1);
        let mut output = hyper::Response::builder().status(response.status_code().0);
        for header in response.headers() {
            output = output.header(header.field.as_str().as_str(), header.value.as_str());
        }
        let output = output
            .body(Body {
                receiver,
                remaining: response.data_length().map(|n| n as u64),
                complete: response.data_length() == Some(0),
                flush_pending: true,
            })
            .map_err(io::Error::other)?;
        self.response
            .take()
            .expect("one response")
            .send(output)
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "client disconnected"))?;
        if response.data_length() == Some(0) {
            return Ok(());
        }
        let mut remaining = response.data_length();
        let mut reader = response.into_reader();
        let mut buffer = [0; 16 * 1024];
        loop {
            let capacity = remaining.unwrap_or(buffer.len()).min(buffer.len());
            let event = match reader.read(&mut buffer[..capacity]) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    let _ = sender.blocking_send(BodyEvent::Failed(io::Error::new(
                        error.kind(),
                        error.to_string(),
                    )));
                    return Err(error);
                }
                Ok(0) if remaining.is_some_and(|left| left > 0) => {
                    let error = io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "response ended before its declared length",
                    );
                    let _ = sender.blocking_send(BodyEvent::Failed(io::Error::new(
                        error.kind(),
                        error.to_string(),
                    )));
                    return Err(error);
                }
                Ok(0) => BodyEvent::Done,
                Ok(count) => {
                    if let Some(left) = &mut remaining {
                        *left -= count;
                    }
                    BodyEvent::Data(Bytes::copy_from_slice(&buffer[..count]))
                }
            };
            let done = matches!(event, BodyEvent::Done);
            sender
                .blocking_send(event)
                .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "client disconnected"))?;
            if done || remaining == Some(0) {
                return Ok(());
            }
        }
    }
}

impl Drop for Request {
    fn drop(&mut self) {
        if let Some(sender) = self.response.take() {
            let (_, receiver) = body_channel::channel(1);
            let mut response = hyper::Response::new(Body {
                receiver,
                remaining: Some(0),
                complete: true,
                flush_pending: false,
            });
            *response.status_mut() = hyper::StatusCode::INTERNAL_SERVER_ERROR;
            let _ = sender.send(response);
        }
    }
}

enum BodyEvent {
    Data(Bytes),
    Done,
    Failed(io::Error),
}

struct Body {
    receiver: body_channel::Receiver<BodyEvent>,
    remaining: Option<u64>,
    complete: bool,
    flush_pending: bool,
}

impl HttpBody for Body {
    type Data = Bytes;
    type Error = io::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<io::Result<Frame<Bytes>>>> {
        if self.complete {
            return Poll::Ready(None);
        }
        // Give HTTP framing a flush point before reading more. Even if the
        // producer already queued the next fragment or an error, the headers
        // and preceding bytes must reach the socket first.
        if self.flush_pending {
            self.flush_pending = false;
            cx.waker().wake_by_ref();
            return Poll::Pending;
        }
        match self.receiver.poll_recv(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Some(BodyEvent::Data(data))) => {
                self.flush_pending = true;
                if let Some(remaining) = &mut self.remaining {
                    *remaining = remaining.saturating_sub(data.len() as u64);
                }
                self.complete = self.remaining == Some(0);
                Poll::Ready(Some(Ok(Frame::data(data))))
            }
            Poll::Ready(Some(BodyEvent::Done)) => {
                self.complete = true;
                Poll::Ready(None)
            }
            Poll::Ready(event) => {
                self.complete = true;
                let error = match event {
                    Some(BodyEvent::Failed(error)) => error,
                    _ => io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "response producer stopped before EOF",
                    ),
                };
                Poll::Ready(Some(Err(error)))
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.complete
    }
    fn size_hint(&self) -> SizeHint {
        let mut hint = SizeHint::new();
        if let Some(remaining) = self.remaining {
            hint.set_exact(remaining);
        }
        hint
    }
}

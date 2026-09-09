//! Small header-only HTTP transport for the dashboard's bodyless API.
//! No body reader/destructor, worker threads, or unbounded request queue.
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};
use tiny_http::{HTTPVersion, Header, Method, Response};

const CONNECTION_LIMIT: usize = 16;
const HEADER_LIMIT: usize = 8192;
const IO_BUDGET: Duration = Duration::from_millis(250);

pub struct Server {
    listener: TcpListener,
    pending: Vec<Pending>,
}
struct Pending {
    stream: TcpStream,
    bytes: Vec<u8>,
    deadline: Instant,
}
pub struct Request {
    stream: TcpStream,
    method: Method,
    url: String,
    headers: Vec<Header>,
}
impl Server {
    pub fn http(address: &str) -> io::Result<Self> {
        let listener = TcpListener::bind(address)?;
        listener.set_nonblocking(true)?;
        Ok(Self {
            listener,
            pending: Vec::new(),
        })
    }
    pub fn recv_timeout(&mut self, timeout: Duration) -> io::Result<Option<Request>> {
        let deadline = Instant::now() + timeout;
        loop {
            // Bound work per tick even if a peer floods new connections.
            for _ in 0..CONNECTION_LIMIT {
                match self.listener.accept() {
                    Ok((stream, _)) => {
                        if self.pending.len() < CONNECTION_LIMIT {
                            stream.set_nonblocking(true)?;
                            self.pending.push(Pending {
                                stream,
                                bytes: Vec::new(),
                                deadline: Instant::now() + IO_BUDGET,
                            });
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                    Err(error) => return Err(error),
                }
            }
            let mut index = 0;
            while index < self.pending.len() {
                let pending = &mut self.pending[index];
                let mut buffer = [0; 1024];
                let expired = Instant::now() >= pending.deadline;
                let read = if expired {
                    Ok(0)
                } else {
                    pending.stream.read(&mut buffer)
                };
                match read {
                    Ok(0) => {
                        self.pending.swap_remove(index);
                        continue;
                    }
                    Ok(count) => {
                        pending.bytes.extend_from_slice(&buffer[..count]);
                        if let Some(end) = pending.bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                            let pending = self.pending.swap_remove(index);
                            if end <= HEADER_LIMIT {
                                if let Some(request) =
                                    Request::parse(pending.stream, &pending.bytes[..end])
                                {
                                    return Ok(Some(request));
                                }
                            }
                            continue;
                        }
                        if pending.bytes.len() >= HEADER_LIMIT {
                            self.pending.swap_remove(index);
                            continue;
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                    Err(_) => {
                        self.pending.swap_remove(index);
                        continue;
                    }
                }
                index += 1;
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}
impl Request {
    fn parse(stream: TcpStream, bytes: &[u8]) -> Option<Self> {
        let text = std::str::from_utf8(bytes).ok()?;
        let mut lines = text.split("\r\n");
        let mut first = lines.next()?.split(' ');
        let method = first.next()?.parse().ok()?;
        let url = first.next()?.to_owned();
        if !url.starts_with('/') || url.chars().any(char::is_control) {
            return None;
        }
        if !matches!(first.next()?, "HTTP/1.1" | "HTTP/1.0") || first.next().is_some() {
            return None;
        }
        let headers: Vec<Header> = lines.map(str::parse).collect::<Result<_, _>>().ok()?;
        // Duplicate Host/token values are ambiguous and must never authorize a request.
        for name in ["Host", "X-Portboard-Token"] {
            if headers.iter().filter(|h| h.field.equiv(name)).count() > 1 {
                return None;
            }
        }
        Some(Self {
            stream,
            method,
            url,
            headers,
        })
    }
    pub fn method(&self) -> &Method {
        &self.method
    }
    pub fn url(&self) -> &str {
        &self.url
    }
    pub fn headers(&self) -> &[Header] {
        &self.headers
    }
    pub fn respond<R: Read>(self, response: Response<R>) -> io::Result<()> {
        self.stream.set_nonblocking(false)?;
        let mut writer = DeadlineWriter {
            stream: self.stream,
            deadline: Instant::now() + IO_BUDGET,
        };
        let result = response.raw_print(
            &mut writer,
            HTTPVersion(1, 1),
            &self.headers,
            self.method == Method::Head,
            None,
        );
        // Close rather than drain any declared or pipelined request body.
        let _ = writer.stream.shutdown(std::net::Shutdown::Both);
        result
    }
}
struct DeadlineWriter {
    stream: TcpStream,
    deadline: Instant,
}
impl Write for DeadlineWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let remaining = self
            .deadline
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())
            .ok_or_else(|| io::Error::from(io::ErrorKind::TimedOut))?;
        self.stream.set_write_timeout(Some(remaining))?;
        self.stream.write(bytes)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

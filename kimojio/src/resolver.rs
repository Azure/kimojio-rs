// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Asynchronous host name resolution.
//!
//! The resolver uses the local systemd-resolved service through its Varlink
//! socket. If that service is not available, a helper thread for the full
//! process does the blocking name resolution of the platform.

use std::{
    error, fmt, io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV6, ToSocketAddrs},
    rc::Rc,
    sync::{Arc, Mutex, OnceLock, mpsc},
};

use rustix::{
    event::{EventfdFlags, eventfd},
    fd::OwnedFd,
};

pub mod varlink_reply;

use varlink_reply::{
    EscapeString, ReplyError, ReplyVisitor, ResolveHostnameReply, ResolvedAddress,
};

use crate::{
    AsyncLock, AsyncStreamRead, AsyncStreamWrite, Errno, OwnedFdStream,
    operations::{self, AddressFamily, SocketAddrUnix, SocketType},
};

const RESOLVED_SOCKET: &str = "/run/systemd/resolve/io.systemd.Resolve";
const RESOLVE_HOSTNAME_METHOD: &str = "io.systemd.Resolve.ResolveHostname";
const MAX_VARLINK_REPLY_SIZE: usize = 64 * 1024;
/// How much spare capacity each read adds to the reply buffer.
const VARLINK_READ_CHUNK: usize = 4096;

/// An error from [`resolve`].
#[derive(Debug)]
#[non_exhaustive]
pub enum ResolveError {
    /// The resolver returned no address for the requested host.
    HostNotFound {
        /// The host that did not resolve.
        host: String,
    },
    /// The system resolver returned an invalid protocol message.
    Protocol {
        /// A description of the invalid message.
        detail: String,
    },
    /// An asynchronous I/O operation of the resolver failed.
    Io {
        /// The operation that failed.
        operation: &'static str,
        /// The I/O error.
        source: Errno,
    },
    /// The blocking name resolution of the platform failed.
    System {
        /// The host that did not resolve.
        host: String,
        /// The error from the resolver of the platform.
        source: io::Error,
    },
    /// The blocking helper for the full process did not accept the request.
    HelperUnavailable {
        /// A description of the failure of the helper.
        detail: String,
    },
    /// The task was canceled during a wait for resolver state.
    Canceled,
}

impl fmt::Display for ResolveError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HostNotFound { host } => {
                write!(formatter, "host `{host}` did not resolve to any addresses")
            }
            Self::Protocol { detail } => {
                write!(
                    formatter,
                    "invalid response from the system resolver: {detail}"
                )
            }
            Self::Io { operation, source } => {
                write!(formatter, "resolver I/O failed while {operation}: {source}")
            }
            Self::System { host, source } => {
                write!(formatter, "failed to resolve `{host}`: {source}")
            }
            Self::HelperUnavailable { detail } => {
                write!(
                    formatter,
                    "blocking resolver helper is unavailable: {detail}"
                )
            }
            Self::Canceled => formatter.write_str("name resolution was canceled"),
        }
    }
}

impl error::Error for ResolveError {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::System { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ResolverMode {
    Unknown,
    Varlink,
    Blocking,
}

struct ResolverState {
    mode: ResolverMode,
    connection: Option<VarlinkConnection>,
    endpoint: &'static str,
    #[cfg(test)]
    connect_attempts: usize,
}

impl ResolverState {
    fn new(endpoint: &'static str) -> Self {
        Self {
            mode: ResolverMode::Unknown,
            connection: None,
            endpoint,
            #[cfg(test)]
            connect_attempts: 0,
        }
    }

    async fn connect(&mut self) -> Result<VarlinkConnection, Errno> {
        #[cfg(test)]
        {
            self.connect_attempts += 1;
        }
        connect_varlink(self.endpoint).await
    }

    async fn preferred_lookup(&mut self, host: &str, port: u16) -> PreferredOutcome {
        let request = match encode_varlink_request(host) {
            Ok(request) => request,
            Err(error) => return PreferredOutcome::Reply(Err(error)),
        };

        if self.connection.is_none() {
            self.connection = match self.connect().await {
                Ok(connection) => Some(connection),
                Err(_) => return PreferredOutcome::TransportFailure,
            };
        }

        let first = match self.connection.as_mut() {
            Some(connection) => exchange_varlink(connection, &request, host, port).await,
            None => return PreferredOutcome::TransportFailure,
        };
        match first {
            Ok(addresses) => return PreferredOutcome::Reply(Ok(addresses)),
            Err(ExchangeError::Response(error)) => {
                if matches!(error, ResolveError::Protocol { .. }) {
                    self.connection = None;
                }
                return PreferredOutcome::Reply(Err(error));
            }
            Err(ExchangeError::Transport) => {
                self.connection = None;
            }
        }

        self.connection = match self.connect().await {
            Ok(connection) => Some(connection),
            Err(_) => return PreferredOutcome::TransportFailure,
        };

        let retried = match self.connection.as_mut() {
            Some(connection) => exchange_varlink(connection, &request, host, port).await,
            None => return PreferredOutcome::TransportFailure,
        };
        match retried {
            Ok(addresses) => PreferredOutcome::Reply(Ok(addresses)),
            Err(ExchangeError::Response(error)) => {
                if matches!(error, ResolveError::Protocol { .. }) {
                    self.connection = None;
                }
                PreferredOutcome::Reply(Err(error))
            }
            Err(ExchangeError::Transport) => {
                self.connection = None;
                PreferredOutcome::TransportFailure
            }
        }
    }
}

thread_local! {
    static RESOLVER_STATE: Rc<AsyncLock<ResolverState>> =
        Rc::new(AsyncLock::new(ResolverState::new(RESOLVED_SOCKET)));
}

enum PreferredOutcome {
    Reply(Result<Vec<SocketAddr>, ResolveError>),
    TransportFailure,
}

/// Resolves `host` into socket addresses that contain `port`.
///
/// The returned vector always contains at least one address.
pub async fn resolve(host: &str, port: u16) -> Result<Vec<SocketAddr>, ResolveError> {
    // An address literal needs no name service. This test keeps the usual
    // `http://127.0.0.1:8080/` case away from the resolver. Without the test,
    // that case costs one exchange with a service that only returns the literal
    // again. The test also keeps literals usable if no resolver is available.
    if let Some(address) = parse_address_literal(host, port) {
        return Ok(vec![address]);
    }

    let state = RESOLVER_STATE.with(Rc::clone);
    let mut state = state.lock().await.map_err(|_| ResolveError::Canceled)?;

    if state.mode != ResolverMode::Blocking {
        match state.preferred_lookup(host, port).await {
            PreferredOutcome::Reply(result) => {
                state.mode = ResolverMode::Varlink;
                return result;
            }
            PreferredOutcome::TransportFailure => {
                state.mode = ResolverMode::Blocking;
            }
        }
    }

    drop(state);
    resolve_blocking(host, port).await
}

struct VarlinkConnection {
    stream: OwnedFdStream,
    input: Vec<u8>,
}

impl VarlinkConnection {
    fn new(stream: OwnedFdStream) -> Self {
        Self {
            stream,
            input: Vec::new(),
        }
    }

    async fn read_frame(&mut self) -> Result<Vec<u8>, ExchangeError> {
        loop {
            if let Some(terminator) = self.input.iter().position(|byte| *byte == 0) {
                if terminator > MAX_VARLINK_REPLY_SIZE {
                    self.input.clear();
                    return Err(ExchangeError::Response(protocol_error(
                        "reply exceeded the 65536-byte limit",
                    )));
                }

                let mut frame = self.input.drain(..=terminator).collect::<Vec<_>>();
                if frame.pop() != Some(0) {
                    return Err(ExchangeError::Response(protocol_error(
                        "reply did not end with a NUL byte",
                    )));
                }
                return Ok(frame);
            }

            if self.input.len() > MAX_VARLINK_REPLY_SIZE {
                self.input.clear();
                return Err(ExchangeError::Response(protocol_error(
                    "reply exceeded the 65536-byte limit",
                )));
            }

            let remaining = MAX_VARLINK_REPLY_SIZE + 1 - self.input.len();
            // Read directly into the spare capacity of the reply buffer. A
            // scratch array on the stack would stay alive across the await
            // point. That would increase the size of each future that contains
            // a lookup, and would cost a second copy of the data.
            let read_size = remaining.min(VARLINK_READ_CHUNK);
            let filled = self.input.len();
            self.input.resize(filled + read_size, 0);
            let amount = self
                .stream
                .try_read(&mut self.input[filled..], None)
                .await
                .map_err(|_| ExchangeError::Transport)?;
            self.input.truncate(filled + amount);
            if amount == 0 {
                return Err(ExchangeError::Transport);
            }
        }
    }
}

async fn connect_varlink(endpoint: &str) -> Result<VarlinkConnection, Errno> {
    let socket = operations::socket(AddressFamily::UNIX, SocketType::STREAM, None).await?;
    let address = SocketAddrUnix::new(endpoint)?;
    operations::connect_unix(&socket, &address).await?;
    Ok(VarlinkConnection::new(OwnedFdStream::new(socket)))
}

enum ExchangeError {
    Transport,
    Response(ResolveError),
}

async fn exchange_varlink(
    connection: &mut VarlinkConnection,
    request: &[u8],
    host: &str,
    port: u16,
) -> Result<Vec<SocketAddr>, ExchangeError> {
    connection
        .stream
        .write(request, None)
        .await
        .map_err(|_| ExchangeError::Transport)?;
    let frame = connection.read_frame().await?;
    decode_varlink_reply(&frame, host, port).map_err(ExchangeError::Response)
}

fn encode_varlink_request(host: &str) -> Result<Vec<u8>, ResolveError> {
    const PREFIX: &[u8] = br#"{"method":""#;
    const PARAMETERS: &[u8] = br#"","parameters":{"name":""#;
    const SUFFIX: &[u8] = br#""}}"#;

    let mut encoded = Vec::with_capacity(
        PREFIX
            .len()
            .saturating_add(RESOLVE_HOSTNAME_METHOD.len())
            .saturating_add(PARAMETERS.len())
            .saturating_add(host.len())
            .saturating_add(SUFFIX.len())
            .saturating_add(1),
    );
    encoded.extend_from_slice(PREFIX);
    encoded.extend_from_slice(RESOLVE_HOSTNAME_METHOD.as_bytes());
    encoded.extend_from_slice(PARAMETERS);
    encode_json_string_contents(host, &mut encoded);
    encoded.extend_from_slice(SUFFIX);
    encoded.push(0);
    Ok(encoded)
}

fn encode_json_string_contents(value: &str, encoded: &mut Vec<u8>) {
    for character in value.chars() {
        match character {
            '"' => encoded.extend_from_slice(br#"\""#),
            '\\' => encoded.extend_from_slice(br#"\\"#),
            '/' => encoded.extend_from_slice(br#"\/"#),
            '\u{0008}' => encoded.extend_from_slice(br#"\b"#),
            '\u{000c}' => encoded.extend_from_slice(br#"\f"#),
            '\n' => encoded.extend_from_slice(br#"\n"#),
            '\r' => encoded.extend_from_slice(br#"\r"#),
            '\t' => encoded.extend_from_slice(br#"\t"#),
            '\u{0000}'..='\u{001f}' => {
                let value = character as u8;
                encoded.extend_from_slice(br#"\u00"#);
                encoded.push(hex_digit(value >> 4));
                encoded.push(hex_digit(value & 0x0f));
            }
            _ => {
                let mut buffer = [0_u8; 4];
                encoded.extend_from_slice(character.encode_utf8(&mut buffer).as_bytes());
            }
        }
    }
}

fn hex_digit(value: u8) -> u8 {
    match value {
        0..=9 => b'0' + value,
        _ => b'a' + value.saturating_sub(10),
    }
}

/// Converts each address of a reply as it arrives.
///
/// The decoder pushes each item into this visitor. The caller does not pull the
/// items one at a time. A full reply is therefore one pass, and the only
/// buffered data is the address that the visitor converts at that moment.
struct CollectAddresses {
    addresses: Vec<SocketAddr>,
    port: u16,
    /// Whether the reply contained a Varlink method error. systemd-resolved
    /// reports "no such host" in this form.
    method_error: bool,
}

impl<'a> ReplyVisitor<'a> for CollectAddresses {
    /// A conversion that fails stops the decode, and the error leaves through
    /// `drive`. This visitor stops the decode for no other reason.
    type Output = ResolveError;

    fn address(&mut self, address: ResolvedAddress) -> Option<ResolveError> {
        match socket_address(&address, self.port) {
            Ok(socket) => {
                self.addresses.push(socket);
                None
            }
            Err(error) => Some(error),
        }
    }

    fn error(&mut self, _: &'a str) -> Option<ResolveError> {
        self.method_error = true;
        None
    }

    // The canonical name and the lookup flags contain no data that the caller
    // requested, because `resolve` received the host name already. `name` and
    // `flags` therefore keep their empty default body. `escape_name` has no
    // default body, and it is empty here for the same reason.
    fn escape_name(&mut self, _: EscapeString<'a>) -> Option<ResolveError> {
        None
    }
}

fn decode_varlink_reply(
    reply: &[u8],
    host: &str,
    port: u16,
) -> Result<Vec<SocketAddr>, ResolveError> {
    if reply.len() > MAX_VARLINK_REPLY_SIZE {
        return Err(protocol_error("reply exceeded the 65536-byte limit"));
    }

    // A frame arrives from the socket as bytes, and JSON is UTF-8 by
    // definition. The decoder therefore accepts text, and this function
    // converts the frame one time at the position where the bytes arrive. A
    // frame that is not UTF-8 is not JSON, and this reports it in the
    // vocabulary of the resolver.
    let reply = core::str::from_utf8(reply).map_err(|error| {
        protocol_error(format!(
            "reply was not valid UTF-8 after {} bytes",
            error.valid_up_to()
        ))
    })?;

    let mut collect = CollectAddresses {
        addresses: Vec::new(),
        port,
        method_error: false,
    };
    if let Some(error) = ResolveHostnameReply::new(reply)
        .drive(&mut collect)
        .map_err(decode_error)?
    {
        return Err(error);
    }

    if collect.method_error || collect.addresses.is_empty() {
        return Err(ResolveError::HostNotFound {
            host: host.to_owned(),
        });
    }
    Ok(collect.addresses)
}

/// Decodes a Varlink reply, together with its framing, for coverage-guided
/// fuzz tests.
#[cfg(feature = "fuzzing")]
#[doc(hidden)]
pub fn fuzz_decode_varlink_reply(wire: &[u8]) {
    let result = match wire.iter().position(|byte| *byte == 0) {
        Some(terminator) if terminator > MAX_VARLINK_REPLY_SIZE => {
            Err(protocol_error("reply exceeded the 65536-byte limit"))
        }
        Some(terminator) => decode_varlink_reply(&wire[..terminator], "fuzz.invalid", 443),
        None if wire.len() > MAX_VARLINK_REPLY_SIZE => {
            Err(protocol_error("reply exceeded the 65536-byte limit"))
        }
        None => Err(protocol_error("reply did not end with a NUL byte")),
    };
    drop(result);
}

/// Combines a decoded address with `port`, and rejects a combination of family
/// and length that is not valid.
///
/// The decoder reports the content of the reply. The decision if four bytes are
/// correct for the family in the reply needs socket knowledge, thus this
/// function makes that decision.
fn socket_address(address: &ResolvedAddress, port: u16) -> Result<SocketAddr, ResolveError> {
    match address.family() {
        family if family == libc::AF_INET => {
            let octets = <[u8; 4]>::try_from(address.octets())
                .map_err(|_| protocol_error("IPv4 address did not contain exactly 4 bytes"))?;
            Ok(SocketAddr::new(IpAddr::V4(Ipv4Addr::from(octets)), port))
        }
        family if family == libc::AF_INET6 => {
            let octets = <[u8; 16]>::try_from(address.octets())
                .map_err(|_| protocol_error("IPv6 address did not contain exactly 16 bytes"))?;
            // systemd-resolved omits `ifindex` for a synthesized answer such as
            // an IP literal. The decoder reports that condition as zero, which
            // is the correct value for an IPv6 address with no scope.
            let scope = u32::try_from(address.ifindex())
                .map_err(|_| protocol_error("`ifindex` was negative"))?;
            Ok(SocketAddr::V6(SocketAddrV6::new(
                Ipv6Addr::from(octets),
                port,
                0,
                scope,
            )))
        }
        family => Err(protocol_error(format!(
            "address entry used unsupported family {family}"
        ))),
    }
}

/// Describes a rejected reply as a protocol failure.
fn decode_error(error: ReplyError) -> ResolveError {
    protocol_error(format!(
        "reply was not a valid ResolveHostname response: {error}"
    ))
}

/// Parses `host` as an IPv4 or IPv6 address literal. The function accepts the
/// IPv6 form with brackets that a URL authority uses.
fn parse_address_literal(host: &str, port: u16) -> Option<SocketAddr> {
    let candidate = host
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or(host);
    candidate
        .parse::<IpAddr>()
        .ok()
        .map(|address| SocketAddr::new(address, port))
}

fn protocol_error(detail: impl Into<String>) -> ResolveError {
    ResolveError::Protocol {
        detail: detail.into(),
    }
}

type BlockingResult = io::Result<Vec<SocketAddr>>;

struct BlockingJob {
    host: String,
    port: u16,
    result: Arc<Mutex<Option<BlockingResult>>>,
    wakeup: OwnedFd,
}

static BLOCKING_HELPER: OnceLock<Result<mpsc::Sender<BlockingJob>, String>> = OnceLock::new();

#[cfg(test)]
static BLOCKING_HELPER_STARTS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

fn blocking_helper() -> Result<&'static mpsc::Sender<BlockingJob>, ResolveError> {
    match BLOCKING_HELPER.get_or_init(|| {
        let (sender, receiver) = mpsc::channel();
        std::thread::Builder::new()
            .name("kimojio-dns-resolver".to_owned())
            .spawn(move || blocking_helper_loop(receiver))
            .map(|_| sender)
            .map_err(|error| error.to_string())
    }) {
        Ok(sender) => Ok(sender),
        Err(detail) => Err(ResolveError::HelperUnavailable {
            detail: detail.clone(),
        }),
    }
}

fn blocking_helper_loop(receiver: mpsc::Receiver<BlockingJob>) {
    #[cfg(test)]
    BLOCKING_HELPER_STARTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    for job in receiver {
        let result = (job.host.as_str(), job.port)
            .to_socket_addrs()
            .map(|addresses| addresses.collect());

        match job.result.lock() {
            Ok(mut slot) => *slot = Some(result),
            Err(poisoned) => *poisoned.into_inner() = Some(result),
        }

        let signal = 1_u64.to_ne_bytes();
        loop {
            match rustix::io::write(&job.wakeup, &signal) {
                Err(error) if error == rustix::io::Errno::INTR => continue,
                _ => break,
            }
        }
    }
}

async fn resolve_blocking(host: &str, port: u16) -> Result<Vec<SocketAddr>, ResolveError> {
    let wakeup = eventfd(0, EventfdFlags::CLOEXEC).map_err(|source| ResolveError::Io {
        operation: "creating the helper wakeup eventfd",
        source,
    })?;
    let helper_wakeup = crate::try_clone_owned_fd(&wakeup).map_err(|source| ResolveError::Io {
        operation: "cloning the helper wakeup eventfd",
        source,
    })?;
    let result = Arc::new(Mutex::new(None));
    let job = BlockingJob {
        host: host.to_owned(),
        port,
        result: Arc::clone(&result),
        wakeup: helper_wakeup,
    };
    blocking_helper()?
        .send(job)
        .map_err(|error| ResolveError::HelperUnavailable {
            detail: error.to_string(),
        })?;

    // Read the eventfd directly and not through `OwnedFdStream`, which contains
    // a read buffer of 16 KiB. That buffer would stay alive across this await
    // point and would increase the size of each future that contains a lookup,
    // including each HTTP request in progress. The data is a counter of eight
    // bytes.
    let mut signal = [0_u8; 8];
    let mut filled = 0;
    while filled < signal.len() {
        let amount = operations::read(&wakeup, &mut signal[filled..])
            .await
            .map_err(|source| ResolveError::Io {
                operation: "waiting for the blocking resolver helper",
                source,
            })?;
        if amount == 0 {
            return Err(ResolveError::HelperUnavailable {
                detail: "the blocking resolver helper closed its wakeup".to_owned(),
            });
        }
        filled += amount;
    }

    let result = match result.lock() {
        Ok(mut slot) => slot.take(),
        Err(poisoned) => poisoned.into_inner().take(),
    }
    .ok_or_else(|| ResolveError::HelperUnavailable {
        detail: "helper signaled without storing a result".to_owned(),
    })?;
    let addresses = result.map_err(|source| ResolveError::System {
        host: host.to_owned(),
        source,
    })?;

    if addresses.is_empty() {
        Err(ResolveError::HostNotFound {
            host: host.to_owned(),
        })
    } else {
        Ok(addresses)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        net::{Ipv4Addr, Ipv6Addr, SocketAddrV4},
        path::Path,
        sync::atomic::Ordering,
    };

    use futures::future::join_all;

    use super::*;

    const UNAVAILABLE_SOCKET: &str = "/run/kimojio-resolver-test-does-not-exist";

    fn assert_protocol_error(result: Result<Vec<SocketAddr>, ResolveError>) {
        assert!(
            matches!(result, Err(ResolveError::Protocol { .. })),
            "expected protocol error, got {result:?}"
        );
    }

    async fn set_test_state(mode: ResolverMode, endpoint: &'static str) {
        let state = RESOLVER_STATE.with(Rc::clone);
        let mut state = state.lock().await.unwrap();
        *state = ResolverState::new(endpoint);
        state.mode = mode;
    }

    async fn set_test_connection(connection: OwnedFd, endpoint: &'static str) {
        let state = RESOLVER_STATE.with(Rc::clone);
        let mut state = state.lock().await.unwrap();
        *state = ResolverState::new(endpoint);
        state.mode = ResolverMode::Varlink;
        state.connection = Some(VarlinkConnection::new(OwnedFdStream::new(connection)));
    }

    async fn state_snapshot() -> (ResolverMode, usize, bool) {
        let state = RESOLVER_STATE.with(Rc::clone);
        let state = state.lock().await.unwrap();
        (
            state.mode,
            state.connect_attempts,
            state.connection.is_some(),
        )
    }

    fn serve_varlink_reply(peer: OwnedFd, reply: &'static [u8]) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            let mut request = Vec::new();
            while !request.contains(&0) {
                let mut buffer = [0_u8; 256];
                let amount = rustix::io::read(&peer, &mut buffer).unwrap();
                assert_ne!(amount, 0);
                request.extend(buffer.iter().take(amount));
            }

            let mut written = 0;
            while written < reply.len() {
                written += rustix::io::write(&peer, &reply[written..]).unwrap();
            }
        })
    }

    #[test]
    fn varlink_request_encoding() {
        let request = encode_varlink_request("example.com").unwrap();
        assert_eq!(
            request,
            br#"{"method":"io.systemd.Resolve.ResolveHostname","parameters":{"name":"example.com"}}"#
                .iter()
                .copied()
                .chain([0])
                .collect::<Vec<_>>()
        );

        let escaped = encode_varlink_request("quote\"and\\slash").unwrap();
        assert_eq!(escaped.last(), Some(&0));
        assert!(
            std::str::from_utf8(&escaped[..escaped.len() - 1])
                .unwrap()
                .contains(r#""name":"quote\"and\\slash""#)
        );
    }

    #[test]
    fn varlink_request_escapes_json_string_contents() {
        let host = "quote\"\\/\u{0008}\u{000c}\n\r\t\u{0000}\u{001f}é";
        let request = encode_varlink_request(host).unwrap();
        assert_eq!(request.last(), Some(&0));
        assert_eq!(
            std::str::from_utf8(&request[..request.len() - 1]).unwrap(),
            r#"{"method":"io.systemd.Resolve.ResolveHostname","parameters":{"name":"quote\"\\\/\b\f\n\r\t\u0000\u001fé"}}"#
        );
    }

    #[test]
    fn varlink_reply_ipv4_only() {
        let reply = br#"{"parameters":{"addresses":[{"ifindex":2,"family":2,"address":[104,20,23,154]}],"name":"example.com","flags":8388609}}"#;
        assert_eq!(
            decode_varlink_reply(reply, "example.com", 443).unwrap(),
            vec![SocketAddr::V4(SocketAddrV4::new(
                Ipv4Addr::new(104, 20, 23, 154),
                443
            ))]
        );
    }

    #[test]
    fn varlink_reply_ipv6_only() {
        let reply = br#"{"parameters":{"addresses":[{"ifindex":7,"family":10,"address":[32,1,13,184,0,0,0,0,0,0,0,0,0,0,0,1]}]}}"#;
        assert_eq!(
            decode_varlink_reply(reply, "example.com", 8443).unwrap(),
            vec![SocketAddr::V6(SocketAddrV6::new(
                "2001:db8::1".parse::<Ipv6Addr>().unwrap(),
                8443,
                0,
                7
            ))]
        );
    }

    #[test]
    fn varlink_reply_both_families() {
        let reply = br#"{"parameters":{"addresses":[{"ifindex":2,"family":2,"address":[192,0,2,1]},{"ifindex":0,"family":10,"address":[32,1,13,184,0,0,0,0,0,0,0,0,0,0,0,2]}]}}"#;
        assert_eq!(
            decode_varlink_reply(reply, "example.com", 80).unwrap(),
            vec![
                "192.0.2.1:80".parse::<SocketAddr>().unwrap(),
                "[2001:db8::2]:80".parse::<SocketAddr>().unwrap()
            ]
        );
    }

    /// systemd-resolved omits `ifindex` when it synthesizes an answer that is
    /// not bound to a link. These are the replies that it returned for
    /// `127.0.0.1` and `::1` on a host with systemd-resolved.
    #[test]
    fn varlink_reply_without_ifindex_decodes_as_unscoped() {
        let ipv4 = br#"{"parameters":{"addresses":[{"family":2,"address":[127,0,0,1]}],"name":"127.0.0.1","flags":786945}}"#;
        assert_eq!(
            decode_varlink_reply(ipv4, "127.0.0.1", 80).unwrap(),
            vec!["127.0.0.1:80".parse::<SocketAddr>().unwrap()]
        );

        let ipv6 = br#"{"parameters":{"addresses":[{"family":10,"address":[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,1]}],"name":"::1","flags":786945}}"#;
        let decoded = decode_varlink_reply(ipv6, "::1", 80).unwrap();
        assert_eq!(decoded, vec!["[::1]:80".parse::<SocketAddr>().unwrap()]);
        match decoded[0] {
            SocketAddr::V6(address) => assert_eq!(address.scope_id(), 0),
            SocketAddr::V4(_) => panic!("expected an IPv6 address"),
        }
    }

    /// These four inputs come from the `varlink_reply` fuzz target. The target
    /// found that the tokenizer of the previous JSON dependency underflowed on
    /// a single minus sign and unwound into the connection loop. `kimojio-json`
    /// rejects these inputs with an error. The inputs stay here as regression
    /// tests, thus a later decoder cannot add a path that panics again.
    #[test]
    fn varlink_reply_containing_a_parser_panic_input_is_an_error() {
        for wire in [
            b"-\n\0".as_slice(),
            b"-\0".as_slice(),
            b"[-]\0".as_slice(),
            b"{\"parameters\":{\"addresses\":[{\"family\":-}]}}\0",
        ] {
            let frame = &wire[..wire.len() - 1];
            let result = decode_varlink_reply(frame, "example.com", 80);
            assert!(
                result.is_err(),
                "expected an error for {:?}",
                String::from_utf8_lossy(frame)
            );
        }
    }

    /// The future from `resolve` is a part of the future of each caller,
    /// including each HTTP request in progress. Anything large that stays alive
    /// across an await point here is therefore multiplied by the number of
    /// concurrent requests. An earlier version wrapped the eventfd in an
    /// `OwnedFdStream`. That put its read buffer of 16 KiB into this future and
    /// caused a stack overflow in the tests of the connection pool.
    #[test]
    fn resolve_future_stays_small_enough_to_embed_in_callers() {
        let future = resolve("example.com", 80);
        let size = std::mem::size_of_val(&future);
        drop(future);
        assert!(
            size <= 2048,
            "resolve future grew to {size} bytes; something large stays alive across an await"
        );
    }

    #[test]
    fn address_literals_resolve_without_consulting_a_name_service() {
        assert_eq!(
            parse_address_literal("127.0.0.1", 8080),
            Some("127.0.0.1:8080".parse().unwrap())
        );
        assert_eq!(
            parse_address_literal("::1", 8080),
            Some("[::1]:8080".parse().unwrap())
        );
        // A URL authority puts an IPv6 literal in brackets.
        assert_eq!(
            parse_address_literal("[2001:db8::1]", 443),
            Some("[2001:db8::1]:443".parse().unwrap())
        );
        assert_eq!(parse_address_literal("example.com", 80), None);
        assert_eq!(parse_address_literal("", 80), None);
        assert_eq!(parse_address_literal("[not-an-address]", 80), None);
    }

    #[test]
    fn varlink_reply_address_fields_are_order_independent() {
        let reply = br#"{"ignored":{"nested":[null]},"parameters":{"ignored":[{"value":true}],"addresses":[{"address":[104,20,23,154],"extra":{"nested":false},"ifindex":2,"family":2}]}}"#;
        assert_eq!(
            decode_varlink_reply(reply, "example.com", 443).unwrap(),
            vec![SocketAddr::V4(SocketAddrV4::new(
                Ipv4Addr::new(104, 20, 23, 154),
                443
            ))]
        );
    }

    #[test]
    fn varlink_reply_rejects_nesting_beyond_limit() {
        let at_limit = br#"{"parameters":{"addresses":[]},"extra":{"a":{"b":{"c":{"d":null}}}}}"#;
        assert!(matches!(
            decode_varlink_reply(at_limit, "example.com", 80),
            Err(ResolveError::HostNotFound { .. })
        ));

        let beyond_limit =
            br#"{"parameters":{"addresses":[]},"extra":{"a":{"b":{"c":{"d":{"e":null}}}}}}"#;
        assert_protocol_error(decode_varlink_reply(beyond_limit, "example.com", 80));
    }

    #[test]
    fn varlink_error_reply_is_host_not_found() {
        let reply = br#"{"error":"io.systemd.Resolve.NoSuchResourceRecord","parameters":{}}"#;
        assert!(matches!(
            decode_varlink_reply(reply, "missing.example", 80),
            Err(ResolveError::HostNotFound { host }) if host == "missing.example"
        ));
    }

    #[test]
    fn varlink_error_field_is_order_independent() {
        let reply = br#"{"parameters":{},"error":"io.systemd.Resolve.NoSuchResourceRecord"}"#;
        assert!(matches!(
            decode_varlink_reply(reply, "missing.example", 80),
            Err(ResolveError::HostNotFound { host }) if host == "missing.example"
        ));
    }

    #[crate::test]
    async fn varlink_error_reply_does_not_select_fallback() {
        let (client, peer) = crate::pipe::bipipe();
        set_test_connection(client, UNAVAILABLE_SOCKET).await;
        let peer = serve_varlink_reply(
            peer,
            b"{\"error\":\"io.systemd.Resolve.NoSuchResourceRecord\",\"parameters\":{}}\0",
        );

        assert!(matches!(
            resolve("missing.example", 80).await,
            Err(ResolveError::HostNotFound { .. })
        ));
        assert_eq!(state_snapshot().await, (ResolverMode::Varlink, 0, true));
        peer.join().unwrap();
    }

    #[test]
    fn varlink_truncated_reply_is_error() {
        assert_protocol_error(decode_varlink_reply(
            br#"{"parameters":{"addresses":["#,
            "example.com",
            80,
        ));
    }

    #[test]
    fn varlink_wrong_length_address_is_error() {
        let reply =
            br#"{"parameters":{"addresses":[{"ifindex":2,"family":2,"address":[127,0,1]}]}}"#;
        assert_protocol_error(decode_varlink_reply(reply, "example.com", 80));
    }

    #[test]
    fn varlink_wrong_typed_field_is_error() {
        let reply =
            br#"{"parameters":{"addresses":[{"ifindex":2,"family":"2","address":[127,0,0,1]}]}}"#;
        assert_protocol_error(decode_varlink_reply(reply, "example.com", 80));
    }

    #[test]
    fn varlink_malformed_reply_shapes_are_errors() {
        let replies: &[&[u8]] = &[
            br#"[]"#,
            br#"{}"#,
            br#"{"parameters":[]}"#,
            br#"{"parameters":{}}"#,
            br#"{"parameters":{"addresses":{}}}"#,
            br#"{"parameters":{"addresses":[null]}}"#,
            br#"{"parameters":{"addresses":[{"ifindex":2,"address":[127,0,0,1]}]}}"#,
            br#"{"parameters":{"addresses":[{"ifindex":2,"family":99,"address":[127,0,0,1]}]}}"#,
            br#"{"parameters":{"addresses":[{"ifindex":4294967296,"family":2,"address":[127,0,0,1]}]}}"#,
            br#"{"parameters":{"addresses":[{"ifindex":2,"family":2,"address":[127,0,0,256]}]}}"#,
            br#"{"parameters":{"addresses":[{"ifindex":2,"family":2.0,"address":[127,0,0,1]}]}}"#,
            br#"{"error":false,"parameters":{}}"#,
            br#"{"parameters":{"addresses":[]}} null"#,
        ];

        for reply in replies {
            let result = decode_varlink_reply(reply, "example.com", 80);
            assert!(
                matches!(result, Err(ResolveError::Protocol { .. })),
                "expected protocol error for {reply:?}, got {result:?}"
            );
        }
    }

    #[test]
    fn varlink_empty_addresses_are_host_not_found() {
        assert!(matches!(
            decode_varlink_reply(
                br#"{"parameters":{"addresses":[]}}"#,
                "missing.example",
                80
            ),
            Err(ResolveError::HostNotFound { host }) if host == "missing.example"
        ));
    }

    #[test]
    fn varlink_parser_panic_is_contained() {
        assert_protocol_error(decode_varlink_reply(b"1e", "example.com", 80));
    }

    #[test]
    fn varlink_non_utf8_reply_is_error() {
        assert_protocol_error(decode_varlink_reply(&[0xff, 0xfe], "example.com", 80));
    }

    #[test]
    fn varlink_oversized_reply_is_error() {
        let reply = vec![b' '; MAX_VARLINK_REPLY_SIZE + 1];
        assert_protocol_error(decode_varlink_reply(&reply, "example.com", 80));
    }

    #[crate::test]
    async fn malformed_varlink_reply_does_not_poison_mode() {
        let (client, peer) = crate::pipe::bipipe();
        set_test_connection(client, UNAVAILABLE_SOCKET).await;
        let peer = serve_varlink_reply(peer, b"{\0");

        assert_protocol_error(resolve("example.com", 80).await);
        assert_eq!(state_snapshot().await, (ResolverMode::Varlink, 0, false));
        peer.join().unwrap();
    }

    #[crate::test]
    async fn varlink_frame_reader_rejects_oversized_reply() {
        let (reader, writer) = crate::pipe::bipipe();
        let peer = std::thread::spawn(move || {
            let reply = vec![b' '; MAX_VARLINK_REPLY_SIZE + 1];
            let mut written = 0;
            while written < reply.len() {
                written += rustix::io::write(&writer, &reply[written..]).unwrap();
            }
        });

        let mut connection = VarlinkConnection::new(OwnedFdStream::new(reader));
        assert!(matches!(
            connection.read_frame().await,
            Err(ExchangeError::Response(ResolveError::Protocol { .. }))
        ));
        peer.join().unwrap();
    }

    #[crate::test]
    async fn varlink_frame_reader_rejects_eof_without_nul() {
        let (reader, writer) = crate::pipe::bipipe();
        let peer = std::thread::spawn(move || {
            let reply = b"{}";
            let mut written = 0;
            while written < reply.len() {
                written += rustix::io::write(&writer, &reply[written..]).unwrap();
            }
        });

        let mut connection = VarlinkConnection::new(OwnedFdStream::new(reader));
        assert!(matches!(
            connection.read_frame().await,
            Err(ExchangeError::Transport)
        ));
        peer.join().unwrap();
    }

    #[crate::test]
    async fn fallback_helper_resolves_and_wakes_task() {
        set_test_state(ResolverMode::Blocking, UNAVAILABLE_SOCKET).await;
        let addresses = resolve("localhost", 43123).await.unwrap();
        assert!(
            addresses
                .iter()
                .any(|address| address.ip().is_loopback() && address.port() == 43123)
        );
    }

    #[crate::test]
    async fn fallback_handles_concurrent_lookups_on_one_thread() {
        set_test_state(ResolverMode::Blocking, UNAVAILABLE_SOCKET).await;
        let results = join_all((43130..43138).map(|port| resolve("localhost", port))).await;
        for (port, result) in (43130..43138).zip(results) {
            let addresses = result.unwrap();
            assert!(
                addresses
                    .iter()
                    .any(|address| address.ip().is_loopback() && address.port() == port)
            );
        }
    }

    #[crate::test]
    async fn fallback_helper_survives_failed_and_abandoned_requests() {
        set_test_state(ResolverMode::Blocking, UNAVAILABLE_SOCKET).await;
        assert!(matches!(
            resolve("\0", 43139).await,
            Err(ResolveError::System { .. })
        ));

        let abandoned_wakeup = eventfd(0, EventfdFlags::CLOEXEC).unwrap();
        let job = BlockingJob {
            host: "localhost".to_owned(),
            port: 43139,
            result: Arc::new(Mutex::new(None)),
            wakeup: crate::try_clone_owned_fd(&abandoned_wakeup).unwrap(),
        };
        blocking_helper().unwrap().send(job).unwrap();
        drop(abandoned_wakeup);

        let addresses = resolve("localhost", 43139).await.unwrap();
        assert!(addresses.iter().any(|address| address.ip().is_loopback()));
    }

    #[test]
    fn fallback_helper_is_shared_across_runtime_threads() {
        let threads = (0..2)
            .map(|thread_index| {
                std::thread::spawn(move || {
                    let result = crate::run(thread_index, async move {
                        set_test_state(ResolverMode::Blocking, UNAVAILABLE_SOCKET).await;
                        let addresses = resolve("localhost", 43140 + u16::from(thread_index))
                            .await
                            .unwrap();
                        assert!(addresses.iter().any(|address| address.ip().is_loopback()));
                    });
                    assert!(matches!(result, Some(Ok(()))));
                })
            })
            .collect::<Vec<_>>();
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(BLOCKING_HELPER_STARTS.load(Ordering::Relaxed), 1);
    }

    #[crate::test]
    async fn unavailable_varlink_mode_is_remembered() {
        set_test_state(ResolverMode::Unknown, UNAVAILABLE_SOCKET).await;
        assert!(!resolve("localhost", 43150).await.unwrap().is_empty());
        assert!(!resolve("localhost", 43151).await.unwrap().is_empty());
        assert_eq!(state_snapshot().await, (ResolverMode::Blocking, 1, false));
    }

    #[crate::test]
    async fn broken_varlink_connection_reconnects_before_fallback() {
        let (client, peer) = crate::pipe::bipipe();
        set_test_connection(client, UNAVAILABLE_SOCKET).await;
        let peer = std::thread::spawn(move || {
            let mut request = Vec::new();
            while !request.contains(&0) {
                let mut buffer = [0_u8; 256];
                let amount = rustix::io::read(&peer, &mut buffer).unwrap();
                assert_ne!(amount, 0);
                request.extend(buffer.iter().take(amount));
            }
        });

        let addresses = resolve("localhost", 43159).await.unwrap();
        assert!(addresses.iter().any(|address| address.ip().is_loopback()));
        assert_eq!(state_snapshot().await, (ResolverMode::Blocking, 1, false));
        peer.join().unwrap();
    }

    #[crate::test]
    async fn resolve_localhost_end_to_end() {
        set_test_state(ResolverMode::Unknown, RESOLVED_SOCKET).await;
        let addresses = resolve("localhost", 43160).await.unwrap();
        assert!(
            addresses
                .iter()
                .any(|address| address.ip().is_loopback() && address.port() == 43160)
        );
    }

    #[crate::test]
    async fn live_varlink_resolves_localhost_when_available() {
        if !Path::new(RESOLVED_SOCKET).exists() {
            return;
        }

        set_test_state(ResolverMode::Unknown, RESOLVED_SOCKET).await;
        let first = resolve("localhost", 43161).await.unwrap();
        let second = resolve("localhost", 43162).await.unwrap();
        assert!(first.iter().any(|address| address.ip().is_loopback()));
        assert!(second.iter().any(|address| address.ip().is_loopback()));
        assert_eq!(state_snapshot().await, (ResolverMode::Varlink, 1, true));
    }
}

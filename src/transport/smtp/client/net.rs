use std::{
    io::{self, Read, Write},
    mem,
    net::{IpAddr, Ipv4Addr, Shutdown, SocketAddr, SocketAddrV4, TcpStream, ToSocketAddrs},
    time::Duration,
};

#[cfg(feature = "native-tls")]
use native_tls::TlsStream;
#[cfg(feature = "rustls-tls")]
use rustls::{ClientConnection, ServerName, StreamOwned};
use socket2::{Domain, Protocol, Type};

#[cfg(any(feature = "native-tls", feature = "rustls-tls"))]
use super::InnerTlsParameters;
use super::TlsParameters;
use crate::transport::smtp::{error, Error};

/// A network stream
pub struct NetworkStream {
    inner: InnerNetworkStream,
}

/// Represents the different types of underlying network streams
// usually only one TLS backend at a time is going to be enabled,
// so clippy::large_enum_variant doesn't make sense here
#[allow(clippy::large_enum_variant)]
enum InnerNetworkStream {
    /// Plain TCP stream
    Tcp(TcpStream),
    /// Encrypted TCP stream
    #[cfg(feature = "native-tls")]
    NativeTls(TlsStream<TcpStream>),
    /// Encrypted TCP stream
    #[cfg(feature = "rustls-tls")]
    RustlsTls(StreamOwned<ClientConnection, TcpStream>),
    /// Can't be built
    None,
}

impl NetworkStream {
    fn new(inner: InnerNetworkStream) -> Self {
        if let InnerNetworkStream::None = inner {
            debug_assert!(false, "InnerNetworkStream::None must never be built");
        }

        NetworkStream { inner }
    }

    /// Returns peer's address
    pub fn peer_addr(&self) -> io::Result<SocketAddr> {
        match self.inner {
            InnerNetworkStream::Tcp(ref s) => s.peer_addr(),
            #[cfg(feature = "native-tls")]
            InnerNetworkStream::NativeTls(ref s) => s.get_ref().peer_addr(),
            #[cfg(feature = "rustls-tls")]
            InnerNetworkStream::RustlsTls(ref s) => s.get_ref().peer_addr(),
            InnerNetworkStream::None => {
                debug_assert!(false, "InnerNetworkStream::None must never be built");
                Ok(SocketAddr::V4(SocketAddrV4::new(
                    Ipv4Addr::new(127, 0, 0, 1),
                    80,
                )))
            }
        }
    }

    /// Shutdowns the connection
    pub fn shutdown(&self, how: Shutdown) -> io::Result<()> {
        match self.inner {
            InnerNetworkStream::Tcp(ref s) => s.shutdown(how),
            #[cfg(feature = "native-tls")]
            InnerNetworkStream::NativeTls(ref s) => s.get_ref().shutdown(how),
            #[cfg(feature = "rustls-tls")]
            InnerNetworkStream::RustlsTls(ref s) => s.get_ref().shutdown(how),
            InnerNetworkStream::None => {
                debug_assert!(false, "InnerNetworkStream::None must never be built");
                Ok(())
            }
        }
    }

    pub fn connect<T: ToSocketAddrs>(
        server: T,
        timeout: Option<Duration>,
        tls_parameters: Option<&TlsParameters>,
        local_addr: Option<IpAddr>,
    ) -> Result<NetworkStream, Error> {
        fn try_connect<T: ToSocketAddrs>(
            server: T,
            timeout: Option<Duration>,
            local_addr: Option<IpAddr>,
        ) -> Result<TcpStream, Error> {
            let addrs = server
                .to_socket_addrs()
                .map_err(error::connection)?
                .filter(|resolved_addr| resolved_address_filter(resolved_addr, local_addr));

            let mut last_err = None;

            for addr in addrs {
                let socket = socket2::Socket::new(
                    Domain::for_address(addr),
                    Type::STREAM,
                    Some(Protocol::TCP),
                )
                .map_err(error::connection)?;
                bind_local_address(&socket, &addr, local_addr)?;

                if let Some(timeout) = timeout {
                    match socket.connect_timeout(&addr.into(), timeout) {
                        Ok(_) => return Ok(socket.into()),
                        Err(err) => last_err = Some(err),
                    }
                } else {
                    match socket.connect(&addr.into()) {
                        Ok(_) => return Ok(socket.into()),
                        Err(err) => last_err = Some(err),
                    }
                }
            }

            Err(match last_err {
                Some(last_err) => error::connection(last_err),
                None => error::connection("could not resolve to any address"),
            })
        }

        let tcp_stream = try_connect(server, timeout, local_addr)?;
        let mut stream = NetworkStream::new(InnerNetworkStream::Tcp(tcp_stream));
        if let Some(tls_parameters) = tls_parameters {
            stream.upgrade_tls(tls_parameters)?;
        }
        Ok(stream)
    }

    pub fn upgrade_tls(&mut self, tls_parameters: &TlsParameters) -> Result<(), Error> {
        match &self.inner {
            #[cfg(not(any(feature = "native-tls", feature = "rustls-tls")))]
            InnerNetworkStream::Tcp(_) => {
                let _ = tls_parameters;
                panic!("Trying to upgrade an NetworkStream without having enabled either the native-tls or the rustls-tls feature");
            }

            #[cfg(any(feature = "native-tls", feature = "rustls-tls"))]
            InnerNetworkStream::Tcp(_) => {
                // get owned TcpStream
                let tcp_stream = mem::replace(&mut self.inner, InnerNetworkStream::None);
                let tcp_stream = match tcp_stream {
                    InnerNetworkStream::Tcp(tcp_stream) => tcp_stream,
                    _ => unreachable!(),
                };

                self.inner = Self::upgrade_tls_impl(tcp_stream, tls_parameters)?;
                Ok(())
            }
            _ => Ok(()),
        }
    }

    #[cfg(any(feature = "native-tls", feature = "rustls-tls"))]
    fn upgrade_tls_impl(
        tcp_stream: TcpStream,
        tls_parameters: &TlsParameters,
    ) -> Result<InnerNetworkStream, Error> {
        Ok(match &tls_parameters.connector {
            #[cfg(feature = "native-tls")]
            InnerTlsParameters::NativeTls(connector) => {
                let stream = connector
                    .connect(tls_parameters.domain(), tcp_stream)
                    .map_err(error::connection)?;
                InnerNetworkStream::NativeTls(stream)
            }
            #[cfg(feature = "rustls-tls")]
            InnerTlsParameters::RustlsTls(connector) => {
                let domain = ServerName::try_from(tls_parameters.domain())
                    .map_err(|_| error::connection("domain isn't a valid DNS name"))?;
                let connection =
                    ClientConnection::new(connector.clone(), domain).map_err(error::connection)?;
                let stream = StreamOwned::new(connection, tcp_stream);
                InnerNetworkStream::RustlsTls(stream)
            }
        })
    }

    pub fn is_encrypted(&self) -> bool {
        match self.inner {
            InnerNetworkStream::Tcp(_) => false,
            #[cfg(feature = "native-tls")]
            InnerNetworkStream::NativeTls(_) => true,
            #[cfg(feature = "rustls-tls")]
            InnerNetworkStream::RustlsTls(_) => true,
            InnerNetworkStream::None => {
                debug_assert!(false, "InnerNetworkStream::None must never be built");
                false
            }
        }
    }

    #[cfg(any(feature = "native-tls", feature = "rustls-tls"))]
    pub fn peer_certificate(&self) -> Result<Vec<u8>, Error> {
        match &self.inner {
            InnerNetworkStream::Tcp(_) => Err(error::client("Connection is not encrypted")),
            #[cfg(feature = "native-tls")]
            InnerNetworkStream::NativeTls(stream) => Ok(stream
                .peer_certificate()
                .map_err(error::tls)?
                .unwrap()
                .to_der()
                .map_err(error::tls)?),
            #[cfg(feature = "rustls-tls")]
            InnerNetworkStream::RustlsTls(stream) => Ok(stream
                .conn
                .peer_certificates()
                .unwrap()
                .first()
                .unwrap()
                .clone()
                .0),
            InnerNetworkStream::None => panic!("InnerNetworkStream::None must never be built"),
        }
    }

    pub fn set_read_timeout(&mut self, duration: Option<Duration>) -> io::Result<()> {
        match self.inner {
            InnerNetworkStream::Tcp(ref mut stream) => stream.set_read_timeout(duration),
            #[cfg(feature = "native-tls")]
            InnerNetworkStream::NativeTls(ref mut stream) => {
                stream.get_ref().set_read_timeout(duration)
            }
            #[cfg(feature = "rustls-tls")]
            InnerNetworkStream::RustlsTls(ref mut stream) => {
                stream.get_ref().set_read_timeout(duration)
            }
            InnerNetworkStream::None => {
                debug_assert!(false, "InnerNetworkStream::None must never be built");
                Ok(())
            }
        }
    }

    /// Set write timeout for IO calls
    pub fn set_write_timeout(&mut self, duration: Option<Duration>) -> io::Result<()> {
        match self.inner {
            InnerNetworkStream::Tcp(ref mut stream) => stream.set_write_timeout(duration),

            #[cfg(feature = "native-tls")]
            InnerNetworkStream::NativeTls(ref mut stream) => {
                stream.get_ref().set_write_timeout(duration)
            }
            #[cfg(feature = "rustls-tls")]
            InnerNetworkStream::RustlsTls(ref mut stream) => {
                stream.get_ref().set_write_timeout(duration)
            }

            InnerNetworkStream::None => {
                debug_assert!(false, "InnerNetworkStream::None must never be built");
                Ok(())
            }
        }
    }
}

impl Read for NetworkStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self.inner {
            InnerNetworkStream::Tcp(ref mut s) => s.read(buf),
            #[cfg(feature = "native-tls")]
            InnerNetworkStream::NativeTls(ref mut s) => s.read(buf),
            #[cfg(feature = "rustls-tls")]
            InnerNetworkStream::RustlsTls(ref mut s) => s.read(buf),
            InnerNetworkStream::None => {
                debug_assert!(false, "InnerNetworkStream::None must never be built");
                Ok(0)
            }
        }
    }
}

impl Write for NetworkStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self.inner {
            InnerNetworkStream::Tcp(ref mut s) => s.write(buf),
            #[cfg(feature = "native-tls")]
            InnerNetworkStream::NativeTls(ref mut s) => s.write(buf),
            #[cfg(feature = "rustls-tls")]
            InnerNetworkStream::RustlsTls(ref mut s) => s.write(buf),
            InnerNetworkStream::None => {
                debug_assert!(false, "InnerNetworkStream::None must never be built");
                Ok(0)
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self.inner {
            InnerNetworkStream::Tcp(ref mut s) => s.flush(),
            #[cfg(feature = "native-tls")]
            InnerNetworkStream::NativeTls(ref mut s) => s.flush(),
            #[cfg(feature = "rustls-tls")]
            InnerNetworkStream::RustlsTls(ref mut s) => s.flush(),
            InnerNetworkStream::None => {
                debug_assert!(false, "InnerNetworkStream::None must never be built");
                Ok(())
            }
        }
    }
}

/// If the local address is set, binds the socket to this address.
/// If local address is not set, then destination address is required to determine to the default
/// local address on some platforms.
/// See: https://github.com/hyperium/hyper/blob/faf24c6ad8eee1c3d5ccc9a4d4835717b8e2903f/src/client/connect/http.rs#L560
fn bind_local_address(
    socket: &socket2::Socket,
    dst_addr: &SocketAddr,
    local_addr: Option<IpAddr>,
) -> Result<(), Error> {
    match local_addr {
        Some(local_addr) => {
            socket
                .bind(&SocketAddr::new(local_addr, 0).into())
                .map_err(error::connection)?;
        }
        _ => {
            if cfg!(windows) {
                // Windows requires a socket be bound before calling connect
                let any: SocketAddr = match dst_addr {
                    SocketAddr::V4(_) => ([0, 0, 0, 0], 0).into(),
                    SocketAddr::V6(_) => ([0, 0, 0, 0, 0, 0, 0, 0], 0).into(),
                };
                socket.bind(&any.into()).map_err(error::connection)?;
            }
        }
    }
    Ok(())
}

/// When we have an iterator of resolved remote addresses, we must filter them to be the same
/// protocol as the local address binding. If no local address is set, then all will be matched.
pub(crate) fn resolved_address_filter(
    resolved_addr: &SocketAddr,
    local_addr: Option<IpAddr>,
) -> bool {
    match local_addr {
        Some(local_addr) => match resolved_addr.ip() {
            IpAddr::V4(_) => local_addr.is_ipv4(),
            IpAddr::V6(_) => local_addr.is_ipv6(),
        },
        None => true,
    }
}

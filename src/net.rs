//! A collection of traits abstracting over Listeners and Streams.
use std::any::{Any, AnyRefExt};
use std::boxed::BoxAny;
use std::fmt;
use std::intrinsics::TypeId;
use std::io::{IoResult, IoError, ConnectionAborted, InvalidInput, OtherIoError,
              Stream, Listener, Acceptor};
use std::io::net::ip::{SocketAddr, ToSocketAddr, Port};
use std::io::net::tcp::{TcpStream, TcpListener, TcpAcceptor};
use std::mem::{mod, transmute, transmute_copy};
use std::raw::{mod, TraitObject};
use std::sync::Arc;

use uany::UncheckedBoxAnyDowncast;
use openssl::ssl::{Ssl, SslStream, SslContext, VerifyCallback};
use openssl::ssl::SslVerifyMode::{SslVerifyPeer, SslVerifyNone};
use openssl::ssl::SslMethod::Sslv23;
use openssl::ssl::error::{SslError, StreamError, OpenSslErrors, SslSessionClosed};
use openssl::x509::X509FileType;

use self::HttpStream::{Http, Https};
use self::HttpListener::{HttpL, HttpsL};
use self::HttpAcceptor::{HttpA, HttpsA};

/// The write-status indicating headers have not been written.
#[allow(missing_copy_implementations)]
pub struct Fresh;

/// The write-status indicating headers have been written.
#[allow(missing_copy_implementations)]
pub struct Streaming;

/// An abstraction to listen for connections on a certain port.
pub trait NetworkListener<S: NetworkStream, A: NetworkAcceptor<S>>: Listener<S, A> {
    /// Bind to a socket.
    ///
    /// Note: This does not start listening for connections. You must call
    /// `listen()` to do that.
    fn bind<To: ToSocketAddr>(addr: To) -> IoResult<Self>;
    
    /// Bind to a socket with SSL. Otherwise behaves the same as bind().
    fn bind_with_ssl<To: ToSocketAddr>(addr: To, cert: Path, key: Path) -> IoResult<Self>;

    /// Get the address this Listener ended up listening on.
    fn socket_name(&mut self) -> IoResult<SocketAddr>;
}

/// An abstraction to receive `NetworkStream`s.
pub trait NetworkAcceptor<S: NetworkStream>: Acceptor<S> + Clone + Send {
    /// Closes the Acceptor, so no more incoming connections will be handled.
    fn close(&mut self) -> IoResult<()>;
}

/// An abstraction over streams that a Server can utilize.
pub trait NetworkStream: Stream + Any + StreamClone + Send {
    /// Get the remote address of the underlying connection.
    fn peer_name(&mut self) -> IoResult<SocketAddr>;
}

#[doc(hidden)]
pub trait StreamClone {
    fn clone_box(&self) -> Box<NetworkStream + Send>;
}

impl<T: NetworkStream + Send + Clone> StreamClone for T {
    #[inline]
    fn clone_box(&self) -> Box<NetworkStream + Send> {
        box self.clone()
    }
}

/// A connector creates a NetworkStream.
pub trait NetworkConnector<S: NetworkStream> {
    /// Connect to a remote address.
    fn connect(&mut self, host: &str, port: Port, scheme: &str) -> IoResult<S>;
}

impl fmt::Show for Box<NetworkStream + Send> {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.pad("Box<NetworkStream>")
    }
}

impl Clone for Box<NetworkStream + Send> {
    #[inline]
    fn clone(&self) -> Box<NetworkStream + Send> { self.clone_box() }
}

impl Reader for Box<NetworkStream + Send> {
    #[inline]
    fn read(&mut self, buf: &mut [u8]) -> IoResult<uint> { (**self).read(buf) }
}

impl Writer for Box<NetworkStream + Send> {
    #[inline]
    fn write(&mut self, msg: &[u8]) -> IoResult<()> { (**self).write(msg) }

    #[inline]
    fn flush(&mut self) -> IoResult<()> { (**self).flush() }
}

impl<'a> Reader for &'a mut NetworkStream {
    #[inline]
    fn read(&mut self, buf: &mut [u8]) -> IoResult<uint> { (**self).read(buf) }
}

impl<'a> Writer for &'a mut NetworkStream {
    #[inline]
    fn write(&mut self, msg: &[u8]) -> IoResult<()> { (**self).write(msg) }

    #[inline]
    fn flush(&mut self) -> IoResult<()> { (**self).flush() }
}

impl UncheckedBoxAnyDowncast for Box<NetworkStream + Send> {
    unsafe fn downcast_unchecked<T: 'static>(self) -> Box<T>  {
        let to = *mem::transmute::<&Box<NetworkStream + Send>, &raw::TraitObject>(&self);
        // Prevent double-free.
        mem::forget(self);
        mem::transmute(to.data)
    }
}

impl<'a> AnyRefExt<'a> for &'a (NetworkStream + 'static) {
    #[inline]
    fn is<T: 'static>(self) -> bool {
        self.get_type_id() == TypeId::of::<T>()
    }

    #[inline]
    fn downcast_ref<T: 'static>(self) -> Option<&'a T> {
        if self.is::<T>() {
            unsafe {
                // Get the raw representation of the trait object
                let to: TraitObject = transmute_copy(&self);
                // Extract the data pointer
                Some(transmute(to.data))
            }
        } else {
            None
        }
    }
}

impl BoxAny for Box<NetworkStream + Send> {
    fn downcast<T: 'static>(self) -> Result<Box<T>, Box<NetworkStream + Send>> {
        if self.is::<T>() {
            Ok(unsafe { self.downcast_unchecked() })
        } else {
            Err(self)
        }
    }
}

/// A `NetworkListener` for `HttpStream`s.
pub enum HttpListener {
    /// A listener for HTTP protocol over a TCP connection.
    HttpL(TcpListener),
    /// A listener for HTTP protocol over a TCP connection, protected by TLS/SSL.
    HttpsL(TcpListener, SslContext)
}

impl Listener<HttpStream, HttpAcceptor> for HttpListener {
    #[inline]
    fn listen(self) -> IoResult<HttpAcceptor> {
        match self {
            HttpL(inner) => Ok(HttpA(try!(inner.listen()))),
            HttpsL(inner, ssl_context) => 
                Ok(HttpsA(try!(inner.listen()), Arc::<SslContext>::new(ssl_context)))
        }
    }
}

impl NetworkListener<HttpStream, HttpAcceptor> for HttpListener {
    #[inline]
    fn bind<To: ToSocketAddr>(addr: To) -> IoResult<HttpListener> {
        Ok(HttpL(try!(TcpListener::bind(addr))))
    }

    #[inline]
    fn bind_with_ssl<To: ToSocketAddr>(addr: To, cert: Path, key: Path) -> IoResult<HttpListener> {
        // TODO: Make these more configurable
        let mut ssl_context = try!(SslContext::new(Sslv23).map_err(lift_ssl_error));
        if let Some(err) = ssl_context.set_cipher_list("DEFAULT") {
            return Err(lift_ssl_error(err));
        }
        if let Some(err) = ssl_context.set_certificate_file(&cert, X509FileType::PEM) {
            return Err(lift_ssl_error(err));
        }
        if let Some(err) = ssl_context.set_private_key_file(&key, X509FileType::PEM) {
            return Err(lift_ssl_error(err));
        }
        ssl_context.set_verify(SslVerifyNone, None);
        Ok(HttpsL(try!(TcpListener::bind(addr)), ssl_context))
    }

    #[inline]
    fn socket_name(&mut self) -> IoResult<SocketAddr> {
        match *self {
            HttpL(ref mut inner) => inner.socket_name(),
            HttpsL(ref mut inner, _) => inner.socket_name()
        }
    }
}

/// A `NetworkAcceptor` for `HttpStream`s.
#[deriving(Clone)]
pub enum HttpAcceptor {
    /// An acceptor for HTTP protocol over TCP.
    HttpA(TcpAcceptor),
    /// An acceptor for HTTP protocol over TCP protected by TLS/SSL.
    HttpsA(TcpAcceptor, Arc<SslContext>)
}

impl Acceptor<HttpStream> for HttpAcceptor {
    #[inline]
    fn accept(&mut self) -> IoResult<HttpStream> {
        match *self {
            HttpA(ref mut inner) => Ok(Http(try!(inner.accept()))),
            HttpsA(ref mut inner, ref ssl_context) => {
                let stream = try!(inner.accept());
                let ssl_stream = try!(SslStream::<TcpStream>::new_server(&**ssl_context, stream).
                                     map_err(lift_ssl_error));
                Ok(Https(ssl_stream))
            }
        }
    }
}

impl NetworkAcceptor<HttpStream> for HttpAcceptor {
    #[inline]
    fn close(&mut self) -> IoResult<()> {
        match *self {
            HttpA(ref mut inner) => inner.close_accept(),
            HttpsA(ref mut inner, _) => inner.close_accept()
        }
    }
}

/// A wrapper around a TcpStream.
#[deriving(Clone)]
pub enum HttpStream {
    /// A stream over the HTTP protocol.
    Http(TcpStream),
    /// A stream over the HTTP protocol, protected by SSL.
    Https(SslStream<TcpStream>),
}

impl Reader for HttpStream {
    #[inline]
    fn read(&mut self, buf: &mut [u8]) -> IoResult<uint> {
        match *self {
            Http(ref mut inner) => inner.read(buf),
            Https(ref mut inner) => inner.read(buf)
        }
    }
}

impl Writer for HttpStream {
    #[inline]
    fn write(&mut self, msg: &[u8]) -> IoResult<()> {
        match *self {
            Http(ref mut inner) => inner.write(msg),
            Https(ref mut inner) => inner.write(msg)
        }
    }
    #[inline]
    fn flush(&mut self) -> IoResult<()> {
        match *self {
            Http(ref mut inner) => inner.flush(),
            Https(ref mut inner) => inner.flush(),
        }
    }
}

impl NetworkStream for HttpStream {
    fn peer_name(&mut self) -> IoResult<SocketAddr> {
        match *self {
            Http(ref mut inner) => inner.peer_name(),
            Https(ref mut inner) => inner.get_mut().peer_name()
        }
    }
}

/// A connector that will produce HttpStreams.
#[allow(missing_copy_implementations)]
pub struct HttpConnector(pub Option<VerifyCallback>);

impl NetworkConnector<HttpStream> for HttpConnector {
    fn connect(&mut self, host: &str, port: Port, scheme: &str) -> IoResult<HttpStream> {
        let addr = (host, port);
        match scheme {
            "http" => {
                debug!("http scheme");
                Ok(Http(try!(TcpStream::connect(addr))))
            },
            "https" => {
                debug!("https scheme");
                let stream = try!(TcpStream::connect(addr));
                let mut context = try!(SslContext::new(Sslv23).map_err(lift_ssl_error));
                self.0.as_ref().map(|cb| context.set_verify(SslVerifyPeer, Some(*cb)));
                let ssl = try!(Ssl::new(&context).map_err(lift_ssl_error));
                try!(ssl.set_hostname(host).map_err(lift_ssl_error));
                let stream = try!(SslStream::new(&context, stream).map_err(lift_ssl_error));
                Ok(Https(stream))
            },
            _ => {
                Err(IoError {
                    kind: InvalidInput,
                    desc: "Invalid scheme for Http",
                    detail: None
                })
            }
        }
    }
}

fn lift_ssl_error(ssl: SslError) -> IoError {
    debug!("lift_ssl_error: {}", ssl);
    match ssl {
        StreamError(err) => err,
        SslSessionClosed => IoError {
            kind: ConnectionAborted,
            desc: "SSL Connection Closed",
            detail: None
        },
        // Unfortunately throw this away. No way to support this
        // detail without a better Error abstraction.
        OpenSslErrors(errs) => IoError {
            kind: OtherIoError,
            desc: "Error in OpenSSL",
            detail: Some(format!("{}", errs))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::boxed::BoxAny;
    use uany::UncheckedBoxAnyDowncast;

    use mock::MockStream;
    use super::NetworkStream;

    #[test]
    fn test_downcast_box_stream() {
        let stream = box MockStream::new() as Box<NetworkStream + Send>;

        let mock = stream.downcast::<MockStream>().unwrap();
        assert_eq!(mock, box MockStream::new());

    }

    #[test]
    fn test_downcast_unchecked_box_stream() {
        let stream = box MockStream::new() as Box<NetworkStream + Send>;

        let mock = unsafe { stream.downcast_unchecked::<MockStream>() };
        assert_eq!(mock, box MockStream::new());

    }

}

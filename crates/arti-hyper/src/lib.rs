//! High-level layer for making http(s) requests the Tor network as a client.

#![deny(missing_docs)]
#![warn(noop_method_call)]
#![deny(unreachable_pub)]
#![warn(clippy::all)]
#![deny(clippy::await_holding_lock)]
#![deny(clippy::cargo_common_metadata)]
#![deny(clippy::cast_lossless)]
#![deny(clippy::checked_conversions)]
#![warn(clippy::cognitive_complexity)]
#![deny(clippy::debug_assert_with_mut_call)]
#![deny(clippy::exhaustive_enums)]
#![deny(clippy::exhaustive_structs)]
#![deny(clippy::expl_impl_clone_on_copy)]
#![deny(clippy::fallible_impl_from)]
#![deny(clippy::implicit_clone)]
#![deny(clippy::large_stack_arrays)]
#![warn(clippy::manual_ok_or)]
#![deny(clippy::missing_docs_in_private_items)]
#![deny(clippy::missing_panics_doc)]
#![warn(clippy::needless_borrow)]
#![warn(clippy::needless_pass_by_value)]
#![warn(clippy::option_option)]
#![warn(clippy::rc_buffer)]
#![deny(clippy::ref_option_ref)]
#![warn(clippy::semicolon_if_nothing_returned)]
#![warn(clippy::trait_duplication_in_bounds)]
#![deny(clippy::unnecessary_wraps)]
#![warn(clippy::unseparated_literal_suffix)]
#![deny(clippy::unwrap_used)]

use std::future::Future;
use std::io::Error;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use arti_client::{DataStream, IntoTorAddr, TorClient};
use hyper::client::connect::{Connected, Connection};
use hyper::http::uri::Scheme;
use hyper::http::Uri;
use hyper::service::Service;
use pin_project::pin_project;
use thiserror::Error;
use tls_api::TlsConnector as TlsConn; // This is different from tor_rtompat::TlsConnector
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tor_rtcompat::Runtime;

/// Error making or using http connection
///
/// This error ends up being passed to hyper and bundled up into a [`hyper::Error`]
#[derive(Error, Clone, Debug)]
#[non_exhaustive]
pub enum ConnectionError {
    /// Unsupported URI scheme
    #[error("unsupported URI scheme in {uri:?}")]
    UnsupportedUriScheme {
        /// URI
        uri: Uri,
    },

    /// Missing hostname
    #[error("Missing hostname in {uri:?}")]
    MissingHostname {
        /// URI
        uri: Uri,
    },

    /// Tor connection failed
    #[error("Tor connection failed")]
    Arti(#[from] arti_client::Error),

    /// TLS connection failed
    #[error("TLS connection failed")]
    TLS(#[source] Arc<anyhow::Error>),
}

/// We implement this for form's sake
impl tor_error::HasKind for ConnectionError {
    #[rustfmt::skip]
    fn kind(&self) -> tor_error::ErrorKind {
        use ConnectionError as CE;
        use tor_error::ErrorKind as EK;
        match self {
            CE::UnsupportedUriScheme{..} => EK::NotImplemented,
            CE::MissingHostname{..}      => EK::BadApiUsage,
            CE::Arti(e)                  => e.kind(),
            CE::TLS(_)                   => EK::RemoteProtocolFailed,
        }
    }
}

/// A `hyper` connector to proxy HTTP connections via the Tor network, using Arti.
///
/// Only supports plaintext HTTP for now.
///
/// TC is the TLS to used *across* Tor to connect to the origin server.
/// This is a different Rust type to the TLS used *by* Tor to connect to relays etc.
/// It might even be a different underlying TLS implementation
/// (although that is usually not a particularly good idea).
pub struct ArtiHttpConnector<R: Runtime, TC: TlsConn> {
    /// The client
    client: TorClient<R>,

    /// TLS for using across Tor.
    tls_conn: Arc<TC>,
}

// #[derive(Clone)] infers a TC: Clone bound
impl<R: Runtime, TC: TlsConn> Clone for ArtiHttpConnector<R, TC> {
    fn clone(&self) -> Self {
        let client = self.client.clone();
        let tls_conn = self.tls_conn.clone();
        Self { client, tls_conn }
    }
}

impl<R: Runtime, TC: TlsConn> ArtiHttpConnector<R, TC> {
    /// Make a new `ArtiHttpConnector` using an Arti `TorClient` object.
    pub fn new(client: TorClient<R>, tls_conn: TC) -> Self {
        let tls_conn = tls_conn.into();
        Self { client, tls_conn }
    }
}

/// Wrapper type that makes an Arti `DataStream` implement necessary traits to be used as
/// a `hyper` connection object (mainly `Connection`).
#[pin_project]
pub struct ArtiHttpConnection<TC: TlsConn> {
    /// The stream
    #[pin]
    inner: MaybeHttpsStream<TC>,
}

/// The actual actual stream; might be TLS, might not
#[pin_project(project = MaybeHttpsStreamProj)]
enum MaybeHttpsStream<TC: TlsConn> {
    /// http
    Http(Pin<Box<DataStream>>), // Tc:TlsStream is generally boxed; box this one too

    /// https
    Https(#[pin] TC::TlsStream),
}

impl<TC: TlsConn> Connection for ArtiHttpConnection<TC> {
    fn connected(&self) -> Connected {
        Connected::new()
    }
}

// These trait implementations just defer to the inner `DataStream`; the wrapper type is just
// there to implement the `Connection` trait.
impl<TC: TlsConn> AsyncRead for ArtiHttpConnection<TC> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        match self.project().inner.project() {
            MaybeHttpsStreamProj::Http(ds) => ds.as_mut().poll_read(cx, buf),
            MaybeHttpsStreamProj::Https(t) => t.poll_read(cx, buf),
        }
    }
}

impl<TC: TlsConn> AsyncWrite for ArtiHttpConnection<TC> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, Error>> {
        match self.project().inner.project() {
            MaybeHttpsStreamProj::Http(ds) => ds.as_mut().poll_write(cx, buf),
            MaybeHttpsStreamProj::Https(t) => t.poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        match self.project().inner.project() {
            MaybeHttpsStreamProj::Http(ds) => ds.as_mut().poll_flush(cx),
            MaybeHttpsStreamProj::Https(t) => t.poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        match self.project().inner.project() {
            MaybeHttpsStreamProj::Http(ds) => ds.as_mut().poll_shutdown(cx),
            MaybeHttpsStreamProj::Https(t) => t.poll_shutdown(cx),
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
/// Are we doing TLS?
enum UseTls {
    Bare,
    Tls,
}

/// Convert uri to http[s] host and port, and whether to do tls
fn uri_to_host_port_tls(uri: Uri) -> Result<(String, u16, UseTls), ConnectionError> {
    let use_tls = {
        // Scheme doesn't derive PartialEq so can't be matched on
        let scheme = uri.scheme();
        if scheme == Some(&Scheme::HTTP) {
            UseTls::Bare
        } else if scheme == Some(&Scheme::HTTPS) {
            UseTls::Tls
        } else {
            return Err(ConnectionError::UnsupportedUriScheme { uri });
        }
    };
    let host = match uri.host() {
        Some(h) => h,
        _ => return Err(ConnectionError::MissingHostname { uri }),
    };
    let port = uri.port().map(|x| x.as_u16()).unwrap_or(match use_tls {
        UseTls::Tls => 443,
        UseTls::Bare => 80,
    });

    Ok((host.to_owned(), port, use_tls))
}

impl<R: Runtime, TC: TlsConn> Service<Uri> for ArtiHttpConnector<R, TC> {
    type Response = ArtiHttpConnection<TC>;
    type Error = ConnectionError;
    #[allow(clippy::type_complexity)]
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Uri) -> Self::Future {
        // `TorClient` objects can be cloned cheaply (the cloned objects refer to the same
        // underlying handles required to make Tor connections internally).
        // We use this to avoid the returned future having to borrow `self`.
        let client = self.client.clone();
        let tls_conn = self.tls_conn.clone();
        Box::pin(async move {
            // Extract the host and port to connect to from the URI.
            let (host, port, use_tls) = uri_to_host_port_tls(req)?;
            // Initiate a new Tor connection, producing a `DataStream` if successful.
            let addr = (&host as &str, port)
                .into_tor_addr()
                .map_err(arti_client::Error::from)?;
            let ds = client.connect(addr).await?;

            let inner = match use_tls {
                UseTls::Tls => {
                    let conn = tls_conn
                        .connect_impl_tls_stream(&host, ds)
                        .await
                        .map_err(|e| ConnectionError::TLS(e.into()))?;
                    MaybeHttpsStream::Https(conn)
                }
                UseTls::Bare => MaybeHttpsStream::Http(Box::new(ds).into()),
            };

            Ok(ArtiHttpConnection { inner })
        })
    }
}

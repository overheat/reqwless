/// Client using embedded-nal-async traits to establish connections and perform HTTP requests.
///
use crate::headers::ContentType;
use crate::request::*;
use crate::response::*;
use crate::Error;
use buffered_io::asynch::BufferedWrite;
use embedded_io::Error as _;
use embedded_io::ErrorType;
use embedded_io_async::{Read, Write};
use embedded_nal_async::{Dns, SocketAddr, TcpConnect};
use nourl::{Url, UrlScheme};

/// An async HTTP client that can establish a TCP connection and perform
/// HTTP requests.
pub struct HttpClient<'a, T, D>
where
    T: TcpConnect + 'a,
    D: Dns + 'a,
{
    client: &'a T,
    dns: &'a D,
    #[cfg(feature = "embedded-tls")]
    tls: Option<TlsConfig<'a>>,
}

/// Type for TLS configuration of HTTP client.
#[cfg(feature = "embedded-tls")]
pub struct TlsConfig<'a> {
    seed: u64,
    read_buffer: &'a mut [u8],
    write_buffer: &'a mut [u8],
    verify: TlsVerify<'a>,
}

/// Supported verification modes.
#[cfg(feature = "embedded-tls")]
pub enum TlsVerify<'a> {
    /// No verification of the remote host
    None,
    /// Use pre-shared keys for verifying
    Psk { identity: &'a [u8], psk: &'a [u8] },
}

#[cfg(feature = "embedded-tls")]
impl<'a> TlsConfig<'a> {
    pub fn new(seed: u64, read_buffer: &'a mut [u8], write_buffer: &'a mut [u8], verify: TlsVerify<'a>) -> Self {
        Self {
            seed,
            write_buffer,
            read_buffer,
            verify,
        }
    }
}

impl<'a, T, D> HttpClient<'a, T, D>
where
    T: TcpConnect + 'a,
    D: Dns + 'a,
{
    /// Create a new HTTP client for a given connection handle and a target host.
    pub fn new(client: &'a T, dns: &'a D) -> Self {
        Self {
            client,
            dns,
            #[cfg(feature = "embedded-tls")]
            tls: None,
        }
    }

    /// Create a new HTTP client for a given connection handle and a target host.
    #[cfg(feature = "embedded-tls")]
    pub fn new_with_tls(client: &'a T, dns: &'a D, tls: TlsConfig<'a>) -> Self {
        Self {
            client,
            dns,
            tls: Some(tls),
        }
    }

    async fn connect<'m>(&'m mut self, url: &Url<'m>) -> Result<HttpConnection<'m, T::Connection<'m>>, Error> {
        let host = url.host();
        let port = url.port_or_default();

        let remote = self
            .dns
            .get_host_by_name(host, embedded_nal_async::AddrType::Either)
            .await
            .map_err(|_| Error::Dns)?;

        let conn = self
            .client
            .connect(SocketAddr::new(remote, port))
            .await
            .map_err(|e| e.kind())?;

        if url.scheme() == UrlScheme::HTTPS {
            #[cfg(feature = "embedded-tls")]
            if let Some(tls) = self.tls.as_mut() {
                use embedded_tls::{TlsConfig, TlsContext};
                use rand_chacha::ChaCha8Rng;
                use rand_core::{RngCore, SeedableRng};
                let mut rng = ChaCha8Rng::seed_from_u64(tls.seed);
                tls.seed = rng.next_u64();
                let mut config = TlsConfig::new().with_server_name(url.host());
                if let TlsVerify::Psk { identity, psk } = tls.verify {
                    config = config.with_psk(psk, &[identity]);
                }
                let mut conn: embedded_tls::TlsConnection<'m, T::Connection<'m>, embedded_tls::Aes128GcmSha256> =
                    embedded_tls::TlsConnection::new(conn, tls.read_buffer, tls.write_buffer);
                conn.open::<_, embedded_tls::NoVerify>(TlsContext::new(&config, &mut rng))
                    .await?;
                Ok(HttpConnection::Tls(conn))
            } else {
                Ok(HttpConnection::Plain(conn))
            }
            #[cfg(not(feature = "embedded-tls"))]
            Err(Error::InvalidUrl(nourl::Error::UnsupportedScheme))
        } else {
            #[cfg(feature = "embedded-tls")]
            match self.tls.as_mut() {
                Some(tls) => Ok(HttpConnection::PlainBuffered(BufferedWrite::new(
                    buffered_io_adapter::ConnErrorAdapter(conn),
                    tls.write_buffer,
                ))),
                None => Ok(HttpConnection::Plain(conn)),
            }
            #[cfg(not(feature = "embedded-tls"))]
            Ok(HttpConnection::Plain(conn))
        }
    }

    /// Create a single http request.
    pub async fn request<'m>(
        &'m mut self,
        method: Method,
        url: &'m str,
    ) -> Result<HttpRequestHandle<'m, T::Connection<'m>, ()>, Error> {
        let url = Url::parse(url)?;
        let conn = self.connect(&url).await?;
        Ok(HttpRequestHandle {
            conn,
            request: Some(Request::new(method, url.path()).host(url.host())),
        })
    }

    /// Create a connection to a server with the provided `resource_url`.
    /// The path in the url is considered the base path for subsequent requests.
    pub async fn resource<'res>(
        &'res mut self,
        resource_url: &'res str,
    ) -> Result<HttpResource<'res, T::Connection<'res>>, Error> {
        let resource_url = Url::parse(resource_url)?;
        let conn = self.connect(&resource_url).await?;
        Ok(HttpResource {
            conn,
            host: resource_url.host(),
            base_path: resource_url.path(),
        })
    }
}

/// Represents a HTTP connection that may be encrypted or unencrypted.
#[allow(clippy::large_enum_variant)]
pub enum HttpConnection<'m, C>
where
    C: Read + Write,
{
    Plain(C),
    #[cfg(feature = "embedded-tls")]
    PlainBuffered(BufferedWrite<'m, buffered_io_adapter::ConnErrorAdapter<C>>),
    #[cfg(feature = "embedded-tls")]
    Tls(embedded_tls::TlsConnection<'m, C, embedded_tls::Aes128GcmSha256>),
    #[cfg(not(feature = "embedded-tls"))]
    Tls((&'m mut (), core::convert::Infallible)), // Variant is impossible to create, but we need it to avoid "unused lifetime" warning
}

impl<'conn, T> HttpConnection<'conn, T>
where
    T: Read + Write,
{
    /// Turn the request into a buffered request.
    ///
    /// This is only relevant if no TLS is used, as `embedded-tls` buffers internally and we reuse
    /// its buffer for non-TLS connections.
    pub fn into_buffered<'buf>(self, tx_buf: &'buf mut [u8]) -> HttpConnection<'buf, T>
    where
        'conn: 'buf,
    {
        match self {
            HttpConnection::Plain(conn) => {
                HttpConnection::PlainBuffered(BufferedWrite::new(buffered_io_adapter::ConnErrorAdapter(conn), tx_buf))
            }
            HttpConnection::PlainBuffered(conn) => HttpConnection::PlainBuffered(conn),
            HttpConnection::Tls(tls) => HttpConnection::Tls(tls),
        }
    }

    /// Send a request on an established connection.
    ///
    /// The request is sent in its raw form without any base path from the resource.
    /// The response headers are stored in the provided rx_buf, which should be sized to contain at least the response headers.
    ///
    /// The response is returned.
    pub async fn send<'buf, B: RequestBody>(
        &'conn mut self,
        request: Request<'conn, B>,
        rx_buf: &'buf mut [u8],
    ) -> Result<Response<'buf, 'conn, HttpConnection<'conn, T>>, Error> {
        request.write(self).await?;
        Response::read(self, request.method, rx_buf).await
    }
}

impl<T> ErrorType for HttpConnection<'_, T>
where
    T: Read + Write,
{
    type Error = embedded_io::ErrorKind;
}

impl<T> Read for HttpConnection<'_, T>
where
    T: Read + Write,
{
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        match self {
            Self::Plain(conn) => conn.read(buf).await.map_err(|e| e.kind()),
            #[cfg(feature = "embedded-tls")]
            Self::PlainBuffered(conn) => conn.read(buf).await.map_err(|e| e.kind()),
            #[cfg(feature = "embedded-tls")]
            Self::Tls(conn) => conn.read(buf).await.map_err(|e| e.kind()),
            #[cfg(not(feature = "embedded-tls"))]
            _ => unreachable!(),
        }
    }
}

impl<T> Write for HttpConnection<'_, T>
where
    T: Read + Write,
{
    async fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
        match self {
            Self::Plain(conn) => conn.write(buf).await.map_err(|e| e.kind()),
            #[cfg(feature = "embedded-tls")]
            Self::PlainBuffered(conn) => conn.write(buf).await.map_err(|e| e.kind()),
            #[cfg(feature = "embedded-tls")]
            Self::Tls(conn) => conn.write(buf).await.map_err(|e| e.kind()),
            #[cfg(not(feature = "embedded-tls"))]
            _ => unreachable!(),
        }
    }

    async fn flush(&mut self) -> Result<(), Self::Error> {
        match self {
            Self::Plain(conn) => conn.flush().await.map_err(|e| e.kind()),
            #[cfg(feature = "embedded-tls")]
            Self::PlainBuffered(conn) => conn.flush().await.map_err(|e| e.kind()),
            #[cfg(feature = "embedded-tls")]
            Self::Tls(conn) => conn.flush().await.map_err(|e| e.kind()),
            #[cfg(not(feature = "embedded-tls"))]
            _ => unreachable!(),
        }
    }
}

/// A HTTP request handle
///
/// The underlying connection is closed when drop'ed.
pub struct HttpRequestHandle<'m, C, B>
where
    C: Read + Write,
    B: RequestBody,
{
    pub conn: HttpConnection<'m, C>,
    request: Option<DefaultRequestBuilder<'m, B>>,
}

impl<'m, C, B> HttpRequestHandle<'m, C, B>
where
    C: Read + Write,
    B: RequestBody,
{
    /// Turn the request into a buffered request.
    ///
    /// This is only relevant if no TLS is used, as `embedded-tls` buffers internally and we reuse
    /// its buffer for non-TLS connections.
    pub fn into_buffered<'buf>(self, tx_buf: &'buf mut [u8]) -> HttpRequestHandle<'buf, C, B>
    where
        'm: 'buf,
    {
        HttpRequestHandle {
            conn: self.conn.into_buffered(tx_buf),
            request: self.request,
        }
    }

    /// Send the request.
    ///
    /// The response headers are stored in the provided rx_buf, which should be sized to contain at least the response headers.
    ///
    /// The response is returned.
    pub async fn send<'buf, 'conn>(
        &'conn mut self,
        rx_buf: &'buf mut [u8],
    ) -> Result<Response<'buf, 'conn, HttpConnection<'conn, C>>, Error>
    where
        'conn: 'm,
    {
        let request = self.request.take().ok_or(Error::AlreadySent)?.build();
        request.write(&mut self.conn).await?;
        Response::read(&mut self.conn, request.method, rx_buf).await
    }
}

impl<'m, C, B> RequestBuilder<'m, B> for HttpRequestHandle<'m, C, B>
where
    C: Read + Write,
    B: RequestBody,
{
    type WithBody<T: RequestBody> = HttpRequestHandle<'m, C, T>;

    fn headers(mut self, headers: &'m [(&'m str, &'m str)]) -> Self {
        self.request = Some(self.request.unwrap().headers(headers));
        self
    }

    fn path(mut self, path: &'m str) -> Self {
        self.request = Some(self.request.unwrap().path(path));
        self
    }

    fn body<T: RequestBody>(self, body: T) -> Self::WithBody<T> {
        HttpRequestHandle {
            conn: self.conn,
            request: Some(self.request.unwrap().body(body)),
        }
    }

    fn host(mut self, host: &'m str) -> Self {
        self.request = Some(self.request.unwrap().host(host));
        self
    }

    fn content_type(mut self, content_type: ContentType) -> Self {
        self.request = Some(self.request.unwrap().content_type(content_type));
        self
    }

    fn basic_auth(mut self, username: &'m str, password: &'m str) -> Self {
        self.request = Some(self.request.unwrap().basic_auth(username, password));
        self
    }

    fn build(self) -> Request<'m, B> {
        self.request.unwrap().build()
    }
}

/// A HTTP resource describing a scoped endpoint
///
/// The underlying connection is closed when drop'ed.
pub struct HttpResource<'res, C>
where
    C: Read + Write,
{
    pub conn: HttpConnection<'res, C>,
    pub host: &'res str,
    pub base_path: &'res str,
}

impl<'res, C> HttpResource<'res, C>
where
    C: Read + Write,
{
    /// Turn the resource into a buffered resource
    ///
    /// This is only relevant if no TLS is used, as `embedded-tls` buffers internally and we reuse
    /// its buffer for non-TLS connections.
    pub fn into_buffered<'buf>(self, tx_buf: &'buf mut [u8]) -> HttpResource<'buf, C>
    where
        'res: 'buf,
    {
        HttpResource {
            conn: self.conn.into_buffered(tx_buf),
            host: self.host,
            base_path: self.base_path,
        }
    }

    pub fn request<'conn, 'm>(
        &'conn mut self,
        method: Method,
        path: &'m str,
    ) -> HttpResourceRequestBuilder<'conn, 'res, 'm, C, ()>
    where
        'res: 'm,
    {
        HttpResourceRequestBuilder {
            conn: &mut self.conn,
            request: Request::new(method, path).host(self.host),
            base_path: self.base_path,
        }
    }

    /// Create a new scoped GET http request.
    pub fn get<'conn, 'm>(&'conn mut self, path: &'m str) -> HttpResourceRequestBuilder<'conn, 'res, 'm, C, ()>
    where
        'res: 'm,
    {
        self.request(Method::GET, path)
    }

    /// Create a new scoped POST http request.
    pub fn post<'conn, 'm>(&'conn mut self, path: &'m str) -> HttpResourceRequestBuilder<'conn, 'res, 'm, C, ()>
    where
        'res: 'm,
    {
        self.request(Method::POST, path)
    }

    /// Create a new scoped PUT http request.
    pub fn put<'conn, 'm>(&'conn mut self, path: &'m str) -> HttpResourceRequestBuilder<'conn, 'res, 'm, C, ()>
    where
        'res: 'm,
    {
        self.request(Method::PUT, path)
    }

    /// Create a new scoped DELETE http request.
    pub fn delete<'conn, 'm>(&'conn mut self, path: &'m str) -> HttpResourceRequestBuilder<'conn, 'res, 'm, C, ()>
    where
        'res: 'm,
    {
        self.request(Method::DELETE, path)
    }

    /// Create a new scoped HEAD http request.
    pub fn head<'conn, 'm>(&'conn mut self, path: &'m str) -> HttpResourceRequestBuilder<'conn, 'res, 'm, C, ()>
    where
        'res: 'm,
    {
        self.request(Method::HEAD, path)
    }

    /// Send a request to a resource.
    ///
    /// The base path of the resource is prepended to the request path.
    /// The response headers are stored in the provided rx_buf, which should be sized to contain at least the response headers.
    ///
    /// The response is returned.
    pub async fn send<'buf, 'conn, B: RequestBody>(
        &'conn mut self,
        mut request: Request<'res, B>,
        rx_buf: &'buf mut [u8],
    ) -> Result<Response<'buf, 'conn, HttpConnection<'res, C>>, Error> {
        request.base_path = Some(self.base_path);
        request.write(&mut self.conn).await?;
        Response::read(&mut self.conn, request.method, rx_buf).await
    }
}

pub struct HttpResourceRequestBuilder<'conn, 'res, 'm, C, B>
where
    C: Read + Write,
    B: RequestBody,
{
    conn: &'conn mut HttpConnection<'res, C>,
    base_path: &'res str,
    request: DefaultRequestBuilder<'m, B>,
}

impl<'conn, 'res, 'm, C, B> HttpResourceRequestBuilder<'conn, 'res, 'm, C, B>
where
    C: Read + Write,
    B: RequestBody,
{
    /// Send the request.
    ///
    /// The base path of the resource is prepended to the request path.
    /// The response headers are stored in the provided rx_buf, which should be sized to contain at least the response headers.
    ///
    /// The response is returned.
    pub async fn send<'buf>(
        self,
        rx_buf: &'buf mut [u8],
    ) -> Result<Response<'buf, 'conn, HttpConnection<'res, C>>, Error> {
        let conn = self.conn;
        let mut request = self.request.build();
        request.base_path = Some(self.base_path);
        request.write(conn).await?;
        Response::read(conn, request.method, rx_buf).await
    }
}

impl<'conn, 'res, 'm, C, B> RequestBuilder<'m, B> for HttpResourceRequestBuilder<'conn, 'res, 'm, C, B>
where
    C: Read + Write,
    B: RequestBody,
{
    type WithBody<T: RequestBody> = HttpResourceRequestBuilder<'conn, 'res, 'm, C, T>;

    fn headers(mut self, headers: &'m [(&'m str, &'m str)]) -> Self {
        self.request = self.request.headers(headers);
        self
    }

    fn path(mut self, path: &'m str) -> Self {
        self.request = self.request.path(path);
        self
    }

    fn body<T: RequestBody>(self, body: T) -> Self::WithBody<T> {
        HttpResourceRequestBuilder {
            conn: self.conn,
            base_path: self.base_path,
            request: self.request.body(body),
        }
    }

    fn host(mut self, host: &'m str) -> Self {
        self.request = self.request.host(host);
        self
    }

    fn content_type(mut self, content_type: ContentType) -> Self {
        self.request = self.request.content_type(content_type);
        self
    }

    fn basic_auth(mut self, username: &'m str, password: &'m str) -> Self {
        self.request = self.request.basic_auth(username, password);
        self
    }

    fn build(self) -> Request<'m, B> {
        self.request.build()
    }
}

mod buffered_io_adapter {
    use embedded_io::{Error as _, ErrorKind, ErrorType, ReadExactError};
    use embedded_io_async::{Read, Write};

    pub struct Error(embedded_io::ErrorKind);

    impl core::fmt::Debug for Error {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            self.0.fmt(f)
        }
    }

    impl embedded_io_async::Error for Error {
        fn kind(&self) -> embedded_io::ErrorKind {
            self.0
        }
    }

    pub struct ConnErrorAdapter<C>(pub C);

    impl<C> ErrorType for ConnErrorAdapter<C> {
        type Error = ErrorKind;
    }

    impl<C> Write for ConnErrorAdapter<C>
    where
        C: Write,
    {
        async fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
            self.0.write(buf).await.map_err(|e| e.kind())
        }

        async fn flush(&mut self) -> Result<(), Self::Error> {
            self.0.flush().await.map_err(|e| e.kind())
        }

        async fn write_all(&mut self, buf: &[u8]) -> Result<(), Self::Error> {
            self.0.write_all(buf).await.map_err(|e| e.kind())
        }
    }

    impl<C> Read for ConnErrorAdapter<C>
    where
        C: Read,
    {
        async fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
            self.0.read(buf).await.map_err(|e| e.kind())
        }

        async fn read_exact(&mut self, buf: &mut [u8]) -> Result<(), ReadExactError<Self::Error>> {
            self.0.read_exact(buf).await.map_err(|e| match e {
                ReadExactError::UnexpectedEof => ReadExactError::UnexpectedEof,
                ReadExactError::Other(e) => ReadExactError::Other(e.kind()),
            })
        }
    }
}

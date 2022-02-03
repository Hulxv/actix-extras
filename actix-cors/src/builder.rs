use std::{collections::HashSet, convert::TryInto, iter::FromIterator, rc::Rc};

use actix_utils::future::{self, Ready};
use actix_web::{
    body::{EitherBody, MessageBody},
    dev::{RequestHead, Service, ServiceRequest, ServiceResponse, Transform},
    error::HttpError,
    http::{
        header::{HeaderName, HeaderValue},
        Method, Uri,
    },
    Either, Error, Result,
};
use log::error;
use once_cell::sync::Lazy;
use smallvec::smallvec;

use crate::{AllOrSome, CorsError, CorsMiddleware, Inner, OriginFn};

/// Convenience for getting mut refs to inner. Cleaner than `Rc::get_mut`.
/// Additionally, always causes first error (if any) to be reported during initialization.
fn cors<'a>(
    inner: &'a mut Rc<Inner>,
    err: &Option<Either<HttpError, CorsError>>,
) -> Option<&'a mut Inner> {
    if err.is_some() {
        return None;
    }

    Rc::get_mut(inner)
}

static ALL_METHODS_SET: Lazy<HashSet<Method>> = Lazy::new(|| {
    HashSet::from_iter(vec![
        Method::GET,
        Method::POST,
        Method::PUT,
        Method::DELETE,
        Method::HEAD,
        Method::OPTIONS,
        Method::CONNECT,
        Method::PATCH,
        Method::TRACE,
    ])
});

/// Builder for CORS middleware.
///
/// To construct a CORS middleware, call [`Cors::default()`] to create a blank, restrictive builder.
/// Then use any of the builder methods to customize CORS behavior.
///
/// The alternative [`Cors::permissive()`] constructor is available for local development, allowing
/// all origins and headers, etc. **The permissive constructor should not be used in production.**
///
/// # Errors
/// Errors surface in the middleware initialization phase. This means that, if you have logs enabled
/// in Actix Web (using `env_logger` or other crate that exposes logs from the `log` crate), error
/// messages will outline what is wrong with the CORS configuration in the server logs and the
/// server will fail to start up or serve requests.
///
/// # Example
/// ```
/// use actix_cors::Cors;
/// use actix_web::http::header;
///
/// let cors = Cors::default()
///     .allowed_origin("https://www.rust-lang.org")
///     .allowed_methods(vec!["GET", "POST"])
///     .allowed_headers(vec![header::AUTHORIZATION, header::ACCEPT])
///     .allowed_header(header::CONTENT_TYPE)
///     .max_age(3600);
///
/// // `cors` can now be used in `App::wrap`.
/// ```
#[derive(Debug)]
pub struct Cors {
    inner: Rc<Inner>,
    error: Option<Either<HttpError, CorsError>>,
}

impl Cors {
    /// A very permissive set of default for quick development. Not recommended for production use.
    ///
    /// *All* origins, methods, request headers and exposed headers allowed. Credentials supported.
    /// Max age 1 hour. Does not send wildcard.
    pub fn permissive() -> Self {
        let inner = Inner {
            allowed_origins: AllOrSome::All,
            allowed_origins_fns: smallvec![],

            allowed_methods: ALL_METHODS_SET.clone(),
            allowed_methods_baked: None,

            allowed_headers: AllOrSome::All,
            allowed_headers_baked: None,

            expose_headers: AllOrSome::All,
            expose_headers_baked: None,

            max_age: Some(3600),
            preflight: true,
            send_wildcard: false,
            supports_credentials: true,
            vary_header: true,
        };

        Cors {
            inner: Rc::new(inner),
            error: None,
        }
    }

    /// Resets allowed origin list to a state where any origin is accepted.
    ///
    /// See [`Cors::allowed_origin`] for more info on allowed origins.
    pub fn allow_any_origin(mut self) -> Cors {
        if let Some(cors) = cors(&mut self.inner, &self.error) {
            cors.allowed_origins = AllOrSome::All;
        }

        self
    }

    /// Add an origin that is allowed to make requests.
    ///
    /// By default, requests from all origins are accepted by CORS logic. This method allows to
    /// specify a finite set of origins to verify the value of the `Origin` request header.
    ///
    /// These are `origin-or-null` types in the [Fetch Standard].
    ///
    /// When this list is set, the client's `Origin` request header will be checked in a
    /// case-sensitive manner.
    ///
    /// When all origins are allowed and `send_wildcard` is set, `*` will be sent in the
    /// `Access-Control-Allow-Origin` response header. If `send_wildcard` is not set, the client's
    /// `Origin` request header will be echoed back in the `Access-Control-Allow-Origin`
    /// response header.
    ///
    /// If the origin of the request doesn't match any allowed origins and at least one
    /// `allowed_origin_fn` function is set, these functions will be used to determinate
    /// allowed origins.
    ///
    /// # Initialization Errors
    /// - If supplied origin is not valid uri
    /// - If supplied origin is a wildcard (`*`). [`Cors::send_wildcard`] should be used instead.
    ///
    /// [Fetch Standard]: https://fetch.spec.whatwg.org/#origin-header
    pub fn allowed_origin(mut self, origin: &str) -> Cors {
        if let Some(cors) = cors(&mut self.inner, &self.error) {
            match TryInto::<Uri>::try_into(origin) {
                Ok(_) if origin == "*" => {
                    error!("Wildcard in `allowed_origin` is not allowed. Use `send_wildcard`.");
                    self.error = Some(Either::Right(CorsError::WildcardOrigin));
                }

                Ok(_) => {
                    if cors.allowed_origins.is_all() {
                        cors.allowed_origins = AllOrSome::Some(HashSet::with_capacity(8));
                    }

                    if let Some(origins) = cors.allowed_origins.as_mut() {
                        // any uri is a valid header value
                        let hv = origin.try_into().unwrap();
                        origins.insert(hv);
                    }
                }

                Err(err) => {
                    self.error = Some(Either::Left(err.into()));
                }
            }
        }

        self
    }

    /// Determinate allowed origins by processing requests which didn't match any origins specified
    /// in the `allowed_origin`.
    ///
    /// The function will receive two parameters, the Origin header value, and the `RequestHead` of
    /// each request, which can be used to determine whether to allow the request or not.
    ///
    /// If the function returns `true`, the client's `Origin` request header will be echoed back
    /// into the `Access-Control-Allow-Origin` response header.
    pub fn allowed_origin_fn<F>(mut self, f: F) -> Cors
    where
        F: (Fn(&HeaderValue, &RequestHead) -> bool) + 'static,
    {
        if let Some(cors) = cors(&mut self.inner, &self.error) {
            cors.allowed_origins_fns.push(OriginFn {
                boxed_fn: Rc::new(f),
            });
        }

        self
    }

    /// Resets allowed methods list to all methods.
    ///
    /// See [`Cors::allowed_methods`] for more info on allowed methods.
    pub fn allow_any_method(mut self) -> Cors {
        if let Some(cors) = cors(&mut self.inner, &self.error) {
            cors.allowed_methods = ALL_METHODS_SET.clone();
        }

        self
    }

    /// Set a list of methods which allowed origins can perform.
    ///
    /// These will be sent in the `Access-Control-Allow-Methods` response header as specified in
    /// the [Fetch Standard CORS protocol].
    ///
    /// Defaults to `[GET, HEAD, POST, OPTIONS, PUT, PATCH, DELETE]`
    ///
    /// [Fetch Standard CORS protocol]: https://fetch.spec.whatwg.org/#http-cors-protocol
    pub fn allowed_methods<U, M>(mut self, methods: U) -> Cors
    where
        U: IntoIterator<Item = M>,
        M: TryInto<Method>,
        <M as TryInto<Method>>::Error: Into<HttpError>,
    {
        if let Some(cors) = cors(&mut self.inner, &self.error) {
            for m in methods {
                match m.try_into() {
                    Ok(method) => {
                        cors.allowed_methods.insert(method);
                    }

                    Err(err) => {
                        self.error = Some(Either::Left(err.into()));
                        break;
                    }
                }
            }
        }

        self
    }

    /// Resets allowed request header list to a state where any header is accepted.
    ///
    /// See [`Cors::allowed_headers`] for more info on allowed request headers.
    pub fn allow_any_header(mut self) -> Cors {
        if let Some(cors) = cors(&mut self.inner, &self.error) {
            cors.allowed_headers = AllOrSome::All;
        }

        self
    }

    /// Add an allowed request header.
    ///
    /// See [`Cors::allowed_headers`] for more info on allowed request headers.
    pub fn allowed_header<H>(mut self, header: H) -> Cors
    where
        H: TryInto<HeaderName>,
        <H as TryInto<HeaderName>>::Error: Into<HttpError>,
    {
        if let Some(cors) = cors(&mut self.inner, &self.error) {
            match header.try_into() {
                Ok(method) => {
                    if cors.allowed_headers.is_all() {
                        cors.allowed_headers = AllOrSome::Some(HashSet::with_capacity(8));
                    }

                    if let AllOrSome::Some(ref mut headers) = cors.allowed_headers {
                        headers.insert(method);
                    }
                }

                Err(err) => self.error = Some(Either::Left(err.into())),
            }
        }

        self
    }

    /// Set a list of request header field names which can be used when this resource is accessed by
    /// allowed origins.
    ///
    /// If `All` is set, whatever is requested by the client in `Access-Control-Request-Headers`
    /// will be echoed back in the `Access-Control-Allow-Headers` header as specified in
    /// the [Fetch Standard CORS protocol].
    ///
    /// Defaults to `All`.
    ///
    /// [Fetch Standard CORS protocol]: https://fetch.spec.whatwg.org/#http-cors-protocol
    pub fn allowed_headers<U, H>(mut self, headers: U) -> Cors
    where
        U: IntoIterator<Item = H>,
        H: TryInto<HeaderName>,
        <H as TryInto<HeaderName>>::Error: Into<HttpError>,
    {
        if let Some(cors) = cors(&mut self.inner, &self.error) {
            for h in headers {
                match h.try_into() {
                    Ok(method) => {
                        if cors.allowed_headers.is_all() {
                            cors.allowed_headers = AllOrSome::Some(HashSet::with_capacity(8));
                        }

                        if let AllOrSome::Some(ref mut headers) = cors.allowed_headers {
                            headers.insert(method);
                        }
                    }
                    Err(err) => {
                        self.error = Some(Either::Left(err.into()));
                        break;
                    }
                }
            }
        }

        self
    }

    /// Resets exposed response header list to a state where any header is accepted.
    ///
    /// See [`Cors::expose_headers`] for more info on exposed response headers.
    pub fn expose_any_header(mut self) -> Cors {
        if let Some(cors) = cors(&mut self.inner, &self.error) {
            cors.expose_headers = AllOrSome::All;
        }

        self
    }

    /// Set a list of headers which are safe to expose to the API of a CORS API specification.
    /// This corresponds to the `Access-Control-Expose-Headers` response header as specified in
    /// the [Fetch Standard CORS protocol].
    ///
    /// This defaults to an empty set.
    ///
    /// [Fetch Standard CORS protocol]: https://fetch.spec.whatwg.org/#http-cors-protocol
    pub fn expose_headers<U, H>(mut self, headers: U) -> Cors
    where
        U: IntoIterator<Item = H>,
        H: TryInto<HeaderName>,
        <H as TryInto<HeaderName>>::Error: Into<HttpError>,
    {
        for h in headers {
            match h.try_into() {
                Ok(header) => {
                    if let Some(cors) = cors(&mut self.inner, &self.error) {
                        if cors.expose_headers.is_all() {
                            cors.expose_headers = AllOrSome::Some(HashSet::with_capacity(8));
                        }
                        if let AllOrSome::Some(ref mut headers) = cors.expose_headers {
                            headers.insert(header);
                        }
                    }
                }
                Err(err) => {
                    self.error = Some(Either::Left(err.into()));
                    break;
                }
            }
        }

        self
    }

    /// Set a maximum time (in seconds) for which this CORS request maybe cached.
    /// This value is set as the `Access-Control-Max-Age` header as specified in
    /// the [Fetch Standard CORS protocol].
    ///
    /// Pass a number (of seconds) or use None to disable sending max age header.
    ///
    /// [Fetch Standard CORS protocol]: https://fetch.spec.whatwg.org/#http-cors-protocol
    pub fn max_age(mut self, max_age: impl Into<Option<usize>>) -> Cors {
        if let Some(cors) = cors(&mut self.inner, &self.error) {
            cors.max_age = max_age.into()
        }

        self
    }

    /// Set to use wildcard origins.
    ///
    /// If send wildcard is set and the `allowed_origins` parameter is `All`, a wildcard
    /// `Access-Control-Allow-Origin` response header is sent, rather than the request’s
    /// `Origin` header.
    ///
    /// This **CANNOT** be used in conjunction with `allowed_origins` set to `All` and
    /// `allow_credentials` set to `true`. Depending on the mode of usage, this will either result
    /// in an `CorsError::CredentialsWithWildcardOrigin` error during actix launch or runtime.
    ///
    /// Defaults to `false`.
    pub fn send_wildcard(mut self) -> Cors {
        if let Some(cors) = cors(&mut self.inner, &self.error) {
            cors.send_wildcard = true
        }

        self
    }

    /// Allows users to make authenticated requests
    ///
    /// If true, injects the `Access-Control-Allow-Credentials` header in responses. This allows
    /// cookies and credentials to be submitted across domains as specified in
    /// the [Fetch Standard CORS protocol].
    ///
    /// This option cannot be used in conjunction with an `allowed_origin` set to `All` and
    /// `send_wildcards` set to `true`.
    ///
    /// Defaults to `false`.
    ///
    /// A server initialization error will occur if credentials are allowed, but the Origin is set
    /// to send wildcards (`*`); this is not allowed by the CORS protocol.
    ///
    /// [Fetch Standard CORS protocol]: https://fetch.spec.whatwg.org/#http-cors-protocol
    pub fn supports_credentials(mut self) -> Cors {
        if let Some(cors) = cors(&mut self.inner, &self.error) {
            cors.supports_credentials = true
        }

        self
    }

    /// Disable `Vary` header support.
    ///
    /// When enabled the header `Vary: Origin` will be returned as per the Fetch Standard
    /// implementation guidelines.
    ///
    /// Setting this header when the `Access-Control-Allow-Origin` is dynamically generated
    /// (eg. when there is more than one allowed origin, and an Origin other than '*' is returned)
    /// informs CDNs and other caches that the CORS headers are dynamic, and cannot be cached.
    ///
    /// By default, `Vary` header support is enabled.
    pub fn disable_vary_header(mut self) -> Cors {
        if let Some(cors) = cors(&mut self.inner, &self.error) {
            cors.vary_header = false
        }

        self
    }

    /// Disable support for preflight requests.
    ///
    /// When enabled CORS middleware automatically handles `OPTIONS` requests.
    /// This is useful for application level middleware.
    ///
    /// By default *preflight* support is enabled.
    pub fn disable_preflight(mut self) -> Cors {
        if let Some(cors) = cors(&mut self.inner, &self.error) {
            cors.preflight = false
        }

        self
    }
}

impl Default for Cors {
    /// A restrictive (security paranoid) set of defaults.
    ///
    /// *No* allowed origins, methods, request headers or exposed headers. Credentials
    /// not supported. No max age (will use browser's default).
    fn default() -> Cors {
        let inner = Inner {
            allowed_origins: AllOrSome::Some(HashSet::with_capacity(8)),
            allowed_origins_fns: smallvec![],

            allowed_methods: HashSet::with_capacity(8),
            allowed_methods_baked: None,

            allowed_headers: AllOrSome::Some(HashSet::with_capacity(8)),
            allowed_headers_baked: None,

            expose_headers: AllOrSome::Some(HashSet::with_capacity(8)),
            expose_headers_baked: None,

            max_age: None,
            preflight: true,
            send_wildcard: false,
            supports_credentials: false,
            vary_header: true,
        };

        Cors {
            inner: Rc::new(inner),
            error: None,
        }
    }
}

impl<S, B> Transform<S, ServiceRequest> for Cors
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error>,
    S::Future: 'static,

    B: MessageBody + 'static,
    B::Error: Into<Error>,
{
    type Response = ServiceResponse<EitherBody<B>>;
    type Error = Error;
    type InitError = ();
    type Transform = CorsMiddleware<S>;
    type Future = Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        if let Some(ref err) = self.error {
            match err {
                Either::Left(err) => error!("{}", err),
                Either::Right(err) => error!("{}", err),
            }

            return future::err(());
        }

        let mut inner = Rc::clone(&self.inner);

        if inner.supports_credentials && inner.send_wildcard && inner.allowed_origins.is_all() {
            error!(
                "Illegal combination of CORS options: credentials can not be supported when all \
                    origins are allowed and `send_wildcard` is enabled."
            );
            return future::err(());
        }

        // bake allowed headers value if Some and not empty
        match inner.allowed_headers.as_ref() {
            Some(header_set) if !header_set.is_empty() => {
                let allowed_headers_str = intersperse_header_values(header_set);
                Rc::make_mut(&mut inner).allowed_headers_baked = Some(allowed_headers_str);
            }
            _ => {}
        }

        // bake allowed methods value if not empty
        if !inner.allowed_methods.is_empty() {
            let allowed_methods_str = intersperse_header_values(&inner.allowed_methods);
            Rc::make_mut(&mut inner).allowed_methods_baked = Some(allowed_methods_str);
        }

        // bake exposed headers value if Some and not empty
        match inner.expose_headers.as_ref() {
            Some(header_set) if !header_set.is_empty() => {
                let expose_headers_str = intersperse_header_values(header_set);
                Rc::make_mut(&mut inner).expose_headers_baked = Some(expose_headers_str);
            }
            _ => {}
        }

        future::ok(CorsMiddleware { service, inner })
    }
}

/// Only call when values are guaranteed to be valid header values and set is not empty.
pub(crate) fn intersperse_header_values<T>(val_set: &HashSet<T>) -> HeaderValue
where
    T: AsRef<str>,
{
    debug_assert!(
        !val_set.is_empty(),
        "only call `intersperse_header_values` when set is not empty"
    );

    val_set
        .iter()
        .fold(String::with_capacity(64), |mut acc, val| {
            acc.push_str(", ");
            acc.push_str(val.as_ref());
            acc
        })
        // set is not empty so string will always have leading ", " to trim
        [2..]
        .try_into()
        // all method names are valid header values
        .unwrap()
}

#[cfg(test)]
mod test {
    use std::convert::{Infallible, TryInto};

    use actix_web::{
        body,
        dev::{fn_service, Transform},
        http::{header::HeaderName, StatusCode},
        test::{self, TestRequest},
        web::HttpResponse,
    };

    use super::*;

    #[test]
    fn illegal_allow_credentials() {
        // using the permissive defaults (all origins allowed) and adding send_wildcard
        // and supports_credentials should error on construction

        assert!(Cors::permissive()
            .supports_credentials()
            .send_wildcard()
            .new_transform(test::ok_service())
            .into_inner()
            .is_err());
    }

    #[actix_web::test]
    async fn restrictive_defaults() {
        let cors = Cors::default()
            .new_transform(test::ok_service())
            .await
            .unwrap();

        let req = TestRequest::default()
            .insert_header(("Origin", "https://www.example.com"))
            .to_srv_request();

        let resp = test::call_service(&cors, req).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[actix_web::test]
    async fn allowed_header_try_from() {
        let _cors = Cors::default().allowed_header("Content-Type");
    }

    #[actix_web::test]
    async fn allowed_header_try_into() {
        struct ContentType;

        impl TryInto<HeaderName> for ContentType {
            type Error = Infallible;

            fn try_into(self) -> Result<HeaderName, Self::Error> {
                Ok(HeaderName::from_static("content-type"))
            }
        }

        let _cors = Cors::default().allowed_header(ContentType);
    }

    #[actix_web::test]
    async fn middleware_generic_over_body_type() {
        let srv = fn_service(|req: ServiceRequest| async move {
            Ok(req.into_response(HttpResponse::with_body(StatusCode::OK, body::None::new())))
        });

        Cors::default().new_transform(srv).await.unwrap();
    }
}

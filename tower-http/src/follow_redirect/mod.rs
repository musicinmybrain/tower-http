//! Middleware for following redirections.

pub mod policy;

use self::policy::{Action, Attempt, Policy, Standard};
use futures_core::ready;
use futures_util::future::Either;
use http::{
    header::LOCATION, HeaderMap, HeaderValue, Method, Request, Response, StatusCode, Uri, Version,
};
use http_body::Body;
use iri_string::{
    spec::UriSpec,
    types::{RiAbsoluteString, RiReferenceStr},
};
use pin_project::pin_project;
use std::{
    convert::TryFrom,
    future::Future,
    mem,
    pin::Pin,
    str,
    task::{Context, Poll},
};
use tower::util::Oneshot;
use tower_layer::Layer;
use tower_service::Service;

/// [`Layer`] for retrying requests with a [`Service`] to follow redirection responses.
#[derive(Clone, Copy, Debug, Default)]
pub struct FollowRedirectLayer<P = Standard> {
    policy: P,
}

impl FollowRedirectLayer {
    /// Create a new [`FollowRedirectLayer`] with a [`Standard`] redirection policy.
    pub fn standard() -> Self {
        Self::default()
    }
}

impl<P> FollowRedirectLayer<P> {
    /// Create a new [`FollowRedirectLayer`] with the given redirection [`Policy`].
    pub fn new(policy: P) -> Self {
        FollowRedirectLayer { policy }
    }
}

impl<S, P> Layer<S> for FollowRedirectLayer<P>
where
    S: Clone,
    P: Clone,
{
    type Service = FollowRedirect<S, P>;

    fn layer(&self, inner: S) -> Self::Service {
        FollowRedirect::new(inner, self.policy.clone())
    }
}

/// Middleware that retries requests with a [`Service`] to follow redirection responses.
#[derive(Clone, Copy, Debug)]
pub struct FollowRedirect<S, P = Standard> {
    inner: S,
    policy: P,
}

impl<S> FollowRedirect<S> {
    /// Create a new [`FollowRedirect`] with a [`Standard`] redirection policy.
    pub fn standard(inner: S) -> Self {
        Self::new(inner, Standard::default())
    }

    define_inner_service_accessors!();
}

impl<S, P> FollowRedirect<S, P>
where
    P: Clone,
{
    /// Create a new [`FollowRedirect`] with the given redirection [`Policy`].
    pub fn new(inner: S, policy: P) -> Self {
        FollowRedirect { inner, policy }
    }

    /// Returns a new [`Layer`] that wraps services with a `FollowRedirect` middleware.
    ///
    /// [`Layer`]: tower_layer::Layer
    pub fn layer(policy: P) -> FollowRedirectLayer<P> {
        FollowRedirectLayer::new(policy)
    }
}

impl<ReqBody, ResBody, S, P> Service<Request<ReqBody>> for FollowRedirect<S, P>
where
    S: Service<Request<ReqBody>, Response = Response<ResBody>> + Clone,
    ReqBody: Body + Default,
    P: Policy<ReqBody, S::Error> + Clone,
{
    type Response = Response<ResBody>;
    type Error = S::Error;
    type Future = ResponseFuture<S, ReqBody, P>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: Request<ReqBody>) -> Self::Future {
        let service = self.inner.clone();
        let mut service = mem::replace(&mut self.inner, service);
        let mut policy = self.policy.clone();
        let mut body = BodyRepr::None;
        body.try_clone_from(req.body(), &policy);
        policy.on_request(&mut req);
        ResponseFuture {
            method: req.method().clone(),
            uri: req.uri().clone(),
            version: req.version(),
            headers: req.headers().clone(),
            body,
            future: Either::Left(service.call(req)),
            service,
            policy,
        }
    }
}

/// Response future for [`FollowRedirect`].
#[pin_project]
#[derive(Debug)]
pub struct ResponseFuture<S, B, P>
where
    S: Service<Request<B>>,
{
    #[pin]
    future: Either<S::Future, Oneshot<S, Request<B>>>,
    service: S,
    policy: P,
    method: Method,
    uri: Uri,
    version: Version,
    headers: HeaderMap<HeaderValue>,
    body: BodyRepr<B>,
}

impl<S, ReqBody, ResBody, P> Future for ResponseFuture<S, ReqBody, P>
where
    S: Service<Request<ReqBody>, Response = Response<ResBody>> + Clone,
    ReqBody: Body + Default,
    P: Policy<ReqBody, S::Error>,
{
    type Output = Result<Response<ResBody>, S::Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.project();
        let res = ready!(this.future.as_mut().poll(cx)?);

        match res.status() {
            StatusCode::MOVED_PERMANENTLY | StatusCode::FOUND => {
                // User agents MAY change the request method from POST to GET
                // (RFC 7231 section 6.4.2. and 6.4.3.).
                if *this.method == Method::POST {
                    *this.method = Method::GET;
                    *this.body = BodyRepr::Empty;
                }
            }
            StatusCode::SEE_OTHER => {
                // A user agent can perform a GET or HEAD request (RFC 7231 section 6.4.4.).
                if *this.method != Method::HEAD {
                    *this.method = Method::GET;
                }
                *this.body = BodyRepr::Empty;
            }
            StatusCode::TEMPORARY_REDIRECT | StatusCode::PERMANENT_REDIRECT => {}
            _ => return Poll::Ready(Ok(res)),
        };

        let body = if let Some(body) = this.body.take() {
            body
        } else {
            return Poll::Ready(Ok(res));
        };

        let location = res
            .headers()
            .get(&LOCATION)
            .and_then(|loc| resolve_uri(str::from_utf8(loc.as_bytes()).ok()?, this.uri));
        let location = if let Some(loc) = location {
            loc
        } else {
            return Poll::Ready(Ok(res));
        };

        let attempt = Attempt {
            status: res.status(),
            location: &location,
            previous: this.uri,
        };
        match this.policy.redirect(&attempt)? {
            Action::Follow => {
                this.body.try_clone_from(&body, &this.policy);

                let mut req = Request::new(body);
                *req.uri_mut() = location;
                *req.method_mut() = this.method.clone();
                *req.version_mut() = *this.version;
                *req.headers_mut() = this.headers.clone();
                this.policy.on_request(&mut req);
                this.future
                    .set(Either::Right(Oneshot::new(this.service.clone(), req)));

                cx.waker().wake_by_ref();
                Poll::Pending
            }
            Action::Stop => Poll::Ready(Ok(res)),
        }
    }
}

#[derive(Debug)]
enum BodyRepr<B> {
    Some(B),
    Empty,
    None,
}

impl<B> BodyRepr<B>
where
    B: Body + Default,
{
    fn take(&mut self) -> Option<B> {
        match mem::replace(self, BodyRepr::None) {
            BodyRepr::Some(body) => Some(body),
            BodyRepr::Empty => {
                *self = BodyRepr::Empty;
                Some(B::default())
            }
            BodyRepr::None => None,
        }
    }

    fn try_clone_from<P, E>(&mut self, body: &B, policy: &P)
    where
        P: Policy<B, E>,
    {
        match self {
            BodyRepr::Some(_) | BodyRepr::Empty => {}
            BodyRepr::None => {
                if let Some(body) = clone_body(policy, body) {
                    *self = BodyRepr::Some(body);
                }
            }
        }
    }
}

fn clone_body<P, B, E>(policy: &P, body: &B) -> Option<B>
where
    P: Policy<B, E>,
    B: Body + Default,
{
    if body.size_hint().exact() == Some(0) {
        Some(B::default())
    } else {
        policy.clone_body(body)
    }
}

/// Try to resolve a URI reference `relative` against a base URI `base`.
fn resolve_uri(relative: &str, base: &Uri) -> Option<Uri> {
    let relative = RiReferenceStr::<UriSpec>::new(relative).ok()?;
    let base = RiAbsoluteString::try_from(base.to_string()).ok()?;
    let uri = relative.resolve_against(&base);
    Uri::try_from(uri.as_str()).ok()
}

#[cfg(test)]
mod tests {
    use super::{policy::*, *};
    use hyper::{header::LOCATION, Body};
    use std::convert::Infallible;
    use tower::{ServiceBuilder, ServiceExt};

    #[tokio::test]
    async fn follows() {
        let svc = ServiceBuilder::new()
            .layer(FollowRedirectLayer::new(Action::Follow))
            .service_fn(handle);
        let req = Request::builder()
            .uri("http://example.com/42")
            .body(Body::empty())
            .unwrap();
        let res = svc.oneshot(req).await.unwrap();
        assert_eq!(res.into_body(), 0);
    }

    #[tokio::test]
    async fn stops() {
        let svc = ServiceBuilder::new()
            .layer(FollowRedirectLayer::new(Action::Stop))
            .service_fn(handle);
        let req = Request::builder()
            .uri("http://example.com/42")
            .body(Body::empty())
            .unwrap();
        let res = svc.oneshot(req).await.unwrap();
        assert_eq!(res.into_body(), 42);
    }

    #[tokio::test]
    async fn limited() {
        let svc = ServiceBuilder::new()
            .layer(FollowRedirectLayer::new(Limited::new(10)))
            .service_fn(handle);
        let req = Request::builder()
            .uri("http://example.com/42")
            .body(Body::empty())
            .unwrap();
        let res = svc.oneshot(req).await.unwrap();
        assert_eq!(res.into_body(), 42 - 10);
    }

    /// A server with an endpoint `GET /{n}` which redirects to `/{n-1}` unless `n` equals zero,
    /// returning `n` as the response body.
    async fn handle<B>(req: Request<B>) -> Result<Response<u64>, Infallible> {
        let n: u64 = req.uri().path()[1..].parse().unwrap();
        let mut res = Response::builder();
        if n > 0 {
            res = res
                .status(StatusCode::MOVED_PERMANENTLY)
                .header(LOCATION, format!("/{}", n - 1));
        }
        Ok::<_, Infallible>(res.body(n).unwrap())
    }
}

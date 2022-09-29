#![deny(missing_docs)]
#![deny(unsafe_code)]
//! # Actix Middleware - ETag
//!
//! To avoid sending unnecessary bodies downstream, this middleware handles comparing If-None-Match headers
//! to the calculated hash of the body of the GET request.
//! Inspired by Node's [express framework](http://expressjs.com/en/api.html#etag.options.table) and how it does ETag calculation, this middleware behaves in a similar fashion.
//!
//! First hash the resulting body, then base64 encode the hash and set this as the ETag header for the GET request.
//!
//! This does not save CPU resources on server side, since the body is still being calculated.
//!
//! Beware: This middleware does not look at headers, so if you need to refresh your headers even if body is exactly the same, use something else
//! (or better yet, add a PR on this repo adding a sane way to adhere to headers as well)
use std::pin::Pin;

use actix_service::{forward_ready, Service, Transform};
use actix_web::{HttpMessage, HttpResponse};
use actix_web::body::{BodySize, BoxBody, EitherBody, MessageBody};
use actix_web::dev::{ServiceRequest, ServiceResponse};
use actix_web::http::header::{EntityTag, ETag, IfNoneMatch, TryIntoHeaderPair};
use actix_web::http::Method;
use actix_web::web::Bytes;
use futures::{future::{ok, Ready}, Future};
use xxhash_rust::xxh3::xxh3_128;

///
/// This should be loaded as the last middleware, as in, first among the wraps.
/// Actix loads middlewares in bottom up fashion, and we want to have the resulting body from processing the entire request

/// # Examples
/// ```no_run
/// use actix_web::{web, App, HttpServer, HttpResponse, Error};
/// use actix_middleware_etag::{Etag};
///
/// // The secret key would usually be read from a configuration file/environment variables.
///
/// #[actix_web::main]
/// async fn main() -> std::io::Result<()> {
///     HttpServer::new(move ||
///             App::new()
///             // Add session management to your application using Redis for session state storage
///             .wrap(Etag::default())
///             .default_service(web::to(|| HttpResponse::Ok())))
///         .bind(("127.0.0.1", 8080))?
///         .run()
///         .await
/// }
/// ```
#[derive(Debug, Default)]
pub struct Etag;

impl<S, B> Transform<S, ServiceRequest> for Etag
    where S: Service<ServiceRequest, Response=ServiceResponse<B>, Error=actix_web::Error>,
          S::Future: 'static,
          B: MessageBody + 'static
{
    type Response = ServiceResponse<EitherBody<BoxBody>>;
    type Error = actix_web::Error;
    type Transform = EtagMiddleware<S>;
    type InitError = ();
    type Future = Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        ok(EtagMiddleware { service })
    }
}

///
/// The service holder for the transform that should happen
pub struct EtagMiddleware<S> {
    service: S,
}

impl<S, B> Service<ServiceRequest> for EtagMiddleware<S>
    where S: Service<ServiceRequest, Response=ServiceResponse<B>, Error=actix_web::Error>,
          S::Future: 'static,
          B: MessageBody + 'static
{
    type Response = ServiceResponse<EitherBody<BoxBody>>;
    type Error = actix_web::Error;
    #[allow(clippy::type_complexity)]
    type Future = Pin<Box<dyn Future<Output=Result<ServiceResponse<EitherBody<BoxBody>>, Self::Error>>>>;
    forward_ready!(service);

    fn call(&self, req: ServiceRequest) -> Self::Future {
        let request_etag_header: Option<IfNoneMatch> = req.get_header();
        let method = req.method().clone();
        let fut = self.service.call(req);

        Box::pin(async move {
            let res: ServiceResponse<B> = fut.await?;
            match method {
                Method::GET => {
                    let mut modified = true;
                    let mut payload: Option<Bytes> = None;
                    let mut res = res.map_body(|_h, body| {
                        match body.size() {
                            BodySize::Sized(_size) => {
                                let bytes = body.try_into_bytes().unwrap_or_else(|_| Bytes::new());
                                payload = Some(bytes.clone());
                                bytes.clone().boxed()
                            }
                            _ => body.boxed()
                        }
                    });

                    match payload {
                        Some(bytes) => {
                            let response_hash = xxh3_128(&bytes);
                            let base64_response_hash = base64::encode(response_hash.to_le_bytes());
                            let tag = EntityTag::new_weak(base64_response_hash.clone());

                            if let Some(request_etag_header) = request_etag_header {
                                if request_etag_header == IfNoneMatch::Any || request_etag_header.to_string() == tag.to_string() {
                                    modified = false
                                }
                            }
                            if modified {
                                if let Ok((name, value)) = ETag(tag.clone()).try_into_pair() {
                                    res.headers_mut().insert(name, value);
                                }
                            }
                        }
                        None => {}
                    }


                    Ok(match modified {
                        false => res.into_response(HttpResponse::NotModified().finish()).map_into_right_body(),
                        true => res.map_into_left_body()
                    })

                }
                _ => Ok(res.map_into_boxed_body().map_into_left_body())
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use actix_service::IntoService;
    use actix_web::{App, http::StatusCode, Responder, test::{call_service, init_service, TestRequest}, web};
    use actix_web::http::header::{EntityTag, ETag, Header, HeaderName};
    use super::*;

    async fn index() -> impl Responder {
        HttpResponse::Ok().body("abcd")
    }

    async fn image() -> impl Responder {
        HttpResponse::Ok()
            .content_type("image/png")
            .body(&include_bytes!("assets/favicon.ico")[..])
    }

    #[actix_web::test]
    async fn test_generates_etag() {
        let srv = |req: ServiceRequest| {
            ok(req.into_response(HttpResponse::build(StatusCode::OK).body("abc")))
        };
        let etag_service = Etag::default();
        let srv = etag_service
            .new_transform(srv.into_service())
            .await
            .unwrap();

        let req = TestRequest::default().to_srv_request();
        let res = srv.call(req).await;
        if let Ok(response) = res {
            assert_eq!(response.status(), StatusCode::OK);
            let headers = response.headers();
            let etag = HeaderName::from_lowercase(b"etag").unwrap();
            let etag = headers.get(etag);
            assert_eq!(
                etag.unwrap().to_str().unwrap(),
                r#"W/"UDkviZRfr3iFYTpztlqwBg==""#
            );
        } else {
            panic!("No response was generated!");
        }
    }

    #[actix_web::test]
    async fn test_any_data_matches_wildcard_etag() {
        let mut app = init_service(
            App::new()
                .wrap(Etag::default())
                .route("/", web::get().to(index))
        ).await;

        let match_header = IfNoneMatch::Any;
        let req = TestRequest::default().append_header(match_header).to_request();
        let res = call_service(&mut app, req).await;
        assert_eq!(res.status(), StatusCode::NOT_MODIFIED)
    }

    #[actix_web::test]
    async fn test_generates_etag_on_changes() {
        let mut app = init_service(
            App::new()
                .wrap(Etag::default())
                .route("/", web::get().to(index))
        ).await;
        let match_header = IfNoneMatch::Items(vec![EntityTag::new_weak("UDkviZRfr3iFYTpztlqwBg==".to_string())]);
        let req = TestRequest::default().append_header(match_header).to_request();
        let res = call_service(&mut app, req).await;
        let etag = res.headers().get(ETag::name()).unwrap();
        assert_eq!(etag.to_str().unwrap(), r#"W/"PTWx0eye5xvCkPo9OGBrjQ==""#);
        assert!(res.status().is_success());
    }

    #[actix_web::test]
    async fn test_body_gets_preserved() {
        let mut app = init_service(
            App::new()
                .wrap(Etag::default())
                .route("/", web::get().to(index))
        ).await;
        let match_header = IfNoneMatch::Items(vec![EntityTag::new_weak("UDkviZRfr3iFYTpztlqwBg==".to_string())]);
        let req = TestRequest::default().append_header(match_header).to_request();
        let res = call_service(&mut app, req).await;
        assert!(res.status().is_success());
        let body = res.into_body();
        let body: Bytes = body.try_into_bytes().unwrap();
        let example: Bytes = Bytes::from("abcd");
        assert!(example.bytes().zip(body).all(|(a, b)| a.unwrap() == b));
    }

    #[actix_web::test]
    async fn test_favicon_generates_correct_status_coded_on_etag_match() {
        let mut app = init_service(
            App::new()
                .wrap(Etag::default())
                .route("/", web::get().to(image)))
            .await;
        let match_header = IfNoneMatch::Items(vec![EntityTag::new_weak("m0RKLkLoLS6kJ1N8xt0D5A==".to_string())]);
        let req = TestRequest::default().append_header(match_header).to_request();
        let res = call_service(&mut app, req).await;
        assert_eq!(res.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(res.into_body().size(), BodySize::Sized(0));
    }

    #[actix_web::test]
    async fn test_favicon_data_works() {
        let mut app = init_service(App::new()
            .wrap(Etag::default())
            .route("/", web::get().to(image)))
            .await;

        let match_header = IfNoneMatch::Items(vec![EntityTag::new_weak("UDkviZRfr3iFYTpztlqwBg==".to_string())]);
        let req = TestRequest::default().append_header(match_header).to_request();
        let res = call_service(&mut app, req).await;

        let etag = res.headers().get(ETag::name()).unwrap();
        assert_eq!(etag.to_str().unwrap(), r#"W/"m0RKLkLoLS6kJ1N8xt0D5A==""#)
    }

    #[actix_web::test]
    async fn does_not_add_etag_header_to_post_request() {
        let mut app = init_service(App::new()
            .wrap(Etag::default())
            .route("/", web::post().to(image)))
            .await;

        let req = TestRequest::default().method(Method::POST).to_request();
        let res = call_service(&mut app, req).await;

        assert_eq!(res.headers().get(ETag::name()), None)
    }
}

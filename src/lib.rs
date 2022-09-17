#![deny(missing_docs)]
#![deny(unsafe_code)]

use std::pin::Pin;

use actix_service::{forward_ready, Service, Transform};
use actix_web::{HttpMessage, HttpResponse};
use actix_web::body::MessageBody;
use actix_web::dev::{Response, ServiceRequest, ServiceResponse};
use actix_web::http::header::{EntityTag, IfNoneMatch, TryIntoHeaderPair, TryIntoHeaderValue};
use actix_web::web::Bytes;
use futures::{
    future::{ok, Ready},
    Future,
};
use xxhash_rust::xxh3::xxh3_128;

#[derive(Debug, Default)]
pub struct Etag;

impl<S, B> Transform<S, ServiceRequest> for Etag
    where S: Service<ServiceRequest, Response=ServiceResponse<B>, Error=actix_web::Error>,
          S::Future: 'static,
          B: MessageBody + 'static
{
    type Response = ServiceResponse<B>;
    type Error = actix_web::Error;
    type Transform = EtagMiddleware<S>;
    type InitError = ();
    type Future = Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        ok(EtagMiddleware { service })
    }
}

pub struct EtagMiddleware<S> {
    service: S,
}

impl<S, B> Service<ServiceRequest> for EtagMiddleware<S>
    where S: Service<ServiceRequest, Response=ServiceResponse<B>, Error=actix_web::Error>,
          S::Future: 'static,
          B: MessageBody + 'static
{
    type Response = ServiceResponse<B>;
    type Error = actix_web::Error;
    #[allow(clippy::type_complexity)]
    type Future = Pin<Box<dyn Future<Output=Result<Self::Response, Self::Error>>>>;
    forward_ready!(service);

    fn call(&self, req: ServiceRequest) -> Self::Future {
        let request_etag_header: Option<IfNoneMatch> = req.get_header();
        let fut = self.service.call(req);

        Box::pin(async move {
            let res = fut.await?;

            let mut payload = None;
            let mut res = res.map_body(|h, b| {
                payload = Some(b.try_into_bytes().unwrap_or_else(|_| Bytes::new()));
                b
            });
            let payload = if let Some(payload) = payload {
                if payload.is_empty() {
                    return Ok(res);
                } else {
                    payload
                }
            } else {
                return Ok(res);
            };

            let response_hash = xxh3_128(&payload);

            let base64_response_hash = base64::encode(response_hash.to_le_bytes());

            if let Some(request_etag_header) = request_etag_header {
                if let Ok(request_etag) = request_etag_header.try_into_value() {
                    if let Ok(request_etag_string) = request_etag.to_str() {
                        let request_etag_string = request_etag_string.replace("W/", "").replace("\"", "");
                        if request_etag_string == base64_response_hash.as_str() || request_etag_string == "*" {
                            let not_modified = res.into_response(HttpResponse::NotModified().finish());
                            return Ok(not_modified);
                        }
                    }
                }
            }

            if let Ok((name, value)) = actix_web::http::header::ETag(EntityTag::new_weak(base64_response_hash)).try_into_pair() {
                res.headers_mut().insert(name, value);
            }

            Ok(res)
        })
    }
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use actix_service::IntoService;
    use actix_web::{
        App,
        http::StatusCode,
        test::{call_service, init_service, TestRequest}, web,
    };
    use actix_web::http::header::{EntityTag, HeaderName, IF_NONE_MATCH};

    use super::*;

    #[actix_web::test]
    async fn test_generates_etag() {
        let srv = |req: ServiceRequest| {
            ok(req.into_response(HttpResponse::build(StatusCode::OK).body("abc")))
        };
        let etag_service = Etag::default();
        let mut srv = etag_service
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
                "W/\"UDkviZRfr3iFYTpztlqwBg==\""
            );
        } else {
            panic!("No response was generated!");
        }
    }

    #[actix_web::test]
    async fn test_if_none_matched_generates_not_modified() {
        let mut app = init_service(App::new().wrap(Etag::default()).service(web::resource("/").to(|| HttpResponse::Ok().body("abc"))),
        ).await;
        let match_header = IfNoneMatch::Items(vec![EntityTag::new_weak("W/\"UDkviZRfr3iFYTpztlqwBg==\"".to_string())]).try_into_pair();
        let req = TestRequest::default().append_header(match_header).to_request();
        let res = call_service(&mut app, req).await;
        assert_eq!(res.status(), StatusCode::NOT_MODIFIED);
        let etag = actix_web::http::header::ETag::parse(&res).unwrap();
        assert_eq!(etag, None);
    }

    #[actix_web::test]
    async fn test_generates_etag_on_changes() {
        let mut app = init_service(
            App::new()
                .wrap(Etag::default())
                .service(web::resource("/").to(|| HttpResponse::Ok().body("abcd"))),
        )
            .await;
        let match_header = IfNoneMatch::Items(vec![EntityTag::new_weak("W/\"UDkviZRfr3iFYTpztlqwBg==\"".to_string())]).try_into_pair();
        let req = TestRequest::default().append_header(match_header).to_request();
        let res = call_service(&mut app, req).await;
        assert!(res.status().is_success());
        let etag = actix_web::http::header::ETag::parse(&res).unwrap();
        assert_eq!(etag.to_string(), "W/\"PTWx0eye5xvCkPo9OGBrjQ==\"")
    }

    #[actix_web::test]
    async fn test_body_gets_preserved() {
        let mut app = init_service(
            App::new()
                .wrap(Etag::default())
                .service(web::resource("/").to(|| HttpResponse::Ok().body("abcd"))),
        )
            .await;
        let match_header = IfNoneMatch::Items(vec![EntityTag::new_weak("W/\"UDkviZRfr3iFYTpztlqwBg==\"".to_string())]).try_into_pair();
        let req = TestRequest::default().append_header(match_header).to_request();
        let res = call_service(&mut app, req).await;
        assert!(res.status().is_success());
        let body = res.into_body();
        let body = body.as_ref().unwrap();
        let example = web::BytesMut::from("abcd");
        assert_eq!(example.bytes(), body);
    }

    #[actix_web::test]
    async fn test_favicon_generates_correct_status_coded_on_etag_match() {
        init_service(App::new().wrap(Etag::default()).service(
            web::resource("/").to(|| {
                HttpResponse::Ok()
                    .content_type("image/png")
                    .body(&include_bytes!("../../assets/favicon.ico")[..])
            }),
        ))
            .await;
        let match_header = IfNoneMatch::Items(vec![EntityTag::new_weak("W/\"qNoeBNlNgCaLddIJcyev5A==\"".to_string())]).try_into_pair();
        let req = TestRequest::default().append_header(match_header).to_request();
        let res = call_service(&mut app, req).await;
        assert_eq!(res.status(), StatusCode::NOT_MODIFIED);
    }

    #[actix_web::test]
    async fn test_favicon_data_works() {
        let mut app = init_service(App::new().wrap(Etag::default()).service(
            web::resource("/").to(|| {
                HttpResponse::Ok()
                    .content_type("image/png")
                    .body(&include_bytes!("../../assets/favicon.ico")[..])
            }),
        ))
            .await;

        let match_header = IfNoneMatch::Items(vec![EntityTag::new_weak("W/\"UDkviZRfr3iFYTpztlqwBg==\"".to_string())]).try_into_pair();
        let req = TestRequest::default().append_header(match_header).to_request();
        let res = call_service(&mut app, req).await;


        assert!(res.status().is_success());
        let body = res.into_body();
        let body = body.as_ref().unwrap();
        assert_eq!(web::BytesMut::from(&include_bytes!("../assets/favicon.ico")[..]), body);
        let etag = actix_web::http::header::ETag::parse(&res).unwrap();
        assert_eq!(etag.to_string(), "W/\"qNoeBNlNgCaLddIJcyev5A==\"")
    }
}

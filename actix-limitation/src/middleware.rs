use std::{future::Future, pin::Pin, rc::Rc};

use actix_session::SessionExt as _;
use actix_utils::future::{ok, Ready};
use actix_web::{
    body::EitherBody,
    dev::{forward_ready, Service, ServiceRequest, ServiceResponse, Transform},
    http::StatusCode,
    web, Error, HttpResponse,
};

use crate::{Error as LimitationError, Limiter};

/// Rate limit middleware.
#[derive(Debug, Default)]
#[non_exhaustive]
pub struct RateLimiter;

impl<S, B> Transform<S, ServiceRequest> for RateLimiter
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error> + 'static,
    S::Future: 'static,
    B: 'static,
{
    type Response = ServiceResponse<EitherBody<B>>;
    type Error = Error;
    type Transform = RateLimiterMiddleware<S>;
    type InitError = ();
    type Future = Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        ok(RateLimiterMiddleware {
            service: Rc::new(service),
        })
    }
}

/// Rate limit middleware service.
#[derive(Debug)]
pub struct RateLimiterMiddleware<S> {
    service: Rc<S>,
}

impl<S, B> Service<ServiceRequest> for RateLimiterMiddleware<S>
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error> + 'static,
    S::Future: 'static,
    B: 'static,
{
    type Response = ServiceResponse<EitherBody<B>>;
    type Error = Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>>>>;

    forward_ready!(service);

    fn call(&self, req: ServiceRequest) -> Self::Future {
        // A mis-configuration of the Actix App will result in a **runtime** failure, so the expect
        // method description is important context for the developer.
        let limiter = req
            .app_data::<web::Data<Limiter>>()
            .expect("web::Data<Limiter> should be set in app data for RateLimiter middleware")
            .clone();

        let key = req.get_session().get(&limiter.session_key).unwrap_or(None);
        let service = Rc::clone(&self.service);

        let key = match key {
            Some(key) => key,
            None => {
                let fallback = req.cookie(&limiter.cookie_name).map(|c| c.to_string());

                match fallback {
                    Some(key) => key,
                    None => {
                        return Box::pin(async move {
                            service
                                .call(req)
                                .await
                                .map(ServiceResponse::map_into_left_body)
                        });
                    }
                }
            }
        };

        Box::pin(async move {
            let status = limiter.count(key.to_string()).await;

            if let Err(err) = status {
                match err {
                    LimitationError::LimitExceeded(_) => {
                        log::warn!("Rate limit exceed error for {}", key);

                        Ok(req.into_response(
                            HttpResponse::new(StatusCode::TOO_MANY_REQUESTS).map_into_right_body(),
                        ))
                    }
                    LimitationError::Client(e) => {
                        log::error!("Client request failed, redis error: {}", e);

                        Ok(req.into_response(
                            HttpResponse::new(StatusCode::INTERNAL_SERVER_ERROR)
                                .map_into_right_body(),
                        ))
                    }
                    _ => {
                        log::error!("Count failed: {}", err);

                        Ok(req.into_response(
                            HttpResponse::new(StatusCode::INTERNAL_SERVER_ERROR)
                                .map_into_right_body(),
                        ))
                    }
                }
            } else {
                service
                    .call(req)
                    .await
                    .map(ServiceResponse::map_into_left_body)
            }
        })
    }
}

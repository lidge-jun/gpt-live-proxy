//! Official Realtime REST relay.

use axum::body::Body;
use axum::extract::{OriginalUri, State};
use axum::response::{IntoResponse, Response};
use http::{HeaderMap, Method, Uri};

use crate::app::AppState;
use crate::error::RelayError;
use crate::live::call_create::RequestPath;
use crate::realtime::capability::{support, Capability, ProfileKind, Support};
use crate::realtime::contract::{classify_rest, ApiDialect, RouteFacts};
use crate::realtime::path::RestOperation;
use crate::relay::body::read_capped;
use crate::relay::http::{
    begin_exchange, spawn_execute, ExchangeLifecycle, ExchangeTerminal, OpaqueResponse,
};

/// Relay one of the ten literal official Realtime REST routes.
pub async fn handle(
    State(state): State<AppState>,
    method: Method,
    OriginalUri(original_uri): OriginalUri,
    request_headers: HeaderMap,
    request_body: Body,
) -> Response {
    let path = original_uri.path();
    let query = [];
    let content_type = request_headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok());
    let openai_alpha = request_headers
        .get("openai-alpha")
        .and_then(|value| value.to_str().ok());
    let facts = RouteFacts {
        method: &method,
        path,
        query: &query,
        content_type,
        openai_alpha,
        credential_mode: state.config.upstream.credential_mode(),
    };

    let classified = match classify_rest(&facts) {
        Ok(classified) => classified,
        Err(error) => {
            return RelayError::from_rest_contract(error, &method, path).into_response();
        }
    };

    let capability = Capability::from_rest(&classified);
    let profile = ProfileKind::from_profile(&state.config.upstream);
    let decision = support(profile, capability);
    if classified.selection.dialect == ApiDialect::OfficialGa {
        if let Support::Unsupported { required_profiles } = decision {
            return RelayError::unsupported_capability(capability, profile, required_profiles)
                .into_response();
        }
    }

    // Validate protocol metadata before private capability policy, then build
    // credentials only after policy accepts the request.
    let private_validated = if classified.selection.dialect == ApiDialect::OfficialGa {
        if let Err(error) = crate::realtime::headers::validate_upstream_headers(
            &request_headers,
            &classified.selection,
        ) {
            return error.into_response();
        }
        None
    } else {
        match crate::live::headers::validate_private_call_headers(
            &request_headers,
            &classified.selection,
        ) {
            Ok(validated) => Some(validated),
            Err(error) => return error.into_response(),
        }
    };
    if classified.selection.dialect != ApiDialect::OfficialGa {
        if let Support::Unsupported { required_profiles } = decision {
            return RelayError::unsupported_capability(capability, profile, required_profiles)
                .into_response();
        }
    }

    let upstream_headers = match match private_validated {
        Some(validated) => crate::live::headers::build_private_call_headers(
            &request_headers,
            &state.config.upstream,
            validated,
        ),
        None => crate::realtime::headers::build_upstream_headers(
            &request_headers,
            &state.config.upstream,
            &classified.selection,
        ),
    } {
        Ok(headers) => headers,
        Err(error) => return error.into_response(),
    };

    if classified.selection.dialect != ApiDialect::OfficialGa {
        debug_assert!(matches!(&classified.operation, RestOperation::CreateCall));
        return crate::live::handle_call_create(
            State(state),
            method,
            RequestPath::from(path.to_string()),
            classified.selection,
            upstream_headers,
            request_body,
        )
        .await;
    }

    let (lifecycle, _guard) = begin_exchange();
    let permit = match state.active_requests.clone().try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            lifecycle.finish(ExchangeTerminal::Failed);
            return RelayError::TooManyActiveRealtimeRequests.into_response();
        }
    };

    let body_bytes = match tokio::time::timeout(
        state.config.limits.request_read_timeout,
        read_capped(request_body, state.config.limits.request_bytes),
    )
    .await
    {
        Ok(Ok(body)) => body,
        Ok(Err(error)) => return finish_error(&lifecycle, error),
        Err(_) => return finish_error(&lifecycle, RelayError::RealtimeRequestBodyTimeout),
    };

    let target = match official_url(state.config.upstream.base_url(), &original_uri) {
        Ok(target) => target,
        Err(error) => return finish_error(&lifecycle, error),
    };
    let request = match state
        .http
        .request(method, target)
        .headers(upstream_headers)
        .body(body_bytes)
        .build()
    {
        Ok(request) => request,
        Err(error) => {
            return finish_error(&lifecycle, RelayError::UpstreamFailed(error.to_string()));
        }
    };

    let task = spawn_execute(
        state.http.clone(),
        request,
        state.config.limits.response_bytes,
        state.config.limits.upstream_timeout,
        lifecycle.clone(),
        permit,
    );
    match task.await {
        Ok(Ok(response)) => relay_response(response),
        Ok(Err(error)) => error.into_response(),
        Err(error) => {
            lifecycle.finish(ExchangeTerminal::Failed);
            RelayError::UpstreamFailed(format!("upstream exchange task failed: {error}"))
                .into_response()
        }
    }
}

/// Join a validated upstream base to the inbound raw path-and-query.
pub fn official_url(base: &str, original: &Uri) -> Result<String, RelayError> {
    let base = base.trim_end_matches('/');
    let inbound = original
        .path_and_query()
        .map(|value| value.as_str())
        .unwrap_or("/");
    let suffix = if base.ends_with("/v1") {
        inbound.strip_prefix("/v1").ok_or_else(|| {
            RelayError::UpstreamFailed("official Realtime path is outside /v1".to_string())
        })?
    } else {
        inbound
    };
    Ok(format!("{base}{suffix}"))
}

fn finish_error(lifecycle: &ExchangeLifecycle, error: RelayError) -> Response {
    let terminal = match &error {
        RelayError::ClientCanceled => ExchangeTerminal::Canceled,
        RelayError::RealtimeRequestBodyTimeout | RelayError::UpstreamTimeout => {
            ExchangeTerminal::TimedOut
        }
        _ => ExchangeTerminal::Failed,
    };
    lifecycle.finish(terminal);
    error.into_response()
}

fn relay_response(upstream: OpaqueResponse) -> Response {
    let mut response = Response::new(Body::from(upstream.body));
    *response.status_mut() = upstream.status;
    *response.headers_mut() = crate::realtime::headers::response_headers(&upstream.headers);
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn official_url_preserves_raw_path_and_query() {
        let uri: Uri = "/v1/realtime/calls?dup=1&blank=&plus=+&encoded=%2B&utf8=%ED%95%9C&dup=2"
            .parse()
            .unwrap();
        let raw = uri.path_and_query().unwrap().as_str();

        assert_eq!(
            official_url("https://api.openai.com", &uri).unwrap(),
            format!("https://api.openai.com{raw}")
        );
        assert_eq!(
            official_url("https://api.openai.com/v1/", &uri).unwrap(),
            "https://api.openai.com/v1/realtime/calls?dup=1&blank=&plus=+&encoded=%2B&utf8=%ED%95%9C&dup=2"
        );
        assert_eq!(
            official_url("https://proxy.test/custom/", &uri).unwrap(),
            format!("https://proxy.test/custom{raw}")
        );
    }

    #[test]
    fn official_url_has_no_query_when_inbound_has_none() {
        let uri: Uri = "/v1/realtime/sessions".parse().unwrap();
        assert_eq!(
            official_url("https://api.openai.com/v1", &uri).unwrap(),
            "https://api.openai.com/v1/realtime/sessions"
        );
    }
}

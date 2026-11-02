mod chat_completions;
pub(crate) mod model_list;

use std::{ops::Range, time::Duration};

use axum::{
    body::{Body, Bytes},
    http::{HeaderMap, HeaderName, HeaderValue, Response, StatusCode, header},
};
use reqwest::Client;
use reqwest_middleware::ClientWithMiddleware;
use reqwest_tracing::DisableOtelPropagation;
use tokio::time::Instant;

pub use crate::transport::ProviderError;
use crate::{
    capture::{ExecutionCapture, RelayOutcome},
    config::ExecutionLimits,
    correlation::{RequestCorrelation, UpstreamTarget},
    observability::{ErrorOrigin, RequestObservation, Wait, provider_status_reason},
    resolution::{
        ApiFormat, IngestionMode, ProviderConnection, ProviderCredential, ResolvedRequestContext,
    },
    transport,
};

#[derive(Clone, Copy)]
pub(crate) enum Route {
    OpenAiResponses,
    OpenAiResponsesCompact,
    OpenAiChatCompletions,
    AnthropicMessages,
    AnthropicCountTokens,
}

impl Route {
    pub(crate) fn api_format(self) -> ApiFormat {
        match self {
            Self::OpenAiChatCompletions => ApiFormat::OpenAiChatCompletions,
            Self::OpenAiResponses | Self::OpenAiResponsesCompact => ApiFormat::OpenAiResponses,
            Self::AnthropicMessages | Self::AnthropicCountTokens => ApiFormat::AnthropicMessages,
        }
    }

    fn path(self) -> &'static str {
        match self {
            Self::OpenAiResponses => "/responses",
            Self::OpenAiResponsesCompact => "/responses/compact",
            Self::OpenAiChatCompletions => "/chat/completions",
            Self::AnthropicMessages => "/messages",
            Self::AnthropicCountTokens => "/messages/count_tokens",
        }
    }

    pub(crate) fn captures_generation(self) -> bool {
        matches!(
            self,
            Self::OpenAiResponses
                | Self::OpenAiResponsesCompact
                | Self::OpenAiChatCompletions
                | Self::AnthropicMessages
        )
    }
}

#[derive(Default)]
pub(crate) struct ProviderLimits {
    pub execution: ExecutionLimits,
}

/// An admitted execution: its deadline and in-flight accounting. Memory is reserved
/// separately, before the body is read.
pub struct RequestPermit {
    _active: crate::observability::Active,
    deadline: Instant,
}

/// The validated caller body and the facts the relay needs about it.
pub(crate) struct RequestBody {
    pub bytes: Bytes,
    pub model_range: Range<usize>,
    /// The caller asked for a streamed response (`"stream": true`).
    pub streaming: bool,
    /// Capacity held for the body, released with the execution once the response ends.
    pub reservation: Option<BodyReservation>,
}

pub(crate) type BodyReservation = Box<dyn Send + Sync>;

pub struct ProviderTransport {
    client: ClientWithMiddleware,
    limits: ProviderLimits,
    telemetry: Option<crate::telemetry::Telemetry>,
    /// Gate for IP-literal origins, which hyper-util connects without
    /// consulting the DNS resolver.
    egress: Option<crate::egress::EgressPolicy>,
    /// Per-connection breaker state, consulted before each fallback attempt.
    breakers: crate::breaker::CircuitBreakers,
    /// Test-only: the URL tests use instead of each connection's `base_url`,
    /// which points at the real provider origin.
    #[cfg(test)]
    test_base_url: Option<String>,
}

impl ProviderTransport {
    /// # Errors
    /// Returns [`ProviderError::Configuration`] when the HTTPS client cannot be initialized.
    pub fn new(execution: ExecutionLimits) -> Result<Self, ProviderError> {
        Self::with_limits(ProviderLimits { execution })
    }

    fn with_limits(limits: ProviderLimits) -> Result<Self, ProviderError> {
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .no_proxy()
            .no_gzip()
            .no_brotli()
            .no_deflate()
            .no_zstd()
            // No client read timeout: it would also cut a non-streaming call while the
            // provider generates before sending headers. The relay bounds stalls instead.
            .connect_timeout(Duration::from_secs(5))
            .pool_max_idle_per_host(16)
            // reqwest's http2 feature is for Web-bound clients (resolver, telemetry); provider calls stay on HTTP/1.1.
            .http1_only()
            .build()
            .map_err(|_| ProviderError::Configuration)?;
        Ok(Self {
            client: crate::observability::instrument_client(client, "provider.headers"),
            limits,
            telemetry: None,
            egress: None,
            breakers: crate::breaker::CircuitBreakers::default(),
            #[cfg(test)]
            test_base_url: None,
        })
    }

    /// Build a transport whose client resolves DNS through `resolver` and whose
    /// request path gates IP-literal origins through `egress`.
    pub(crate) fn with_egress(
        egress: crate::egress::EgressPolicy,
        limits: ProviderLimits,
    ) -> Result<Self, ProviderError> {
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .no_proxy()
            .no_gzip()
            .no_brotli()
            .no_deflate()
            .no_zstd()
            // No client read timeout: it would also cut a non-streaming call while the
            // provider generates before sending headers. The relay bounds stalls instead.
            .connect_timeout(Duration::from_secs(5))
            .pool_max_idle_per_host(16)
            // reqwest's http2 feature is for Web-bound clients (resolver, telemetry); provider calls stay on HTTP/1.1.
            .http1_only()
            .dns_resolver(crate::egress::EgressResolver::new(egress.clone()))
            .build()
            .map_err(|_| ProviderError::Configuration)?;
        Ok(Self {
            client: crate::observability::instrument_client(client, "provider.headers"),
            limits,
            telemetry: None,
            egress: Some(egress),
            breakers: crate::breaker::CircuitBreakers::default(),
            #[cfg(test)]
            test_base_url: None,
        })
    }

    pub(crate) fn with_telemetry(mut self, telemetry: crate::telemetry::Telemetry) -> Self {
        self.telemetry = Some(telemetry);
        self
    }

    #[cfg(test)]
    pub(crate) fn for_test(base_url: String, limits: ProviderLimits) -> Self {
        let mut provider = Self::with_limits(limits).unwrap();
        provider.test_base_url = Some(base_url);
        provider
    }

    /// Start the execution deadline.
    pub fn admit(&self) -> RequestPermit {
        RequestPermit {
            _active: crate::observability::Active::new("execution"),
            deadline: Instant::now() + self.limits.execution.max_execution_time,
        }
    }

    /// Forward as a streamed `OpenAI` Responses call.
    ///
    /// # Errors
    /// See [`Self::forward_route`].
    #[cfg(test)]
    pub async fn forward(
        &self,
        permit: RequestPermit,
        context: ResolvedRequestContext,
        headers: &HeaderMap,
        body: Bytes,
    ) -> Result<Response<Body>, ProviderError> {
        let model_range = model_range(&body)?;
        self.forward_route(
            permit,
            context,
            headers,
            RequestBody {
                bytes: body,
                model_range,
                streaming: true,
                reservation: None,
            },
            Route::OpenAiResponses,
            &mut RequestCorrelation::from_headers(headers),
        )
        .await
    }

    pub(crate) async fn forward_route(
        &self,
        permit: RequestPermit,
        context: ResolvedRequestContext,
        headers: &HeaderMap,
        request: RequestBody,
        route: Route,
        correlation: &mut RequestCorrelation,
    ) -> Result<Response<Body>, ProviderError> {
        let RequestBody {
            bytes: body,
            model_range,
            streaming,
            reservation,
        } = request;
        let api_format = route.api_format();
        let mut capture = if route.captures_generation() {
            let mut capture = ExecutionCapture::for_request(api_format, &context, headers, &body);
            if let Some(telemetry) = &self.telemetry
                && context.ingestion_mode() != IngestionMode::None
            {
                capture.deliver_to(telemetry.clone(), &context, headers, correlation);
            }
            capture
        } else {
            ExecutionCapture::unobserved()
        };
        // The model rewrite depends on the target's provider_model, so it is
        // recomputed per attempt from the caller's original body.
        let original_body = body;
        let original_model_range = model_range;
        // The primary connection, then the alias's fallbacks in order.
        let targets = std::iter::once(context.connection())
            .chain(context.fallbacks().iter())
            .collect::<Vec<_>>();
        let mut last_error = None;
        for (index, target) in targets.iter().enumerate() {
            if index > 0 {
                tracing::Span::current().record("gateway.attempt", index as u64);
            }
            // An open breaker skips the target; a half-open one admits a probe.
            if !self.breakers.allows(target.id()) {
                last_error = Some(ProviderError::Transport);
                record_failed_attempt(target, "circuit_open");
                continue;
            }
            let credential = provider_credential(target)?;
            let provider_model = target.provider_model();
            let body = Self::rewrite_body_for_target(
                route,
                &original_body,
                &original_model_range,
                provider_model,
            )?;
            capture.set_provider_model(provider_model);
            let base_url = target.base_url();
            if let Err(error) = self.enforce_egress(base_url) {
                last_error = Some(error);
                record_failed_attempt(target, "blocked");
                continue;
            }
            let upstream_target =
                UpstreamTarget::for_connection(target, Some(provider_model.to_owned()));
            correlation.record_upstream(upstream_target);
            let upstream = self.build_upstream_request(credential, headers, route, base_url, body);
            let started = Instant::now();
            let response = self
                .send_until_headers(upstream, streaming, route, permit.deadline)
                .await;
            let observation = correlation.observation();
            observation.record_wait(Wait::ProviderHeaders, started.elapsed());
            // Ownership moves only on a terminal outcome; a failed-over attempt
            // leaves permit, context, reservation and capture in the loop.
            let attempt = handle_attempt_response(
                response,
                target,
                index + 1 < targets.len(),
                observation,
                api_format,
                &self.breakers,
                &mut capture,
            );
            match attempt {
                AttemptOutcome::FailOver(error) => {
                    last_error = Some(error);
                }
                AttemptOutcome::Relay(DownstreamResponse {
                    mut response,
                    upstream_response,
                }) => {
                    *response.body_mut() = transport::relay(
                        upstream_response,
                        api_format.provider().as_str(),
                        permit.deadline,
                        self.limits.execution.stall_timeout,
                        (permit, context, reservation),
                        capture,
                    );
                    return Ok(response);
                }
                AttemptOutcome::Done(result) => return result,
            }
        }
        Err(last_error.unwrap_or(ProviderError::Transport))
    }

    /// The model rewrite depends on the target's `provider_model`, so it is
    /// recomputed per attempt from the caller's original body.
    fn rewrite_body_for_target(
        route: Route,
        original_body: &Bytes,
        original_model_range: &Range<usize>,
        provider_model: &str,
    ) -> Result<Bytes, ProviderError> {
        match route {
            Route::OpenAiChatCompletions => {
                match chat_completions::request_stream_usage(original_body, provider_model) {
                    Some(rewritten) => Ok(rewritten),
                    None => rewrite_model(
                        original_body.clone(),
                        original_model_range.clone(),
                        provider_model,
                    ),
                }
            }
            _ => rewrite_model(
                original_body.clone(),
                original_model_range.clone(),
                provider_model,
            ),
        }
    }

    /// Build the POST for one attempt: the namespace path under the target's
    /// origin, sanitized headers, identity encoding, and the target's own
    /// credential. A keyless custom connection's credential is None: no header
    /// at all.
    fn build_upstream_request(
        &self,
        credential: Option<(HeaderName, HeaderValue)>,
        headers: &HeaderMap,
        route: Route,
        base_url: &str,
        body: Bytes,
    ) -> reqwest_middleware::RequestBuilder {
        let upstream = self
            .client
            .post(self.request_url(route, base_url))
            .with_extension(DisableOtelPropagation)
            .headers(transport::request_headers(headers, route.api_format()))
            .header(header::ACCEPT_ENCODING, "identity");
        let upstream = if let Some((name, value)) = credential {
            upstream.header(name, value)
        } else {
            upstream
        };
        upstream.body(body)
    }

    /// Send the built request and wait for its response headers. A stream, or
    /// a call that does not generate, that goes quiet before its headers is
    /// stalled; a non-streamed generation sends nothing until done. This is
    /// the retryable unit of an attempt: once headers arrive, the response is
    /// relayed and fallback is no longer possible.
    async fn send_until_headers(
        &self,
        request: reqwest_middleware::RequestBuilder,
        streaming: bool,
        route: Route,
        deadline: Instant,
    ) -> Result<reqwest::Response, ProviderError> {
        // A stream, or a call that does not generate, that goes quiet before
        // its headers is stalled; a non-streamed generation sends nothing
        // until done.
        let stall_deadline = Instant::now() + self.limits.execution.stall_timeout;
        let headers_deadline = if streaming || !route.captures_generation() {
            deadline.min(stall_deadline)
        } else {
            deadline
        };
        tokio::time::timeout_at(headers_deadline, request.send())
            .await
            .map_err(|_| ProviderError::Timeout)
            .and_then(|result| {
                result.map_err(|error| {
                    if error.is_timeout() {
                        ProviderError::Timeout
                    } else {
                        ProviderError::Transport
                    }
                })
            })
    }

    /// Relay a gateway-built response body under the execution deadlines, holding
    /// `owner` until the client has read it or the relay ends.
    pub(crate) fn relay_buffered<T: Send + 'static>(&self, body: Vec<u8>, owner: T) -> Body {
        transport::relay_buffered(
            body,
            Instant::now() + self.limits.execution.max_execution_time,
            self.limits.execution.stall_timeout,
            owner,
        )
    }

    /// The upstream URL for `route`: the connection's origin, with the trailing
    /// slash trimmed so a Web-supplied origin never produces a double slash.
    // Tests route the transport at a fake provider through `test_base_url`;
    // production builds derive the origin from the connection alone.
    #[cfg_attr(
        not(test),
        expect(
            clippy::unused_self,
            reason = "tests override the origin through the transport; production derives it from the connection"
        )
    )]
    fn request_url(&self, route: Route, base_url: &str) -> String {
        #[cfg(test)]
        let base = self.test_base_url.as_deref().unwrap_or(base_url);
        #[cfg(not(test))]
        let base = base_url;
        let base = base.trim_end_matches('/');
        let mut url = String::with_capacity(base.len() + route.path().len());
        url.push_str(base);
        url.push_str(route.path());
        url
    }

    /// Gate the origin before any connection attempt. hyper-util connects
    /// IP-literal hosts without consulting the DNS resolver, so literals take
    /// the policy check here; resolved names are filtered by the resolver.
    ///
    /// # Errors
    /// Returns [`ProviderError::Blocked`] when the policy forbids the origin.
    fn enforce_egress(&self, base_url: &str) -> Result<(), ProviderError> {
        let Some(egress) = &self.egress else {
            return Ok(());
        };
        if let Some(host) = literal_host(base_url)
            && !egress.allows_literal(host)
        {
            return Err(ProviderError::Blocked);
        }
        Ok(())
    }
}

/// A failed attempt is logged with its connection so operators can see
/// failover in the gateway logs; the span keeps the last target.
fn record_failed_attempt(target: &ProviderConnection, reason: &'static str) {
    tracing::warn!(
        reason,
        connection_id = target.id(),
        "upstream attempt failed; failing over to the next target"
    );
}

/// What one upstream attempt decided: fail over to the next target, relay the
/// response downstream (headers were accepted), or hand the result back.
enum AttemptOutcome {
    FailOver(ProviderError),
    Relay(DownstreamResponse),
    Done(Result<Response<Body>, ProviderError>),
}

/// A response whose headers were accepted: relay moves ownership of the
/// execution from the attempt loop into the stream.
struct DownstreamResponse {
    response: Response<Body>,
    upstream_response: reqwest::Response,
}

/// Classify one attempt's response. Failover happens only before response
/// headers are sent downstream: connect errors, first-byte timeouts, 429 and
/// 5xx. A caller error (4xx) is returned unchanged; once [`transport::relay`]
/// owns the response the stream is terminal, so fallback is impossible. The
/// capture survives a failed attempt: only its per-attempt facts are recorded.
fn handle_attempt_response(
    response: Result<reqwest::Response, ProviderError>,
    target: &ProviderConnection,
    has_next_target: bool,
    observation: &RequestObservation,
    api_format: ApiFormat,
    breakers: &crate::breaker::CircuitBreakers,
    capture: &mut ExecutionCapture,
) -> AttemptOutcome {
    match response {
        Ok(response) => {
            if let Some(request_id) = transport::provider_request_id(response.headers(), api_format)
            {
                tracing::Span::current().record("provider_request_id", request_id);
            }
            let status = response.status();
            capture.record_response(status.as_u16(), response.headers());
            record_provider_status(observation, status);
            // Fail over on rate limiting and upstream faults, but hand a caller
            // error back unchanged: retrying it cannot help.
            let failover = status.as_u16() == 429 || status.is_server_error();
            if failover && has_next_target {
                breakers.record_failure(target.id());
                record_failed_attempt(
                    target,
                    if status.as_u16() == 429 {
                        "rate_limited"
                    } else {
                        "server_error"
                    },
                );
                return AttemptOutcome::FailOver(ProviderError::Transport);
            }
            breakers.record_success(target.id());
            let mut downstream = Response::new(Body::empty());
            *downstream.status_mut() = status;
            *downstream.headers_mut() = transport::response_headers(response.headers(), api_format);
            AttemptOutcome::Relay(DownstreamResponse {
                response: downstream,
                upstream_response: response,
            })
        }
        Err(error) => {
            capture.finish(if matches!(error, ProviderError::Timeout) {
                RelayOutcome::Timeout
            } else {
                RelayOutcome::TransportError
            });
            if has_next_target && matches!(error, ProviderError::Transport | ProviderError::Timeout)
            {
                breakers.record_failure(target.id());
                record_failed_attempt(
                    target,
                    if matches!(error, ProviderError::Timeout) {
                        "timeout"
                    } else {
                        "connect_error"
                    },
                );
                return AttemptOutcome::FailOver(error);
            }
            AttemptOutcome::Done(Err(error))
        }
    }
}

/// The host of `base_url` when it is an IP literal, without brackets.
fn literal_host(base_url: &str) -> Option<&str> {
    let rest = base_url.split_once("://")?.1;
    let authority = rest.split(['/', '?', '#']).next()?;
    let host = authority
        .rsplit_once(':')
        .map_or(authority, |(host, _)| host);
    let host = host.trim_matches(['[', ']']);
    host.parse::<std::net::IpAddr>().is_ok().then_some(host)
}

/// Provider error statuses are relayed unchanged, so the handler's rejection log
/// and error classification never see them.
fn record_provider_status(observation: &RequestObservation, status: StatusCode) {
    if !(status.is_client_error() || status.is_server_error()) {
        return;
    }
    let reason = provider_status_reason(status.as_u16());
    observation.record_error(ErrorOrigin::Provider, reason);
    if status.is_server_error() {
        tracing::warn!(
            reason,
            status = status.as_u16(),
            "provider returned an error status"
        );
    } else {
        tracing::info!(
            reason,
            status = status.as_u16(),
            "provider returned an error status"
        );
    }
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "ownership releases the caller body when rewriting returns, before the upstream wait"
)]
fn rewrite_model(
    body: Bytes,
    model_range: Range<usize>,
    provider_model: &str,
) -> Result<Bytes, ProviderError> {
    if model_range.start > model_range.end || model_range.end > body.len() {
        return Err(ProviderError::Configuration);
    }
    let model = serde_json::to_vec(provider_model).map_err(|_| ProviderError::Configuration)?;
    let capacity = body.len() - model_range.len() + model.len();
    let mut rewritten = Vec::with_capacity(capacity);
    rewritten.extend_from_slice(&body[..model_range.start]);
    rewritten.extend_from_slice(&model);
    rewritten.extend_from_slice(&body[model_range.end..]);
    Ok(Bytes::from(rewritten))
}

#[cfg(test)]
pub(crate) fn model_range(body: &[u8]) -> Result<Range<usize>, ProviderError> {
    #[derive(serde::Deserialize)]
    struct ModelMember<'a> {
        #[serde(borrow)]
        model: &'a serde_json::value::RawValue,
    }

    let request = serde_json::from_slice::<ModelMember<'_>>(body)
        .map_err(|_| ProviderError::Configuration)?;
    let model = request.model.get();
    let start = model
        .as_ptr()
        .addr()
        .checked_sub(body.as_ptr().addr())
        .ok_or(ProviderError::Configuration)?;
    let end = start
        .checked_add(model.len())
        .filter(|end| *end <= body.len())
        .ok_or(ProviderError::Configuration)?;
    Ok(start..end)
}

/// The credential header. A keyless custom connection's empty token sends no
/// credential header at all.
fn provider_credential(
    connection: &ProviderConnection,
) -> Result<Option<(HeaderName, HeaderValue)>, ProviderError> {
    let (name, value) = match connection.credential() {
        // An empty Bearer token means the connection carries no credential.
        ProviderCredential::Bearer("") => return Ok(None),
        ProviderCredential::Bearer(token) => (header::AUTHORIZATION, format!("Bearer {token}")),
        ProviderCredential::XApiKey(key) => (HeaderName::from_static("x-api-key"), key.to_owned()),
    };
    let value = HeaderValue::from_str(&value).map_err(|_| ProviderError::Configuration)?;
    Ok(Some((name, value)))
}

#[cfg(test)]
mod tests;

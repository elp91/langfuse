use super::{ResolutionError, valid_optional_token, valid_token};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use reqwest::Url;
use secrecy::{ExposeSecret, SecretString};
use serde::{
    Deserialize, Deserializer, Serialize,
    de::{DeserializeOwned, IntoDeserializer},
};
use std::{collections::BTreeMap, fmt};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub enum ApiFormat {
    #[serde(rename = "openai.responses")]
    OpenAiResponses,
    #[serde(rename = "openai.chat-completions")]
    OpenAiChatCompletions,
    #[serde(rename = "anthropic.messages")]
    AnthropicMessages,
}

impl ApiFormat {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OpenAiResponses => "openai.responses",
            Self::OpenAiChatCompletions => "openai.chat-completions",
            Self::AnthropicMessages => "anthropic.messages",
        }
    }

    pub fn provider(self) -> Provider {
        match self {
            Self::OpenAiResponses | Self::OpenAiChatCompletions => Provider::OpenAi,
            Self::AnthropicMessages => Provider::Anthropic,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    OpenAi,
    Anthropic,
    Custom,
}

impl Provider {
    /// The stable lowercase provider type, as Web sends it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OpenAi => "openai",
            Self::Anthropic => "anthropic",
            Self::Custom => "custom",
        }
    }

    fn supports(self, api_format: ApiFormat) -> bool {
        match self {
            Self::OpenAi => matches!(
                api_format,
                ApiFormat::OpenAiResponses | ApiFormat::OpenAiChatCompletions
            ),
            Self::Anthropic => matches!(api_format, ApiFormat::AnthropicMessages),
            // Whether an OpenAI-compatible host serves Responses or Messages is
            // per connection; Web's route probe records it, so the gateway
            // accepts all three formats and lets the upstream answer.
            Self::Custom => matches!(
                api_format,
                ApiFormat::OpenAiResponses
                    | ApiFormat::OpenAiChatCompletions
                    | ApiFormat::AnthropicMessages
            ),
        }
    }

    fn accepts(self, credential: ProviderCredential<'_>) -> bool {
        match self {
            // vLLM, SGLang, and OpenAI-compatible servers authenticate with
            // Bearer; Web sends Bearer even when the credential is optional.
            Self::OpenAi | Self::Custom => matches!(credential, ProviderCredential::Bearer(_)),
            Self::Anthropic => matches!(credential, ProviderCredential::XApiKey(_)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IngestionMode {
    /// Route only; nothing is delivered to Langfuse.
    None,
    Usage,
    Full,
}

impl IngestionMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Usage => "usage",
            Self::Full => "full",
        }
    }
}

#[derive(Clone, Deserialize)]
#[serde(untagged)]
pub enum MetadataValue {
    String(String),
    Number(serde_json::Number),
    Bool(bool),
}

/// A borrowed, exposed view of the stored provider secret, for building the
/// upstream header. It deliberately implements neither `Debug` nor `Display`.
#[derive(Clone, Copy)]
pub enum ProviderCredential<'a> {
    /// `Authorization: Bearer <token>`
    Bearer(&'a str),
    /// `x-api-key: <key>`
    XApiKey(&'a str),
}

impl<'a> ProviderCredential<'a> {
    fn secret(self) -> &'a str {
        match self {
            Self::Bearer(token) | Self::XApiKey(token) => token,
        }
    }
}

#[derive(Deserialize)]
enum BearerType {
    Bearer,
}

#[derive(Deserialize)]
enum XApiKeyType {
    #[serde(rename = "x-api-key")]
    XApiKey,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BearerAuth {
    #[serde(rename = "type", deserialize_with = "string_enum")]
    _token_type: BearerType,
    token: SecretString,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct XApiKeyAuth {
    #[serde(rename = "type", deserialize_with = "string_enum")]
    _kind: XApiKeyType,
    #[serde(rename = "header", deserialize_with = "string_enum")]
    _header: XApiKeyType,
    value: SecretString,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Auth {
    Bearer(BearerAuth),
    XApiKey(XApiKeyAuth),
}

impl Auth {
    fn credential(&self) -> ProviderCredential<'_> {
        match self {
            Self::Bearer(auth) => ProviderCredential::Bearer(auth.token.expose_secret()),
            Self::XApiKey(auth) => ProviderCredential::XApiKey(auth.value.expose_secret()),
        }
    }
}

#[derive(Deserialize)]
pub struct ProviderConnection {
    id: String,
    // Optional so the gateway keeps resolving against a Web that predates it.
    #[serde(default)]
    name: Option<String>,
    #[serde(deserialize_with = "string_enum")]
    provider: Provider,
    provider_model: String,
    #[serde(deserialize_with = "string_enum")]
    api_format: ApiFormat,
    base_url: String,
    auth: Auth,
}

impl ProviderConnection {
    pub fn id(&self) -> &str {
        &self.id
    }
    /// The user-chosen display name. Never send it to callers; it is for operators.
    pub fn name(&self) -> Option<&str> {
        non_blank(self.name.as_deref())
    }
    pub fn provider(&self) -> Provider {
        self.provider
    }
    pub fn provider_model(&self) -> &str {
        &self.provider_model
    }
    pub fn api_format(&self) -> ApiFormat {
        self.api_format
    }
    pub fn base_url(&self) -> &str {
        &self.base_url
    }
    pub fn credential(&self) -> ProviderCredential<'_> {
        self.auth.credential()
    }
}

/// A Langfuse organization ID, typed apart from [`ProjectId`] so the tenant pair
/// cannot be swapped.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
#[serde(transparent)]
pub struct OrganizationId(String);

impl OrganizationId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A Langfuse project ID; telemetry batches and ingestion grants are per project.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
#[serde(transparent)]
pub struct ProjectId(String);

impl ProjectId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
impl From<&str> for ProjectId {
    fn from(id: &str) -> Self {
        Self(id.to_owned())
    }
}

#[derive(Deserialize)]
pub struct RequestAttribution {
    organization_id: OrganizationId,
    /// Absent only in [`IngestionMode::None`], which needs no ingestion project.
    project_id: Option<ProjectId>,
    key_id: String,
    /// The user the key is bound to, which is the key's owner. Absent for keys
    /// created before keys were bound to users, and from Web versions that
    /// predate the field.
    #[serde(default)]
    user_id: Option<String>,
    #[serde(default)]
    key_name: Option<String>,
    #[serde(default)]
    user_name: Option<String>,
    #[serde(default)]
    user_email: Option<String>,
    key_metadata: BTreeMap<String, MetadataValue>,
    provider_connection_id: String,
}

impl RequestAttribution {
    pub fn organization_id(&self) -> &OrganizationId {
        &self.organization_id
    }
    pub fn project_id(&self) -> Option<&ProjectId> {
        self.project_id.as_ref()
    }
    pub fn key_id(&self) -> &str {
        &self.key_id
    }
    pub fn user_id(&self) -> Option<&str> {
        self.user_id.as_deref()
    }
    pub fn key_name(&self) -> Option<&str> {
        non_blank(self.key_name.as_deref())
    }
    pub fn user_name(&self) -> Option<&str> {
        non_blank(self.user_name.as_deref())
    }
    pub fn user_email(&self) -> Option<&str> {
        non_blank(self.user_email.as_deref())
    }
    pub fn key_metadata(&self) -> &BTreeMap<String, MetadataValue> {
        &self.key_metadata
    }
    pub fn provider_connection_id(&self) -> &str {
        &self.provider_connection_id
    }
}

#[derive(Deserialize)]
pub struct IngestionGrant {
    access_token: SecretString,
    #[serde(rename = "token_type", deserialize_with = "string_enum")]
    _token_type: BearerType,
    expires_at: u64,
}

impl IngestionGrant {
    pub fn access_token(&self) -> &SecretString {
        &self.access_token
    }
    pub fn expires_at(&self) -> u64 {
        self.expires_at
    }
}

#[derive(Deserialize)]
struct ResolutionResponse {
    version: u8,
    connection: ProviderConnection,
    /// Lower-priority targets for the same model, in failover order. Absent or
    /// empty keeps single-connection behavior; Web only sends it for aliases.
    #[serde(default)]
    fallbacks: Vec<ProviderConnection>,
    attribution: RequestAttribution,
    #[serde(deserialize_with = "string_enum")]
    ingestion_mode: IngestionMode,
    /// Absent only in [`IngestionMode::None`]; nothing is delivered without it.
    ingestion: Option<IngestionGrant>,
}

/// A validated v1 execution. Construction is restricted to successful resolution.
pub struct ResolvedRequestContext(ResolutionResponse);

fn non_blank(value: Option<&str>) -> Option<&str> {
    value.filter(|value| !value.trim().is_empty())
}

// Web enums are JSON strings. Serde's externally tagged enums also accept objects.
fn string_enum<'de, D: Deserializer<'de>, T: DeserializeOwned>(
    deserializer: D,
) -> Result<T, D::Error> {
    T::deserialize(String::deserialize(deserializer)?.into_deserializer())
}

impl ResolvedRequestContext {
    pub fn connection(&self) -> &ProviderConnection {
        &self.0.connection
    }
    /// Ordered fallback targets for the same model, primary first. Empty when
    /// Web resolved a plain connection.
    pub fn fallbacks(&self) -> &[ProviderConnection] {
        &self.0.fallbacks
    }
    pub fn attribution(&self) -> &RequestAttribution {
        &self.0.attribution
    }
    pub fn ingestion_mode(&self) -> IngestionMode {
        self.0.ingestion_mode
    }
    pub fn ingestion(&self) -> Option<&IngestionGrant> {
        self.0.ingestion.as_ref()
    }
    /// The project and grant telemetry is delivered with. Resolution guarantees both
    /// in every mode but [`IngestionMode::None`], which never delivers.
    pub fn delivery_target(&self) -> Option<(&ProjectId, &IngestionGrant)> {
        if self.0.ingestion_mode == IngestionMode::None {
            return None;
        }
        Some((
            self.0.attribution.project_id.as_ref()?,
            self.0.ingestion.as_ref()?,
        ))
    }
}

impl fmt::Debug for ResolvedRequestContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResolvedRequestContext")
            .finish_non_exhaustive()
    }
}

#[derive(Deserialize)]
struct IngestionClaims {
    organization_id: OrganizationId,
    project_id: ProjectId,
}

fn ingestion_claims_match(token: &str, attribution: &RequestAttribution) -> bool {
    let mut segments = token.split('.');
    let (Some(_header), Some(payload), Some(_signature), None) = (
        segments.next(),
        segments.next(),
        segments.next(),
        segments.next(),
    ) else {
        return false;
    };
    URL_SAFE_NO_PAD
        .decode(payload)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<IngestionClaims>(&bytes).ok())
        .is_some_and(|claims| {
            claims.organization_id == attribution.organization_id
                && Some(&claims.project_id) == attribution.project_id.as_ref()
        })
}

/// A grant must be valid and bound to the attributed tenant whenever it is sent;
/// only [`IngestionMode::None`] may omit it.
fn valid_ingestion(response: &ResolutionResponse, now: u64) -> bool {
    match &response.ingestion {
        None => response.ingestion_mode == IngestionMode::None,
        Some(grant) => {
            let token = grant.access_token.expose_secret();
            valid_token(token)
                && ingestion_claims_match(token, &response.attribution)
                && grant.expires_at > now
        }
    }
}

pub(super) fn decode(
    bytes: &[u8],
    expected_format: ApiFormat,
    now: u64,
) -> Result<ResolvedRequestContext, ResolutionError> {
    let response: ResolutionResponse =
        serde_json::from_slice(bytes).map_err(|_| ResolutionError::InvalidResponse)?;
    let connection = &response.connection;
    let attribution = &response.attribution;
    if response.version != 1
        || connection.api_format != expected_format
        || !valid_target(connection, expected_format)
        || !valid_ingestion(&response, now)
        || [
            connection.id.as_str(),
            attribution.organization_id.as_str(),
            attribution.key_id.as_str(),
            attribution.provider_connection_id.as_str(),
        ]
        .into_iter()
        .chain(attribution.project_id.as_ref().map(ProjectId::as_str))
        .chain(attribution.user_id.as_deref())
        .any(|value| value.trim().is_empty())
        || connection.provider_model.trim().is_empty()
    {
        return Err(ResolutionError::InvalidResponse);
    }
    if response
        .fallbacks
        .iter()
        .any(|fallback| !valid_target(fallback, expected_format))
    {
        return Err(ResolutionError::InvalidResponse);
    }
    Ok(ResolvedRequestContext(response))
}

/// The per-target rules: format agreement, origin validity, and a credential
/// scheme the provider accepts. The primary connection additionally bounds the
/// attribution; fallbacks share it.
fn valid_target(connection: &ProviderConnection, expected_format: ApiFormat) -> bool {
    let provider = connection.provider;
    connection.api_format == expected_format
        && provider.supports(connection.api_format)
        && valid_connection_origin(&connection.base_url)
        // A custom connection may omit its key entirely; the other providers
        // always carry one.
        && match provider {
            // A custom connection may omit its key entirely; the other
            // providers always carry one.
            Provider::Custom => valid_optional_token(connection.credential()),
            _ => {
                provider.accepts(connection.credential())
                    && valid_token(connection.credential().secret())
            }
        }
        && !connection.provider_model.trim().is_empty()
}

/// The origin Web vouches for must be a syntactic http or https origin with no
/// extras; reachability is enforced at connect time by the egress policy.
fn valid_connection_origin(base_url: &str) -> bool {
    let Ok(url) = Url::parse(base_url) else {
        return false;
    };
    (url.scheme() == "https" || url.scheme() == "http")
        && url.host_str().is_some_and(|host| !host.is_empty())
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none()
}

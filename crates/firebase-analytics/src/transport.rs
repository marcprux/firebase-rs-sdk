use std::collections::BTreeMap;
use std::time::Duration;

use firebase_core::platform::http::{HttpClient, HttpRequest};
use serde::Serialize;
use url::Url;

use crate::error::{internal_error, invalid_argument, network_error, AnalyticsResult};

/// Configuration used to dispatch analytics events through the GA4 Measurement Protocol.
#[derive(Clone, Debug)]
pub struct MeasurementProtocolConfig {
    measurement_id: String,
    api_secret: String,
    endpoint: MeasurementProtocolEndpoint,
    timeout: Duration,
}

impl MeasurementProtocolConfig {
    pub fn new(measurement_id: impl Into<String>, api_secret: impl Into<String>) -> Self {
        Self {
            measurement_id: measurement_id.into(),
            api_secret: api_secret.into(),
            endpoint: MeasurementProtocolEndpoint::Collect,
            timeout: Duration::from_secs(10),
        }
    }

    pub fn with_endpoint(mut self, endpoint: MeasurementProtocolEndpoint) -> Self {
        self.endpoint = endpoint;
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// The per-request timeout. It applies in the browser too: the shared HTTP client times a
    /// request out itself rather than relying on the transport underneath.
    pub(crate) fn timeout(&self) -> Duration {
        self.timeout
    }

    pub(crate) fn measurement_id(&self) -> &str {
        &self.measurement_id
    }

    pub(crate) fn api_secret(&self) -> &str {
        &self.api_secret
    }
}

/// Supported endpoints for the Measurement Protocol.
#[derive(Clone, Debug)]
pub enum MeasurementProtocolEndpoint {
    /// Production collection endpoint: <https://www.google-analytics.com/mp/collect>
    Collect,
    /// Debugging endpoint: <https://www.google-analytics.com/debug/mp/collect>
    DebugCollect,
    /// Custom endpoint (primarily for testing).
    Custom(String),
}

impl MeasurementProtocolEndpoint {
    fn as_str(&self) -> &str {
        match self {
            MeasurementProtocolEndpoint::Collect => "https://www.google-analytics.com/mp/collect",
            MeasurementProtocolEndpoint::DebugCollect => "https://www.google-analytics.com/debug/mp/collect",
            MeasurementProtocolEndpoint::Custom(url) => url,
        }
    }
}

#[derive(Clone, Debug)]
pub struct MeasurementProtocolDispatcher {
    client: HttpClient,
    config: MeasurementProtocolConfig,
}

impl MeasurementProtocolDispatcher {
    /// Creates a new dispatcher that will send events to the GA4 Measurement Protocol endpoint.
    pub fn new(config: MeasurementProtocolConfig) -> AnalyticsResult<Self> {
        if config.measurement_id().trim().is_empty() {
            return Err(invalid_argument("measurement protocol measurement_id must not be empty"));
        }
        if config.api_secret().trim().is_empty() {
            return Err(invalid_argument("measurement protocol api_secret must not be empty"));
        }
        Ok(Self {
            client: HttpClient::new(),
            config,
        })
    }

    /// Sends a single analytics event via the measurement protocol.
    ///
    /// The caller is responsible for providing a stable `client_id`, typically sourced from
    /// Firebase Installations or another per-device identifier.
    pub async fn send_event(
        &self,
        client_id: &str,
        event_name: &str,
        params: &BTreeMap<String, String>,
    ) -> AnalyticsResult<()> {
        let payload = MeasurementPayloadOwned {
            client_id: client_id.to_owned(),
            events: vec![MeasurementEventOwned {
                name: event_name.to_owned(),
                params: params.clone(),
            }],
        };

        let mut endpoint = Url::parse(self.config.endpoint.as_str())
            .map_err(|err| internal_error(format!("invalid measurement protocol endpoint: {err}")))?;
        endpoint
            .query_pairs_mut()
            .append_pair("measurement_id", self.config.measurement_id())
            .append_pair("api_secret", self.config.api_secret());

        let request = HttpRequest::post(endpoint)
            .json(&payload)
            .map_err(|err| internal_error(format!("failed to serialise analytics payload: {err}")))?
            .timeout(self.config.timeout());

        let response = self
            .client
            .send(request)
            .await
            .map_err(|err| network_error(format!("failed to send analytics event: {err}")))?;

        if response.is_success() {
            return Ok(());
        }

        let status = response.status();
        let body = response.text();
        let message = if status == 400 {
            format!("measurement protocol rejected the event (400). Response: {body}")
        } else {
            format!("measurement protocol request failed with status {status}. Response: {body}")
        };

        Err(network_error(message))
    }

    pub fn config(&self) -> &MeasurementProtocolConfig {
        &self.config
    }
}

#[derive(Serialize)]
struct MeasurementPayloadOwned {
    client_id: String,
    events: Vec<MeasurementEventOwned>,
}

#[derive(Serialize)]
struct MeasurementEventOwned {
    name: String,
    #[serde(rename = "params")]
    params: BTreeMap<String, String>,
}

use std::time::Duration;

use async_trait::async_trait;
use color_eyre::eyre;
use reqwest::Client;
use url::Url;

use super::{JsonRpcRequest, JsonRpcResponse, Transport};
use crate::engine_api::jwt::JwtProvider;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(3);

pub struct HttpTransport {
    client: Client,
    url: Url,
    jwt_provider: Option<JwtProvider>,
}

impl HttpTransport {
    pub fn new(url: Url) -> Self {
        let client = Client::builder()
            .pool_idle_timeout(Duration::from_secs(90))
            .timeout(REQUEST_TIMEOUT)
            .build()
            .expect("Failed to build HTTP client for Engine Api");
        Self { client, url, jwt_provider: None }
    }

    pub fn with_jwt(mut self, secret: [u8; 32]) -> Self {
        self.jwt_provider = Some(JwtProvider::new(secret));
        self
    }
}

#[async_trait]
impl Transport for HttpTransport {
    async fn send(&self, req: &JsonRpcRequest) -> eyre::Result<JsonRpcResponse> {
        let mut req_builder = self.client.post(self.url.clone()).json(req);
        if let Some(provider) = &self.jwt_provider {
            let token = provider.get_token().await?;
            req_builder = req_builder.bearer_auth(token);
        }

        let resp = req_builder.send().await?;
        let resp_bytes = resp.bytes().await?;
        Ok(serde_json::from_slice(&resp_bytes)?)
    }
}

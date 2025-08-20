// crates/execution/src/transport/http.rs

// This file will contain the HTTP transport implementation.

// --- REFERENCE IMPLEMENTATION ---
// use super::{JsonRpcRequest, JsonRpcResponse, Transport};
// use crate::engine_api::jwt::JwtProvider;
// use async_trait::async_trait;
// use color_eyre::eyre::{self, eyre};
// use reqwest::Client;
// use std::time::Duration;
// use url::Url;
//
// pub struct HttpTransport {
// client: Client,
// url: Url,
// jwt_provider: Option<JwtProvider>,
// }
//
// impl HttpTransport {
// pub fn new(url: Url) -> Self {
// let client = Client::builder()
// .pool_idle_timeout(Duration::from_secs(90))
// .build()
// .expect("Failed to build HTTP client");
//
// Self {
// client,
// url,
// jwt_provider: None,
// }
// }
//
// pub fn with_jwt(mut self, secret: [u8; 32]) -> Self {
// self.jwt_provider = Some(JwtProvider::new(secret));
// self
// }
// }
//
// #[async_trait]
// impl Transport for HttpTransport {
// async fn send(&self, request: &JsonRpcRequest) -> eyre::Result<JsonRpcResponse> {
// let mut req_builder = self.client.post(self.url.clone()).json(request);
//
// if let Some(provider) = &self.jwt_provider {
// let token = provider.get_token().await?;
// req_builder = req_builder.bearer_auth(token);
// }
//
// let response = req_builder.send().await.map_err(|e| eyre!(e))?;
// let response_bytes = response.bytes().await.map_err(|e| eyre!(e))?;
// serde_json::from_slice(&response_bytes).map_err(|e| eyre!(e))
// }
// }

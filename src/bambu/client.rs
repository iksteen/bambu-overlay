use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use reqwest::{Client, Method};
use serde::{de::DeserializeOwned, Serialize};

use super::{
    error::{api_error_message, response_body_is_error, ApiStatus},
    models::{CurrentPrintResponse, LoginRequest, LoginResponse, TasksResponse, UserPreference},
    USER_AGENT,
};

#[derive(Clone)]
pub struct BambuClient {
    client: Client,
    api_base: String,
    timeout: Duration,
}

impl BambuClient {
    pub fn new(api_base: impl Into<String>, timeout: Duration) -> Result<Self> {
        let client = Client::builder()
            .user_agent(USER_AGENT)
            .timeout(timeout)
            .build()
            .context("failed to build HTTP client")?;
        Ok(Self {
            client,
            api_base: api_base.into(),
            timeout,
        })
    }

    pub async fn login(
        &self,
        account: &str,
        password: Option<&str>,
        code: Option<&str>,
    ) -> Result<LoginResponse> {
        if password.is_some() == code.is_some() {
            bail!("provide exactly one of password or verification code");
        }

        let body = LoginRequest {
            account,
            password,
            code,
        };

        self.request_json_with_body(Method::POST, "/v1/user-service/user/login", None, &body)
            .await
    }

    pub async fn current_print(&self, access_token: &str) -> Result<CurrentPrintResponse> {
        self.request_json(
            Method::GET,
            "/v1/iot-service/api/user/print",
            Some(access_token),
        )
        .await
    }

    pub async fn tasks(
        &self,
        access_token: &str,
        limit: usize,
        device_id: Option<&str>,
    ) -> Result<TasksResponse> {
        let mut request = self
            .client
            .request(Method::GET, self.url("/v1/user-service/my/tasks"))
            .bearer_auth(access_token)
            .timeout(self.timeout)
            .query(&[("limit", limit.to_string())]);
        if let Some(device_id) = device_id {
            request = request.query(&[("deviceId", device_id)]);
        }
        parse_response(request.send().await.context("request failed")?).await
    }

    pub async fn user_preference(&self, access_token: &str) -> Result<UserPreference> {
        self.request_json(
            Method::GET,
            "/v1/design-user-service/my/preference",
            Some(access_token),
        )
        .await
    }

    async fn request_json<T>(
        &self,
        method: Method,
        path: &str,
        access_token: Option<&str>,
    ) -> Result<T>
    where
        T: DeserializeOwned,
    {
        let mut request = self
            .client
            .request(method, self.url(path))
            .timeout(self.timeout);
        if let Some(token) = access_token {
            request = request.bearer_auth(token);
        }
        if path == "/v1/iot-service/api/user/print" {
            request = request.query(&[("force", "true")]);
        }
        parse_response(request.send().await.context("request failed")?).await
    }

    async fn request_json_with_body<T, B>(
        &self,
        method: Method,
        path: &str,
        access_token: Option<&str>,
        body: &B,
    ) -> Result<T>
    where
        T: DeserializeOwned,
        B: Serialize + ?Sized,
    {
        let mut request = self
            .client
            .request(method, self.url(path))
            .timeout(self.timeout)
            .json(body);
        if let Some(token) = access_token {
            request = request.bearer_auth(token);
        }
        parse_response(request.send().await.context("request failed")?).await
    }

    fn url(&self, path: &str) -> String {
        format!(
            "{}/{}",
            self.api_base.trim_end_matches('/'),
            path.trim_start_matches('/')
        )
    }
}

async fn parse_response<T>(response: reqwest::Response) -> Result<T>
where
    T: DeserializeOwned,
{
    let status = response.status();
    let bytes = response
        .bytes()
        .await
        .context("failed to read response body")?;
    let body = if bytes.is_empty() {
        b"{}".as_slice()
    } else {
        bytes.as_ref()
    };
    let api_status: ApiStatus = serde_json::from_slice(body).with_context(|| {
        let preview = String::from_utf8_lossy(&body[..body.len().min(500)]);
        format!("response was not a valid JSON object: {preview}")
    })?;

    if !status.is_success() {
        return Err(anyhow!(
            "{}",
            api_error_message(Some(status.as_u16()), &api_status)
        ));
    }
    if response_body_is_error(&api_status) {
        return Err(anyhow!("{}", api_error_message(None, &api_status)));
    }
    serde_json::from_slice(body).context("response JSON did not match the expected shape")
}

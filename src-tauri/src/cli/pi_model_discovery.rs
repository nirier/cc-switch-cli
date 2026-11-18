use std::collections::HashSet;
use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderName, HeaderValue, AUTHORIZATION};
use serde_json::{json, Value};
use url::Url;

use crate::pi_config::MAX_PI_FILE_BYTES;

#[derive(Debug, Clone)]
pub(crate) struct DiscoveryRequest {
    base_url: Url,
    headers: HeaderMap,
}

pub(crate) struct DiscoveredModels {
    pub(crate) base_url: Url,
    pub(crate) models: Vec<Value>,
}

impl DiscoveredModels {
    pub(crate) fn apply(self, settings: &mut Value) {
        settings["baseUrl"] = json!(self.base_url.as_str());
        settings["models"] = Value::Array(self.models);
    }
}

pub(crate) fn target(settings: &Value) -> Option<DiscoveryRequest> {
    if !matches!(
        settings.get("api").and_then(Value::as_str),
        Some("openai-completions" | "openai-responses")
    ) || settings
        .get("models")
        .is_some_and(|models| !models.as_array().is_some_and(Vec::is_empty))
    {
        return None;
    }

    let base_url = Url::parse(settings.get("baseUrl")?.as_str()?.trim()).ok()?;
    if !matches!(base_url.scheme(), "http" | "https") || base_url.host_str().is_none() {
        return None;
    }
    let api_key = literal_value(settings.get("apiKey")?.as_str()?.trim())?;
    if api_key.is_empty() {
        return None;
    }
    let mut headers = HeaderMap::new();
    let mut bearer = HeaderValue::from_str(&format!("Bearer {api_key}")).ok()?;
    bearer.set_sensitive(true);
    headers.insert(AUTHORIZATION, bearer);
    if let Some(configured) = settings.get("headers").and_then(Value::as_object) {
        for (name, value) in configured {
            let name = HeaderName::from_bytes(name.as_bytes()).ok()?;
            let mut value = HeaderValue::from_str(&literal_value(value.as_str()?)?).ok()?;
            value.set_sensitive(true);
            headers.insert(name, value);
        }
    }

    Some(DiscoveryRequest { base_url, headers })
}

// Pi uses leading ! for commands and $ for interpolation. Only resolve literal escapes.
fn literal_value(value: &str) -> Option<String> {
    if value.starts_with('!') {
        return None;
    }

    let mut literal = String::with_capacity(value.len());
    let mut remaining = value;
    while let Some((prefix, suffix)) = remaining.split_once('$') {
        literal.push_str(prefix);
        let mut next = suffix.chars();
        match next.next() {
            Some(escaped @ ('$' | '!')) => {
                literal.push(escaped);
                remaining = next.as_str();
                continue;
            }
            Some('{') => {
                if let Some((name, tail)) = next.as_str().split_once('}') {
                    let mut name_chars = name.chars();
                    if matches!(name_chars.next(), Some('_' | 'a'..='z' | 'A'..='Z'))
                        && name_chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
                    {
                        return None;
                    }
                    literal.push_str("${");
                    literal.push_str(name);
                    literal.push('}');
                    remaining = tail;
                    continue;
                }
            }
            Some('_' | 'a'..='z' | 'A'..='Z') => return None,
            _ => {}
        }
        literal.push('$');
        remaining = suffix;
    }
    literal.push_str(remaining);
    Some(literal)
}

pub(crate) async fn discover(request: DiscoveryRequest) -> Result<DiscoveredModels, String> {
    let DiscoveryRequest {
        mut base_url,
        headers,
    } = request;
    base_url.set_fragment(None);
    let client = crate::proxy::http_client::without_redirects()?;
    let mut last_error = String::new();
    for path in candidate_paths(base_url.path()) {
        let mut models_url = base_url.clone();
        models_url.set_path(&format!("{path}/models"));
        let response = client
            .get(models_url)
            .headers(headers.clone())
            .timeout(Duration::from_secs(5))
            .send()
            .await;
        let discovered = match response {
            Ok(response) if response.status().is_success() => read_models(response).await,
            Ok(response) => Err(format!(
                "{} {}",
                crate::t!("Model endpoint returned", "模型接口返回"),
                response.status()
            )),
            Err(error) => Err(error.without_url().to_string()),
        };
        match discovered {
            Ok(models) => {
                base_url.set_path(&path);
                return Ok(DiscoveredModels { base_url, models });
            }
            Err(error) => last_error = error,
        }
    }

    Err(last_error)
}

fn candidate_paths(path: &str) -> Vec<String> {
    let path = path.trim_end_matches('/');
    let mut paths = vec![path.to_string()];
    if !path.ends_with("/v1") {
        paths.push(format!("{path}/v1"));
    }
    if let Some(base) = path.strip_suffix("/models") {
        paths.push(base.to_string());
    }
    paths
}

async fn read_models(mut response: reqwest::Response) -> Result<Vec<Value>, String> {
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| error.without_url().to_string())?
    {
        if chunk.len() as u64 > MAX_PI_FILE_BYTES - body.len() as u64 {
            return Err(
                crate::t!("Model response exceeds 1 MiB", "模型接口响应超过 1 MiB").to_string(),
            );
        }
        body.extend_from_slice(&chunk);
    }
    let payload: Value = serde_json::from_slice(&body).map_err(|error| error.to_string())?;
    models_from_list(&payload)
}

fn models_from_list(payload: &Value) -> Result<Vec<Value>, String> {
    let mut seen = HashSet::new();
    let models: Vec<_> = payload
        .get("data")
        .unwrap_or(payload)
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|model| model.get("id").and_then(Value::as_str))
        .filter(|id| !id.trim().is_empty() && seen.insert(*id))
        .map(|id| json!({ "id": id }))
        .collect();
    if models.is_empty() {
        return Err(crate::t!("No model IDs returned", "接口未返回模型 ID").to_string());
    }
    Ok(models)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pi_model_discovery_decodes_only_literal_credential_syntax() {
        assert_eq!(literal_value("a$$b$!c").as_deref(), Some("a$b!c"));
        assert!(literal_value("a$TOKEN").is_none());
        assert!(literal_value("a${_TOKEN}").is_none());
        assert_eq!(literal_value("key${1}").as_deref(), Some("key${1}"));
        assert_eq!(literal_value("key${").as_deref(), Some("key${"));
        assert!(literal_value("!${SOME_COMMAND}").is_none());
    }

    #[test]
    fn pi_model_discovery_uses_the_supplied_base_before_interpreting_it_as_a_full_url() {
        assert_eq!(candidate_paths("/models"), ["/models", "/models/v1", ""]);
    }

    #[test]
    fn pi_model_discovery_requires_model_ids() {
        assert!(models_from_list(&json!({ "models": [{ "name": "models/a" }] })).is_err());
    }
}

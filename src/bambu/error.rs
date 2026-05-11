use std::fmt;

use serde::{
    de::{self, IgnoredAny, MapAccess, SeqAccess, Visitor},
    Deserialize, Deserializer,
};

#[derive(Debug, Clone, Default, Deserialize)]
pub(super) struct ApiStatus {
    #[serde(default, deserialize_with = "deserialize_api_field")]
    code: ApiField,
    #[serde(default, deserialize_with = "deserialize_api_field")]
    error: ApiField,
    #[serde(default, deserialize_with = "deserialize_api_field")]
    message: ApiField,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
enum ApiField {
    #[default]
    Empty,
    Text(String),
    Other,
}

impl ApiField {
    fn text(&self) -> Option<&str> {
        match self {
            ApiField::Text(text) if !text.trim().is_empty() => Some(text),
            _ => None,
        }
    }
}

pub(super) fn response_body_is_error(body: &ApiStatus) -> bool {
    is_error_field(&body.error) || is_error_code(&body.code)
}

pub(super) fn api_error_message(status: Option<u16>, body: &ApiStatus) -> String {
    let prefix = status
        .map(|status| format!("HTTP {status}"))
        .unwrap_or_else(|| "Bambu API error".to_owned());
    let detail = [&body.error, &body.message, &body.code]
        .into_iter()
        .find_map(|field| field.text().filter(|text| !is_success_text(text)));
    match detail {
        Some(detail) => format!("{prefix}: {detail}"),
        None => prefix,
    }
}

fn is_error_field(value: &ApiField) -> bool {
    match value {
        ApiField::Empty => false,
        ApiField::Text(text) => !text.trim().is_empty() && !is_success_text(text),
        ApiField::Other => true,
    }
}

fn is_error_code(value: &ApiField) -> bool {
    match value {
        ApiField::Empty => false,
        ApiField::Text(text) => {
            let text = text.trim();
            !text.is_empty() && text != "0" && !is_success_text(text)
        }
        ApiField::Other => true,
    }
}

fn is_success_text(text: &str) -> bool {
    text.trim().eq_ignore_ascii_case("success")
}

fn deserialize_api_field<'de, D>(deserializer: D) -> Result<ApiField, D::Error>
where
    D: Deserializer<'de>,
{
    deserializer.deserialize_any(ApiFieldVisitor)
}

struct ApiFieldVisitor;

impl<'de> Visitor<'de> for ApiFieldVisitor {
    type Value = ApiField;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a Bambu API status field")
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(ApiField::Empty)
    }

    fn visit_none<E>(self) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(ApiField::Empty)
    }

    fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(self)
    }

    fn visit_bool<E>(self, _value: bool) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(ApiField::Other)
    }

    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(ApiField::Text(value.to_string()))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(ApiField::Text(value.to_string()))
    }

    fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(ApiField::Text(value.to_string()))
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        let value = value.trim();
        if value.is_empty() {
            Ok(ApiField::Empty)
        } else {
            Ok(ApiField::Text(value.to_owned()))
        }
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        self.visit_str(&value)
    }

    fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        while seq.next_element::<IgnoredAny>()?.is_some() {}
        Ok(ApiField::Other)
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
        Ok(ApiField::Other)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{api_error_message, response_body_is_error, ApiStatus};

    fn api_status(value: serde_json::Value) -> ApiStatus {
        serde_json::from_value(value).expect("fixture should match API status fields")
    }

    #[test]
    fn success_envelope_with_null_error_fields_is_not_an_error() {
        let body = api_status(json!({
            "message": "success",
            "code": null,
            "error": null,
            "devices": []
        }));

        assert!(!response_body_is_error(&body));
    }

    #[test]
    fn success_code_zero_is_not_an_error() {
        assert!(!response_body_is_error(&api_status(json!({"code": 0}))));
        assert!(!response_body_is_error(&api_status(json!({"code": "0"}))));
    }

    #[test]
    fn non_empty_error_or_non_zero_code_is_an_error() {
        assert!(response_body_is_error(&api_status(
            json!({"error": "Resource forbidden"})
        )));
        assert!(response_body_is_error(&api_status(json!({"code": 8}))));
    }

    #[test]
    fn success_message_is_not_used_as_error_detail() {
        let body = api_status(json!({
            "message": "success",
            "code": 8
        }));

        assert_eq!(api_error_message(None, &body), "Bambu API error: 8");
    }
}

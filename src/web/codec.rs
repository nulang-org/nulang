//! Compiler-owned request/response body codec metadata.
//!
//! Codecs describe wire semantics without changing Nulang's VM value model.
//! `Json[T]` currently erases to an already-serialized `String`; the codec
//! validates the wire representation and preserves `T` as schema metadata.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BodyCodecKind {
    Json,
}

impl BodyCodecKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Json => "json",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BodyCodecContract {
    pub codec: BodyCodecKind,
    pub media_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload_type: Option<String>,
}

impl BodyCodecContract {
    pub fn json(payload_type: String) -> Self {
        Self {
            codec: BodyCodecKind::Json,
            media_type: "application/json".to_string(),
            payload_type: Some(payload_type),
        }
    }
}

/// Derive an explicit wire codec from a source-level body type.
///
/// Unknown/unwrapped types deliberately return `None`: protocol semantics are
/// never guessed from arbitrary domain names or from `String`.
pub fn body_codec_for_type(ty: Option<&str>) -> Option<BodyCodecContract> {
    let ty = ty?.trim();
    let payload = ty.strip_prefix("Json[")?.strip_suffix(']')?.trim();
    if payload.is_empty() {
        return None;
    }
    Some(BodyCodecContract::json(payload.to_string()))
}

/// Validate an encoded body according to its compiler-owned codec.
///
/// The runtime representation remains the original serialized text. This
/// validator proves transport correctness without prematurely inventing a VM
/// record decoder. Primitive payload markers receive shape validation; domain
/// payload types receive JSON syntax validation only until typed codecs land.
pub fn validate_encoded_body(contract: &BodyCodecContract, raw: &str) -> Result<(), String> {
    match contract.codec {
        BodyCodecKind::Json => validate_json(contract.payload_type.as_deref(), raw),
    }
}

fn validate_json(payload_type: Option<&str>, raw: &str) -> Result<(), String> {
    let value: Value =
        serde_json::from_str(raw).map_err(|error| format!("invalid JSON body: {error}"))?;

    let valid_shape = match payload_type.map(str::trim) {
        Some("String") => value.is_string(),
        Some("Bool") => value.is_boolean(),
        Some("Int") => value.as_i64().is_some() || value.as_u64().is_some(),
        Some("Float") => value.as_f64().is_some(),
        _ => true,
    };

    if valid_shape {
        Ok(())
    } else {
        Err(format!(
            "JSON body does not match payload type {}",
            payload_type.unwrap_or("<unknown>")
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_json_codec_without_guessing_string() {
        let codec = body_codec_for_type(Some("Json[CreateUser]")).unwrap();
        assert_eq!(codec.codec, BodyCodecKind::Json);
        assert_eq!(codec.media_type, "application/json");
        assert_eq!(codec.payload_type.as_deref(), Some("CreateUser"));

        assert!(body_codec_for_type(Some("String")).is_none());
        assert!(body_codec_for_type(Some("CreateUser")).is_none());
        assert!(body_codec_for_type(Some("Json[]")).is_none());
    }

    #[test]
    fn validates_json_syntax_and_primitive_shape() {
        let domain = body_codec_for_type(Some("Json[CreateUser]")).unwrap();
        assert!(validate_encoded_body(&domain, r#"{"name":"Ada"}"#).is_ok());
        assert!(validate_encoded_body(&domain, "{").is_err());

        let string = body_codec_for_type(Some("Json[String]")).unwrap();
        assert!(validate_encoded_body(&string, r#""hello""#).is_ok());
        assert!(validate_encoded_body(&string, "42").is_err());

        let int = body_codec_for_type(Some("Json[Int]")).unwrap();
        assert!(validate_encoded_body(&int, "42").is_ok());
        assert!(validate_encoded_body(&int, "42.5").is_err());
    }
}

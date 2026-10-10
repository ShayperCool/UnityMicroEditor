//! The backend seam of the server: `RuntimeHandler` and `call_backend`.
use crate::{
    agentcontrol::{self, Controller},
    authoring::AuthoringError,
    cancel::{Canceled, Context},
    jsondoc::{self, Json},
    sysutil,
};
use serde_json::{Map, Value};

/// What a handler returns as its dynamically typed result, already JSON-encoded.
/// A JSON object result is treated differently from every other value when a
/// reply exceeds the size bound.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandlerValue {
    /// No result (encoded as `null`).
    Nil,
    /// Any value other than an object.
    Json(Vec<u8>),
    /// An object.
    Map(Vec<u8>),
}
impl HandlerValue {
    /// The encoding of a decoded JSON value: objects become `Map`, `null` becomes `Nil`.
    pub fn from_value(value: &Value) -> Self {
        match value {
            Value::Null => HandlerValue::Nil,
            Value::Object(_) => HandlerValue::Map(jsondoc::encode(value)),
            other => HandlerValue::Json(jsondoc::encode(other)),
        }
    }
    pub(crate) fn bytes(&self) -> &[u8] {
        match self {
            HandlerValue::Nil => b"null",
            HandlerValue::Json(bytes) | HandlerValue::Map(bytes) => bytes,
        }
    }
}

/// The error of a handler, classified by kind.
#[derive(Debug, Clone)]
pub enum HandlerError {
    /// An [`AuthoringError`].
    Typed(AuthoringError),
    /// The request was cancelled or its deadline passed; carries the error text.
    Cancelled(String),
    /// Any other error, with its text.
    Failed(String),
}
impl HandlerError {
    pub fn failed(message: impl Into<String>) -> Self {
        HandlerError::Failed(message.into())
    }
    /// The error text.
    pub fn message(&self) -> String {
        match self {
            HandlerError::Typed(error) => error.to_string(),
            HandlerError::Cancelled(message) | HandlerError::Failed(message) => message.clone(),
        }
    }
}
impl std::fmt::Display for HandlerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}
impl std::error::Error for HandlerError {}
impl From<AuthoringError> for HandlerError {
    /// The authoring layer reports untyped errors (I/O, context) with the
    /// code `internal_error`; every other code is a typed authoring error.
    fn from(error: AuthoringError) -> Self {
        if error.code != "internal_error" {
            return HandlerError::Typed(error);
        }
        if [Canceled::Canceled, Canceled::DeadlineExceeded]
            .iter()
            .any(|kind| kind.to_string() == error.message)
        {
            return HandlerError::Cancelled(error.message);
        }
        HandlerError::Failed(error.message)
    }
}
impl From<Canceled> for HandlerError {
    fn from(error: Canceled) -> Self {
        HandlerError::Cancelled(error.to_string())
    }
}
impl From<sysutil::Error> for HandlerError {
    fn from(error: sysutil::Error) -> Self {
        if error.is_canceled() || error.is_deadline_exceeded() {
            HandlerError::Cancelled(error.message().to_string())
        } else {
            HandlerError::Failed(error.message().to_string())
        }
    }
}

/// `func(ctx, name, raw) (any, error)`. `Options::runtime` and
/// `Options::project_actions` both have this type. `raw` is the JSON encoding of
/// the typed tool input.
pub trait RuntimeHandler: Send + Sync {
    fn handle(&self, ctx: &Context, name: &str, raw: &[u8]) -> Result<HandlerValue, HandlerError>;
}
impl<F> RuntimeHandler for F
where
    F: Fn(&Context, &str, &[u8]) -> Result<HandlerValue, HandlerError> + Send + Sync,
{
    fn handle(&self, ctx: &Context, name: &str, raw: &[u8]) -> Result<HandlerValue, HandlerError> {
        self(ctx, name, raw)
    }
}
/// `Controller::handle` is the runtime backend of the agent CLI.
impl RuntimeHandler for Controller {
    fn handle(&self, ctx: &Context, name: &str, raw: &[u8]) -> Result<HandlerValue, HandlerError> {
        let encode = |reply: &agentcontrol::Reply| -> Result<HandlerValue, HandlerError> {
            let bytes = jsondoc::marshal(reply).map_err(HandlerError::Failed)?;
            Ok(match reply {
                // `Controller::request` returns the decoded receipt object.
                agentcontrol::Reply::Value(Value::Object(_)) => HandlerValue::Map(bytes),
                agentcontrol::Reply::Value(Value::Null) => HandlerValue::Nil,
                _ => HandlerValue::Json(bytes),
            })
        };
        match Controller::handle(self, ctx, name, raw) {
            Ok(reply) => encode(&reply),
            // The state returned next to an error is never read by the server.
            Err(failure) => Err(failure.error.into()),
        }
    }
}

/// Decodes an encoded object into a map. Numbers become f64, so their text
/// follows the shortest round-trip f64 encoding. `None` is what a JSON `null`
/// decodes to.
pub(crate) fn object_value(encoded: &[u8]) -> Result<Option<Map<String, Value>>, String> {
    match jsondoc::parse(encoded)? {
        Json::Null => Ok(None),
        Json::Object(members) => jsondoc::to_map(&members, true).map(Some),
        other => Err(jsondoc::Target::VALUE.mismatch(other.kind(), "object")),
    }
}

/// The result of `call_backend`.
pub(crate) enum BackendData {
    /// The handler's own value.
    Original(HandlerValue),
    /// The f64-decoded receipt of an unconfirmed command.
    Structured(Map<String, Value>),
}

pub(crate) fn text<'a>(map: &'a Map<String, Value>, key: &str) -> &'a str {
    map.get(key).and_then(Value::as_str).unwrap_or_default()
}
pub(crate) fn unconfirmed(status: &str) -> bool {
    matches!(
        status,
        "result_pending" | "result_unknown" | "outcome_unknown"
    )
}

/// The data and error handed to `reply`.
pub(crate) fn call_backend(
    ctx: &Context,
    name: &str,
    input: &[u8],
    handler: &dyn RuntimeHandler,
    failure_code: &str,
) -> (BackendData, Option<HandlerError>) {
    let result = match handler.handle(ctx, name, input) {
        Ok(result) => result,
        Err(error) => return (BackendData::Original(HandlerValue::Nil), Some(error)),
    };
    let Ok(Some(structured)) = object_value(result.bytes()) else {
        return (BackendData::Original(result), None);
    };
    if unconfirmed(text(&structured, "status")) {
        return (BackendData::Structured(structured), None);
    }
    if structured.get("success") == Some(&Value::Bool(false)) {
        let mut message = "Backend rejected the command";
        if let Some(text) = structured.get("error").and_then(Value::as_str)
            && !text.is_empty()
        {
            message = text;
        }
        if let Some(text) = structured
            .get("error")
            .and_then(Value::as_object)
            .and_then(|rejection| rejection.get("message"))
            .and_then(Value::as_str)
            && !text.is_empty()
        {
            message = text;
        }
        let rejection = AuthoringError {
            code: failure_code.into(),
            message: message.into(),
            details: Some(Value::Object(structured.clone())),
        };
        return (
            BackendData::Original(result),
            Some(HandlerError::Typed(rejection)),
        );
    }
    (BackendData::Original(result), None)
}

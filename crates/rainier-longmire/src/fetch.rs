//! One call to one Lookout.

use rainier_http_client::Http;
use serde_json::Value;

use crate::{Longmire, Source};

/// Why a Lookout gave nothing, in words a person can act on.
pub(crate) fn explain(status: u16) -> String {
    match status {
        404 => "Lookout answered 404: it is off there, request tracing is off, it has no \
                reader for other services, or these credentials are not one."
            .into(),
        401 | 403 => format!("Lookout refused the credentials ({status})."),
        _ => format!("Lookout answered {status}."),
    }
}

/// `GET {source}/api/{tail}?{query}` with the source's credentials: the JSON
/// it answered, or why there is none.
pub(crate) async fn fetch(
    longmire: &Longmire,
    source: &Source,
    tail: &str,
    query: &str,
) -> Result<Value, String> {
    let mut request =
        Http::get(source.api_url(tail, query)).timeout(longmire.timeout).accept_json();
    if let Some(credentials) = &longmire.credentials {
        match credentials(source).await {
            Some(token) => request = request.bearer(token),
            None => return Err(format!("No credentials for {}.", source.name)),
        }
    }
    if let Some(transport) = &longmire.transport {
        request = request.through(std::sync::Arc::clone(transport));
    }
    let response = request.send().await.map_err(|_| format!("{} did not answer.", source.name))?;
    if !(200..300).contains(&response.status()) {
        return Err(explain(response.status()));
    }
    serde_json::from_slice(response.bytes())
        .map_err(|_| "Lookout's answer was not JSON.".to_string())
}

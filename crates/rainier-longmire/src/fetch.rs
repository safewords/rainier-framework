//! One call to one Lookout.

use rainier_http_client::Http;
use serde_json::Value;

use crate::{Longmire, Source};

/// Why a Lookout gave nothing, in words a person can act on.
pub(crate) fn explain(status: u16) -> String {
    match status {
        404 => "Lookout answered 404: it is off there, request tracing is off, or it does \
                not recognise this Longmire (its link key or credentials)."
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
    let url = source.api_url(tail, query);
    let mut request = Http::get(&url).timeout(longmire.timeout).accept_json();
    let signed = longmire.link.as_ref().map(|key| {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        (key, rainier_lookout_link::sign(key, "GET", path_and_query(&url), now))
    });
    if let Some((_, signed)) = &signed {
        request = request.header(rainier_lookout_link::HEADER, signed.header.clone());
    } else if let Some(credentials) = &longmire.credentials {
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
    let body = match &signed {
        Some((key, signed)) => {
            if response.header("content-type") != Some(rainier_lookout_link::SEALED) {
                return Err("Lookout's answer was not sealed: it is not on the link.".into());
            }
            rainier_lookout_link::open(key, &signed.nonce, response.bytes())
                .map_err(|_| "Lookout's answer could not be opened.".to_string())?
        }
        None => response.bytes().to_vec(),
    };
    serde_json::from_slice(&body).map_err(|_| "Lookout's answer was not JSON.".to_string())
}

/// `/path?query` of a URL: what is signed, exactly as the server receives it.
fn path_and_query(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    rest.find('/').map_or("/", |at| &rest[at..])
}

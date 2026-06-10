use axum::{
    extract::Request,
    http::{HeaderMap, HeaderValue, header},
    middleware::Next,
    response::{Html, IntoResponse, Response},
};
use maud::Markup;

pub struct Template(pub Markup);

impl IntoResponse for Template {
    fn into_response(self) -> Response {
        Html(self.0.into_string()).into_response()
    }
}

/// Plain-text body carried alongside an HTML response; negotiate_plain_text
/// swaps it in when the client didn't ask for HTML.
#[derive(Clone)]
pub struct PlainAlternate(pub String);

/// Whether the client asked for HTML. Browsers put text/html in Accept on
/// every navigation and form submit; CLI clients (curl, wget) send `*/*` or
/// no Accept at all, so they get plain text without opting in.
pub fn wants_html(headers: &HeaderMap) -> bool {
    headers
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|accept| accept.contains("text/html"))
}

/// Serves the PlainAlternate of an HTML response to non-browser clients, and
/// marks negotiated content types with `Vary: Accept` so a shared cache can't
/// hand the HTML page to curl (or the plain text to a browser).
pub async fn negotiate_plain_text(request: Request, next: Next) -> Response {
    let wants_html = wants_html(request.headers());
    let mut response = next.run(request).await;

    let negotiable = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|ct| ct.starts_with("text/html") || ct.starts_with("text/plain"));
    if !negotiable {
        return response;
    }

    if !wants_html
        && let Some(PlainAlternate(text)) = response.extensions_mut().remove::<PlainAlternate>()
    {
        let mut plain = (
            response.status(),
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            text,
        )
            .into_response();
        plain
            .headers_mut()
            .append(header::VARY, HeaderValue::from_static("accept"));
        return plain;
    }

    response
        .headers_mut()
        .append(header::VARY, HeaderValue::from_static("accept"));
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    fn accept(value: &'static str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(header::ACCEPT, HeaderValue::from_static(value));
        headers
    }

    #[test]
    fn browser_accept_wants_html() {
        assert!(wants_html(&accept(
            "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8"
        )));
    }

    #[test]
    fn curl_default_accept_gets_plain_text() {
        assert!(!wants_html(&accept("*/*")));
    }

    #[test]
    fn missing_accept_gets_plain_text() {
        assert!(!wants_html(&HeaderMap::new()));
    }
}

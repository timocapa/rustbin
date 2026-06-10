use std::sync::Arc;

use crate::{
    db::{
        insert_paste, load_paste_by_ref, load_paste_content, load_paste_meta_by_ref, sanitize_form,
        split_paste_ref,
    },
    error::AppError,
    extractors::parse_create_paste_multipart,
    highlighter, preview,
    render::{PastePage, index_page, paste_page, url_paste_page, usage_page},
    response::{Template, wants_html},
    state::{AppResult, AppState},
};
use axum::{
    body::Body,
    extract::{Multipart, Path as AxumPath, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use tracing::warn;

static FAVICON: &[u8] = include_bytes!("../assets/favicon.ico");
static LOGO: &[u8] = include_bytes!("../assets/logo.png");

pub async fn favicon() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "image/x-icon")], FAVICON)
}

pub async fn logo() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "image/png")], LOGO)
}

pub async fn index(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if !wants_html(&headers) {
        return cli_usage_response(state.base_url.as_deref(), &headers);
    }
    Template(index_page(None)).into_response()
}

pub async fn usage(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if !wants_html(&headers) {
        return cli_usage_response(state.base_url.as_deref(), &headers);
    }
    Template(usage_page()).into_response()
}

fn cli_usage_response(base_url: Option<&str>, headers: &HeaderMap) -> Response {
    let origin = request_origin(base_url, headers);
    let text = format!(
        "rustbin — minimalist pastebin and URL shortener\n\
         \n\
         Create a paste:    curl -F 'file=@example.txt' {origin}/\n\
         With expiry:       curl -F 'file=@example.txt' -F 'expires_in=3600' {origin}/\n\
         \x20                  (expires_in: positive seconds or 'never')\n\
         Read a paste:      curl {origin}/<id>\n\
         Shorten a URL:     echo 'https://example.com' | curl -F 'file=@-' {origin}/\n\
         \n\
         In a browser, {origin}/<id> renders with line links; add an extension\n\
         ({origin}/<id>.rs) to pick the syntax; {origin}/<id>/raw is plain text.\n"
    );
    ([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], text).into_response()
}

pub async fn create_paste_multipart(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    multipart: Multipart,
) -> AppResult<Response> {
    let mut form = parse_create_paste_multipart(multipart).await?;
    let from_browser = form.from_browser;
    let content_is_url = form.content.as_deref().is_some_and(is_url);

    // Language detection is irrelevant for URL pastes (they redirect and never
    // render highlighted), so skip it. The filename/size fast paths are pure
    // in-memory lookups and run inline; only the enry classifier — a blocking
    // cgo FFI call — is offloaded to the blocking pool.
    let detection = if content_is_url {
        highlighter::LanguageDetection::Resolved(None)
    } else {
        highlighter::detect_language_fast(
            &state,
            form.filename.as_deref(),
            form.content.as_deref().unwrap_or_default(),
        )
    };
    form.language = match detection {
        highlighter::LanguageDetection::Resolved(language) => language,
        highlighter::LanguageDetection::NeedsClassifier => {
            // Only reached for content within classifier_max_bytes, so this
            // clone is small and the upload survives a classifier failure.
            let content = form.content.clone().unwrap_or_default();
            let classify_state = Arc::clone(&state);
            match tokio::task::spawn_blocking(move || {
                highlighter::classify_language(&classify_state, &content)
            })
            .await
            {
                Ok(language) => language,
                Err(error) => {
                    // Detection is advisory; never fail the upload over it.
                    warn!(%error, "language classifier task failed");
                    None
                }
            }
        }
    };

    let form = sanitize_form(form);
    let destination_url = if content_is_url {
        form.content.as_deref().map(|c| c.trim().to_string())
    } else {
        None
    };
    let id = insert_paste(&state.db, form).await?;
    let location = build_paste_url(state.base_url.as_deref(), &headers, &id);

    // The browser experience needs both the form's `content` field and an
    // Accept asking for HTML; `curl -F content=...` keeps the plain-text URL.
    if from_browser && wants_html(&headers) {
        if let Some(destination) = destination_url {
            return Ok(Template(url_paste_page(&location, &destination)).into_response());
        }
        return Ok((StatusCode::SEE_OTHER, [(header::LOCATION, location)]).into_response());
    }

    Ok((
        StatusCode::CREATED,
        [(header::LOCATION, location.clone())],
        format!("{location}\n"),
    )
        .into_response())
}

pub async fn show_paste(
    State(state): State<Arc<AppState>>,
    AxumPath(paste_ref): AxumPath<String>,
    headers: HeaderMap,
) -> AppResult<Response> {
    let meta = load_paste_meta_by_ref(&state.db, &paste_ref)
        .await?
        .ok_or(AppError::NotFound("Paste not found."))?;

    // URL pastes redirect. The stored content head rules the URL case out for
    // free; only candidates read the full (potentially multi-MB) content row
    // here — cache hits below never touch it.
    let mut content: Option<String> = None;
    if head_may_be_url(&meta.head) {
        let full = load_paste_content(&state.db, &meta.id)
            .await?
            .ok_or(AppError::NotFound("Paste not found."))?;
        if is_url(&full) {
            let url = full.trim().to_string();
            return Ok((StatusCode::FOUND, [(header::LOCATION, url)]).into_response());
        }
        content = Some(full);
    }

    // CLI clients get the content itself; rendering (and its cache) is a
    // browser-only concern.
    if !wants_html(&headers) {
        let content = match content {
            Some(content) => content,
            None => load_paste_content(&state.db, &meta.id)
                .await?
                .ok_or(AppError::NotFound("Paste not found."))?,
        };
        return Ok((
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            content,
        )
            .into_response());
    }

    let (_, ref_extension) = split_paste_ref(&paste_ref);
    let extension = ref_extension.or(meta.language.as_deref());
    let is_markdown = highlighter::is_markdown(extension);
    let cache_key = render_cache_key(&state, &meta.id, extension);
    let origin = request_origin(state.base_url.as_deref(), &headers);

    let page = |content_html: &str| {
        Template(paste_page(&PastePage {
            origin: &origin,
            paste_ref: &paste_ref,
            paste_id: &meta.id,
            content_html,
            is_markdown,
        }))
        .into_response()
    };

    if let Some(cached) = state.render_cache.lock().get(&cache_key).cloned() {
        return Ok(page(&cached));
    }

    // Single-flight: concurrent misses for the same key wait here instead of
    // all rendering, then find the result on the re-check.
    let _render_guard = state.render_locks.lock(&cache_key).await;
    if let Some(cached) = state.render_cache.lock().get(&cache_key).cloned() {
        return Ok(page(&cached));
    }

    let content = match content {
        Some(content) => content,
        None => load_paste_content(&state.db, &meta.id)
            .await?
            .ok_or(AppError::NotFound("Paste not found."))?,
    };

    // Rendering is CPU-bound (syntect / pulldown-cmark); keep it off the async
    // executor. render_paste_html numbers and anchors every line, in
    // content-visibility chunks the browser only lays out as they scroll near.
    let render_state = Arc::clone(&state);
    let extension_owned = extension.map(str::to_string);
    let rendered: Arc<str> = tokio::task::spawn_blocking(move || {
        let html = if is_markdown {
            highlighter::render_markdown(&render_state, &content)
        } else {
            highlighter::render_paste_html(&render_state, extension_owned.as_deref(), &content)
        };
        Arc::from(html)
    })
    .await
    .map_err(|_| AppError::InternalMessage("rendering failed"))?;

    // Don't let a few huge renders own the whole count-bounded LRU's memory;
    // oversized views are re-rendered per request (still single-flighted).
    if rendered.len() <= state.render_cache_max_entry_bytes {
        state
            .render_cache
            .lock()
            .put(cache_key, Arc::clone(&rendered));
    }

    Ok(page(&rendered))
}

fn is_url(content: &str) -> bool {
    let trimmed = content.trim();
    // Prefix check first: it rejects nearly everything in O(1); the
    // control-character scan is O(len) and only worth running on candidates.
    (trimmed.starts_with("http://") || trimmed.starts_with("https://"))
        && !trimmed.bytes().any(|b| b.is_ascii_control())
}

/// Cheap URL-paste gate over the stored 16-char content head, so non-URL
/// pastes (the overwhelming majority) skip loading the full content row.
fn head_may_be_url(head: &str) -> bool {
    let trimmed = head.trim_start();
    if trimmed.len() >= 4 {
        trimmed.starts_with("http")
    } else {
        // Shorter than "http" after trimming (tiny paste or mostly leading
        // whitespace): can't rule it out, check the real content.
        true
    }
}

pub async fn show_raw_paste(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
) -> AppResult<Response> {
    let content = load_paste_by_ref(&state.db, &id)
        .await?
        .ok_or(AppError::NotFound("Paste not found."))?;

    Ok((
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        content,
    )
        .into_response())
}

pub async fn show_preview(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
) -> AppResult<Response> {
    let meta = load_paste_meta_by_ref(&state.db, &id)
        .await?
        .ok_or(AppError::NotFound("Paste not found."))?;

    let (_, ref_extension) = split_paste_ref(&id);
    let extension = ref_extension.or(meta.language.as_deref());

    let cache_key = render_cache_key(&state, &meta.id, extension);
    if let Some(cached) = state.preview_cache.lock().get(&cache_key).cloned() {
        return Ok(preview_response(cached));
    }

    // Single-flight, same as show_paste: one generation per key at a time.
    let _preview_guard = state.preview_locks.lock(&cache_key).await;
    if let Some(cached) = state.preview_cache.lock().get(&cache_key).cloned() {
        return Ok(preview_response(cached));
    }

    let content = load_paste_content(&state.db, &meta.id)
        .await?
        .ok_or(AppError::NotFound("Paste not found."))?;

    let ext_owned = extension.map(str::to_string);
    let state_ref = Arc::clone(&state);
    let png_data = tokio::task::spawn_blocking(move || {
        preview::generate_preview(&state_ref, &content, ext_owned.as_deref())
    })
    .await
    .map_err(|_| AppError::InternalMessage("preview generation failed"))?;

    let cached = Bytes::from(png_data);
    state.preview_cache.lock().put(cache_key, cached.clone());

    Ok(preview_response(cached))
}

fn build_paste_url(base_url: Option<&str>, headers: &HeaderMap, id: &str) -> String {
    format!("{}/{id}", request_origin(base_url, headers))
}

/// Origin (`scheme://host[prefix]`, no trailing slash) for building absolute
/// URLs. A configured BASE_URL is authoritative and avoids trusting
/// client-supplied X-Forwarded-* headers (which would otherwise let a caller
/// spoof the host in returned URLs); Config::from_env already validated and
/// normalized it.
fn request_origin(base_url: Option<&str>, headers: &HeaderMap) -> String {
    if let Some(base) = base_url {
        return base.to_string();
    }

    let host = forwarded_header(headers, "x-forwarded-host")
        .or_else(|| header_value(headers, header::HOST.as_str()))
        .unwrap_or("localhost");
    let proto = forwarded_header(headers, "x-forwarded-proto").unwrap_or("http");
    let prefix = forwarded_header(headers, "x-forwarded-prefix").unwrap_or("");

    format!("{proto}://{host}{prefix}")
}

fn forwarded_header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    header_value(headers, name)
        .and_then(|value| value.split(',').next())
        .map(str::trim)
}

fn header_value<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

fn render_cache_key(state: &AppState, id: &str, extension: Option<&str>) -> String {
    // The extension only changes rendered output when it selects markdown or
    // resolves to a syntax; anything else renders identically to "no
    // extension". Collapse those onto one slot so arbitrary URL extensions
    // (/id.a, /id.b, ...) can't each occupy a cache entry for the same paste,
    // and normalize with the same rules syntax resolution itself uses.
    let normalized = extension
        .and_then(highlighter::normalized_token)
        .filter(|ext| {
            highlighter::is_markdown(Some(ext.as_ref()))
                || highlighter::resolve_syntax(state, Some(ext.as_ref())).is_some()
        });

    match normalized {
        Some(extension) => format!("{id}:{extension}"),
        None => id.to_string(),
    }
}

fn preview_response(bytes: Bytes) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "image/png")
        .header(header::CACHE_CONTROL, "public, max-age=86400, immutable")
        .body(Body::from(bytes))
        .expect("failed to build preview response")
}

use nanoid::nanoid;
use sqlx::{SqlitePool, migrate::Migrator};
use time::OffsetDateTime;

use crate::{
    error::AppError,
    state::{AppResult, CreatePasteForm, PasteMeta},
};

static MIGRATOR: Migrator = sqlx::migrate!();

pub async fn migrate_db(db: &SqlitePool) -> Result<(), sqlx::migrate::MigrateError> {
    MIGRATOR.run(db).await
}

pub async fn insert_paste(db: &SqlitePool, form: CreatePasteForm) -> AppResult<String> {
    let content = form
        .content
        .ok_or(AppError::UnprocessableEntity("Can't paste empty input!"))?;

    if content.is_empty() {
        return Err(AppError::UnprocessableEntity("Can't paste empty input!"));
    }

    let expires_at = parse_expiry(form.expires_in.as_deref().unwrap_or("never"))
        .ok_or_else(|| AppError::BadRequest("Invalid expiry option.".into()))?;

    let id = nanoid!(10);
    let now = now_timestamp();
    let language = form.language;

    sqlx::query!(
        r#"
        INSERT INTO pastes (id, language, content, created_at, expires_at)
        VALUES (?1, ?2, ?3, ?4, ?5)
        "#,
        id,
        language,
        content,
        now,
        expires_at
    )
    .execute(db)
    .await
    .map_err(AppError::Internal)?;

    Ok(id)
}

pub async fn load_paste_by_ref(db: &SqlitePool, paste_ref: &str) -> AppResult<Option<String>> {
    let (id, _) = split_paste_ref(paste_ref);
    load_paste_content(db, id).await
}

/// Split a paste reference such as `"abc123.rs"` into its id and optional
/// extension, only splitting on the final `.` when both sides are non-empty.
/// Shared by the loader and the handlers so id/extension parsing stays
/// consistent across routes.
pub fn split_paste_ref(paste_ref: &str) -> (&str, Option<&str>) {
    match paste_ref.rsplit_once('.') {
        Some((id, ext)) if !id.is_empty() && !ext.is_empty() => (id, Some(ext)),
        _ => (paste_ref, None),
    }
}

/// Load a paste's id/language plus a short content head, leaving the full
/// content column unread. Paste views consult the render/preview caches before
/// they need content, so cache hits skip pulling multi-MB rows out of SQLite.
///
/// Uses the runtime query API (not the `query_as!` macro) so it doesn't
/// require regenerating the offline `.sqlx` metadata.
pub async fn load_paste_meta_by_ref(
    db: &SqlitePool,
    paste_ref: &str,
) -> AppResult<Option<PasteMeta>> {
    let (id, _) = split_paste_ref(paste_ref);
    let now = now_timestamp();

    sqlx::query_as::<_, PasteMeta>(
        r#"
        SELECT id, language, substr(content, 1, 16) AS head
        FROM pastes
        WHERE id = ?1
          AND (expires_at IS NULL OR expires_at > ?2)
        "#,
    )
    .bind(id)
    .bind(now)
    .fetch_optional(db)
    .await
    .map_err(AppError::Internal)
}

/// Load only the content column; id/language come from
/// [`load_paste_meta_by_ref`], so the big column is read solely where it is
/// actually rendered or served.
pub async fn load_paste_content(db: &SqlitePool, id: &str) -> AppResult<Option<String>> {
    let now = now_timestamp();

    sqlx::query_scalar::<_, String>(
        r#"
        SELECT content
        FROM pastes
        WHERE id = ?1
          AND (expires_at IS NULL OR expires_at > ?2)
        "#,
    )
    .bind(id)
    .bind(now)
    .fetch_optional(db)
    .await
    .map_err(AppError::Internal)
}

pub fn sanitize_form(mut form: CreatePasteForm) -> CreatePasteForm {
    form.expires_in = Some(
        form.expires_in
            .unwrap_or_else(|| "never".into())
            .trim()
            .to_string(),
    );
    // Truncate in place instead of `.to_string()` — content can be megabytes
    // and almost never has trailing carriage returns to strip.
    if let Some(content) = form.content.as_mut() {
        let trimmed_len = content.trim_end_matches('\r').len();
        content.truncate(trimmed_len);
    }
    form.filename = form.filename.map(|value| value.trim().to_string());
    form.language = form.language.map(|value| value.trim().to_ascii_lowercase());
    form
}

fn parse_expiry(value: &str) -> Option<Option<i64>> {
    match value {
        "never" | "" => Some(None),
        seconds => seconds
            .parse::<i64>()
            .ok()
            .filter(|seconds| *seconds > 0)
            .and_then(|seconds| now_timestamp().checked_add(seconds))
            .map(Some),
    }
}

fn now_timestamp() -> i64 {
    OffsetDateTime::now_utc().unix_timestamp()
}

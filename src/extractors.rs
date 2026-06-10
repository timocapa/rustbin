use axum::extract::Multipart;

use crate::{
    error::bad_request,
    state::{AppResult, CreatePasteForm},
};

pub async fn parse_create_paste_multipart(mut multipart: Multipart) -> AppResult<CreatePasteForm> {
    let mut form = CreatePasteForm::default();

    while let Some(field) = multipart.next_field().await.map_err(bad_request)? {
        let name = field.name().unwrap_or_default().to_string();
        match name.as_str() {
            "file" => {
                let filename = field.file_name().map(str::to_string);
                let value = field.text().await.map_err(bad_request)?;
                // Only take the filename when this field actually supplies
                // content; the name of a discarded empty upload must not drive
                // language detection for content from another field.
                if !value.is_empty() {
                    form.filename = filename;
                }
                set_content(&mut form, value);
            }
            "content" => {
                form.from_browser = true;
                let value = field.text().await.map_err(bad_request)?;
                set_content(&mut form, value);
            }
            "expires_in" => {
                let value = field.text().await.map_err(bad_request)?;
                form.expires_in = Some(value);
            }
            _ => {
                let _ = field.bytes().await.map_err(bad_request)?;
            }
        }
    }

    if form.content.is_none() {
        return Err(crate::error::AppError::BadRequest(
            "Missing multipart file field `file`.".into(),
        ));
    }

    Ok(form)
}

/// Assign paste content, but never let an empty value (e.g. an empty file
/// upload) overwrite content already provided by another field. Field order in
/// the multipart body therefore can't silently discard the real paste.
fn set_content(form: &mut CreatePasteForm, value: String) {
    if !value.is_empty() || form.content.is_none() {
        form.content = Some(value);
    }
}

use std::{collections::HashMap, sync::Arc};

use bytes::Bytes;
use fontdue::Font;
use lru::LruCache;
use parking_lot::Mutex;
use sqlx::SqlitePool;
use syntect::{highlighting::Theme, parsing::SyntaxSet};

use crate::error::AppError;

#[derive(Clone)]
pub struct AppState {
    pub db: SqlitePool,
    pub syntax_set: Arc<SyntaxSet>,
    pub syntax_index_by_token: Arc<HashMap<String, usize>>,
    pub classifier_max_bytes: usize,
    pub highlight_max_bytes: usize,
    /// Rendered HTML larger than this is served but not cached, so a few huge
    /// pastes can't own the entire count-bounded LRU's memory.
    pub render_cache_max_entry_bytes: usize,
    pub render_cache: Arc<Mutex<LruCache<String, Arc<str>>>>,
    pub preview_cache: Arc<Mutex<LruCache<String, Bytes>>>,
    /// Single-flight locks so concurrent cache misses for the same paste
    /// render once instead of N times.
    pub render_locks: Arc<KeyedMutex>,
    pub preview_locks: Arc<KeyedMutex>,
    pub theme: Arc<Theme>,
    pub font: Arc<Font>,
    /// Canonical base URL (from `BASE_URL`) used when building paste URLs. When
    /// set, it takes precedence over request `X-Forwarded-*`/`Host` headers.
    pub base_url: Option<String>,
}

/// Per-key async mutexes for single-flight computation: the first task to take
/// a key's lock does the work, concurrent tasks for the same key wait on it and
/// then find the result in cache. Slots are removed once no task holds them.
#[derive(Default)]
pub struct KeyedMutex {
    slots: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

pub struct KeyedMutexGuard<'a> {
    owner: &'a KeyedMutex,
    key: String,
    _guard: tokio::sync::OwnedMutexGuard<()>,
}

impl KeyedMutex {
    pub async fn lock(&self, key: &str) -> KeyedMutexGuard<'_> {
        let slot = {
            let mut slots = self.slots.lock();
            Arc::clone(slots.entry(key.to_string()).or_default())
        };
        let guard = slot.lock_owned().await;
        KeyedMutexGuard {
            owner: self,
            key: key.to_string(),
            _guard: guard,
        }
    }

    fn release(&self, key: &str) {
        let mut slots = self.slots.lock();
        if let Some(slot) = slots.get(key) {
            // Two strong refs mean only the map and the guard being dropped
            // still reference the slot — no other task is using or awaiting it.
            if Arc::strong_count(slot) <= 2 {
                slots.remove(key);
            }
        }
    }
}

impl Drop for KeyedMutexGuard<'_> {
    fn drop(&mut self) {
        self.owner.release(&self.key);
    }
}

#[derive(Debug, Default)]
pub struct CreatePasteForm {
    pub expires_in: Option<String>,
    pub filename: Option<String>,
    pub language: Option<String>,
    pub content: Option<String>,
    pub from_browser: bool,
}

/// Lightweight row for paste views: everything the cache-hit path needs
/// without reading the (potentially multi-MB) content column.
#[derive(Debug, sqlx::FromRow)]
pub struct PasteMeta {
    pub id: String,
    pub language: Option<String>,
    /// First few characters of the content, enough to rule out URL pastes.
    pub head: String,
}

pub type AppResult<T> = Result<T, AppError>;

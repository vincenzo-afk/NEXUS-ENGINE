use serde::{Deserialize, Serialize};
use wasm_bindgen::prelude::*;

/// Request payload sent from the main thread to a search worker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerSearchRequest {
    pub query: String,
    pub limit: usize,
    pub offset: usize,
}

/// Response payload sent from the worker back to the main thread.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerSearchResponse {
    pub results: Vec<WorkerSearchResult>,
    pub total_count: usize,
}

/// A single search result within a worker response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerSearchResult {
    pub doc_id: u32,
    pub path: String,
    pub file_name: String,
    pub size_bytes: u64,
    pub modified_unix: i64,
    pub match_count: usize,
    pub score: f32,
}

/// Spawn a web worker from JavaScript source code. The `worker_code` string
/// is converted into a `Blob` URL to be used as the worker script source.
pub fn spawn_worker(worker_code: &str) -> Result<web_sys::Worker, JsValue> {
    let blob_init = web_sys::BlobPropertyBag::new();
    blob_init.set_type("application/javascript");

    let blob_parts = js_sys::Array::new();
    blob_parts.push(&JsValue::from_str(worker_code));

    let blob = web_sys::Blob::new_with_blob_sequence_and_options(&blob_parts, &blob_init)?;
    let url = web_sys::Url::create_object_url_with_blob(&blob)?;
    let worker = web_sys::Worker::new(&url)?;
    web_sys::Url::revoke_object_url(&url).ok();
    Ok(worker)
}

/// Send a search request to a worker by posting a JSON-serialized message.
pub fn send_search_request(
    worker: &web_sys::Worker,
    request: &WorkerSearchRequest,
) -> Result<(), JsValue> {
    let json = serde_json::to_string(request).map_err(|e| JsValue::from_str(&e.to_string()))?;
    worker.post_message(&JsValue::from_str(&json))
}

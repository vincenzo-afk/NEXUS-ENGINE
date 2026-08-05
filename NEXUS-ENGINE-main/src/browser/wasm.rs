use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;

use crate::autocomplete::Autocomplete;
use crate::config::RankingConfig;
use crate::document::{Document, DocumentMetadata};
use crate::index::Index;
use crate::query;
use crate::search;
use crate::spellcheck;

#[wasm_bindgen]
pub struct NexusWasm {
    index: Rc<RefCell<Index>>,
    ranking: RankingConfig,
}

#[wasm_bindgen]
impl NexusWasm {
    #[wasm_bindgen(constructor)]
    pub fn new() -> Self {
        console_error_panic_hook::set_once();
        NexusWasm {
            index: Rc::new(RefCell::new(Index::new())),
            ranking: RankingConfig::default(),
        }
    }

    /// Index a text document. `content` is the full text, `path` is a unique identifier.
    pub fn index(&self, content: &str, path: &str) -> Result<(), JsValue> {
        let file_name = Path::new(path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();
        let extension = Path::new(path)
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_lowercase())
            .unwrap_or_default();

        let metadata = DocumentMetadata {
            path: PathBuf::from(path),
            file_name,
            extension,
            size_bytes: content.len() as u64,
            modified_unix: chrono::Utc::now().timestamp(),
            token_count: 0,
        };

        let doc = Document {
            metadata,
            content: content.to_string(),
        };

        let mut idx = self.index.borrow_mut();
        idx.index_document(doc);
        Ok(())
    }

    /// Search the index. Returns a JSON array of result objects.
    pub fn search(&self, query_str: &str, limit: usize) -> Result<JsValue, JsValue> {
        let parsed =
            query::parse(query_str).map_err(|e| JsValue::from_str(&e.to_string()))?;

        let idx = self.index.borrow();
        let outcome = search::search(&idx, &parsed, &self.ranking, 0, limit, None, search::SearchMode::Both, None);
        let results = outcome.results;

        let output: Vec<serde_json::Value> = results
            .into_iter()
            .map(|r| {
                serde_json::json!({
                    "doc_id": r.doc_id,
                    "path": r.path.to_string_lossy(),
                    "file_name": r.file_name,
                    "size_bytes": r.size_bytes,
                    "modified_unix": r.modified_unix,
                    "match_count": r.match_count,
                    "score": r.score,
                })
            })
            .collect();

        let json =
            serde_json::to_string(&output).map_err(|e| JsValue::from_str(&e.to_string()))?;
        Ok(JsValue::from_str(&json))
    }

    /// Autocomplete suggestions for a prefix. Returns a JSON array of term strings.
    pub fn autocomplete(&self, prefix: &str, limit: usize) -> Result<JsValue, JsValue> {
        let idx = self.index.borrow();
        let mut freqs = std::collections::HashMap::new();
        for (term, id) in idx.vocabulary.iter() {
            if let Some(list) = idx.inverted.postings_for(id) {
                freqs.insert(term.to_string(), list.document_frequency() as u32);
            }
        }

        let ac = Autocomplete::build(&idx.vocabulary, &freqs);
        let suggestions = ac.suggest(prefix, limit);
        let json = serde_json::to_string(&suggestions)
            .map_err(|e| JsValue::from_str(&e.to_string()))?;
        Ok(JsValue::from_str(&json))
    }

    /// Index statistics as a JSON object.
    pub fn stats(&self) -> Result<JsValue, JsValue> {
        let idx = self.index.borrow();
        let s = serde_json::json!({
            "documents": idx.document_count(),
            "terms": idx.vocabulary.len(),
            "total_token_count": idx.inverted.total_token_count,
            "average_document_length": idx.inverted.average_document_length(),
        });
        let json =
            serde_json::to_string(&s).map_err(|e| JsValue::from_str(&e.to_string()))?;
        Ok(JsValue::from_str(&json))
    }

    /// Spelling suggestions for a term. Returns a JSON array of `{term, distance}` objects.
    pub fn suggest(&self, term: &str) -> Result<JsValue, JsValue> {
        let idx = self.index.borrow();
        let suggestions = spellcheck::suggest(term, &idx.vocabulary, 5);
        let output: Vec<serde_json::Value> = suggestions
            .into_iter()
            .map(|s| {
                serde_json::json!({
                    "term": s.term,
                    "distance": s.distance,
                })
            })
            .collect();

        let json =
            serde_json::to_string(&output).map_err(|e| JsValue::from_str(&e.to_string()))?;
        Ok(JsValue::from_str(&json))
    }

    /// Load the index from IndexedDB. Replaces the current in-memory index.
    pub fn load_indexed(&self, db_name: &str) -> js_sys::Promise {
        let db_name = db_name.to_string();
        let index_rc = self.index.clone();
        let future = async move {
            let load_promise =
                crate::browser::indexeddb::load(&db_name, "nexus_index", "index_data");
            let result: JsValue =
                JsFuture::from(load_promise).await.map_err(|e| {
                    JsValue::from_str(&format!("IndexedDB load failed: {:?}", e))
                })?;

            let uint8 = js_sys::Uint8Array::new(&result);
            let mut data = vec![0u8; uint8.length() as usize];
            uint8.copy_to(&mut data);

            let loaded: Index = bincode::deserialize(&data)
                .map_err(|e| JsValue::from_str(&format!("deserialize failed: {:?}", e)))?;

            let mut idx = index_rc.borrow_mut();
            *idx = loaded;
            Ok(JsValue::UNDEFINED)
        };
        wasm_bindgen_futures::future_to_promise(future)
    }

    /// Save the current index to IndexedDB.
    pub fn save_indexed(&self, db_name: &str) -> js_sys::Promise {
        let db_name = db_name.to_string();
        let data = {
            let idx = self.index.borrow();
            bincode::serialize(&*idx).expect("failed to serialize index for IndexedDB save")
        };
        let future = async move {
            let save_promise =
                crate::browser::indexeddb::save(&db_name, "nexus_index", "index_data", &data);
            JsFuture::from(save_promise).await.map_err(|e| {
                JsValue::from_str(&format!("IndexedDB save failed: {:?}", e))
            })?;
            Ok(JsValue::UNDEFINED)
        };
        wasm_bindgen_futures::future_to_promise(future)
    }
}

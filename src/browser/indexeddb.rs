//! Minimal IndexedDB persistence for the browser build.
//!
//! Implemented with the **untyped** `web_sys` API (raw `JsValue` /
//! `js_sys::Promise` casts) rather than the typed `Idb*` wrappers: the
//! typed wrappers each require a separate web-sys feature flag, and the
//! flags needed here (`IdbRequest`, `IdbObjectStore`, `IdbTransaction`,
//! `IdbOpenDbRequest` plus their method-gating counterparts) are easy to
//! miss or misconfigure, which historically broke this module on the
//! `wasm32-unknown-unknown` target. The untyped approach compiles with
//! only the `IdbFactory` feature and a few core DOM features, at the
//! cost of losing compile-time method checks — acceptable for a small,
//! self-contained persistence shim whose behavior is exercised at
//! runtime in the browser.

use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;

fn idb_factory() -> Result<web_sys::IdbFactory, JsValue> {
    web_sys::window()
        .ok_or_else(|| JsValue::from_str("no window global"))?
        .indexed_db()
        .map_err(|_| JsValue::from_str("IndexedDB not available"))?
        .ok_or_else(|| JsValue::from_str("IndexedDB not supported in this browser"))
}

/// Open (or create) an IndexedDB database, ensuring `store_name` exists.
///
/// Returns a raw `js_sys::Promise` that resolves to an untyped `JsValue`
/// holding the `IDBDatabase` connection.
fn open(db_name: &str, store_name: &str) -> js_sys::Promise {
    let db_name = db_name.to_string();
    let store_name = store_name.to_string();
    let future = async move {
        let factory = idb_factory()?;
        // Untyped: `factory.open()` returns `Result<js_sys::Promise, JsValue>`
        // when the typed `IdbOpenDbRequest` feature is not enabled.
        let open_promise: js_sys::Promise = factory.open(&db_name)?;

        let upgrade_cb = {
            let store_name = store_name.clone();
            Closure::wrap(Box::new(move |event: web_sys::Event| {
                // `event.target` during `onupgradeneeded` is the
                // `IDBOpenDBRequest`; its `result` is the new database.
                let request: JsValue = event
                    .target()
                    .expect("event has target")
                    .unchecked_into();
                let db: JsValue = js_sys::Reflect::get(&request, &JsValue::from_str("result"))
                    .expect("request has result");
                if !js_sys::Reflect::get(
                    &db,
                    &JsValue::from_str("objectStoreNames"),
                )
                .map(|names: JsValue| names.unchecked_into::<js_sys::Array>().includes(&JsValue::from_str(&store_name), 0))
                .unwrap_or(false)
                {
                    js_sys::Reflect::get(&db, &JsValue::from_str("createObjectStore"))
                        .expect("db has createObjectStore")
                        .unchecked_into::<js_sys::Function>()
                        .call1(&db, &JsValue::from_str(&store_name))
                        .expect("failed to create object store");
                }
            }) as Box<dyn FnMut(_)>)
        };

        let request = open_promise
            .unchecked_ref::<js_sys::Object>()
            .clone();
        let _ = js_sys::Reflect::set(
            &request,
            &JsValue::from_str("onupgradeneeded"),
            upgrade_cb.as_ref(),
        );

        let result: JsValue = JsFuture::from(open_promise).await?;
        // Keep the closure alive until the connection is established,
        // then it's safe to drop (the upgrade handler won't fire again).
        drop(upgrade_cb);
        Ok(result)
    };
    wasm_bindgen_futures::future_to_promise(future)
}

/// Save serialized data to an object store.
/// Returns a Promise that resolves with `undefined` on success.
pub fn save(db_name: &str, store_name: &str, key: &str, data: &[u8]) -> js_sys::Promise {
    let db_name = db_name.to_string();
    let store_name = store_name.to_string();
    let key = key.to_string();
    let data = data.to_vec();

    let open_promise = open(&db_name, &store_name);
    let future = async move {
        let db: JsValue = JsFuture::from(open_promise)
            .await
            .map_err(|e| JsValue::from_str(&format!("IndexedDB open error: {:?}", e)))?;

        // untyped `db.transaction(storeName, "readwrite")`
        let transaction: JsValue = js_sys::Reflect::get(&db, &JsValue::from_str("transaction"))
            .map_err(|_| JsValue::from_str("db has no transaction method"))?
            .unchecked_into::<js_sys::Function>()
            .call2(
                &db,
                &JsValue::from_str(&store_name),
                &JsValue::from_str("readwrite"),
            )
            .map_err(|e| JsValue::from_str(&format!("failed to create transaction: {:?}", e)))?;

        // untyped `transaction.objectStore(storeName)`
        let store: JsValue = js_sys::Reflect::get(&transaction, &JsValue::from_str("objectStore"))
            .map_err(|_| JsValue::from_str("transaction has no objectStore method"))?
            .unchecked_into::<js_sys::Function>()
            .call1(&transaction, &JsValue::from_str(&store_name))
            .map_err(|e| JsValue::from_str(&format!("failed to get object store: {:?}", e)))?;

        let js_data = js_sys::Uint8Array::from(&data[..]);
        // untyped `store.put(data, key)`
        let put_promise: js_sys::Promise = js_sys::Reflect::get(&store, &JsValue::from_str("put"))
            .map_err(|_| JsValue::from_str("store has no put method"))?
            .unchecked_into::<js_sys::Function>()
            .call2(&store, &js_data, &JsValue::from_str(&key))
            .map_err(|e| JsValue::from_str(&format!("failed to put data: {:?}", e)))?
            .into();

        JsFuture::from(put_promise)
            .await
            .map_err(|e| JsValue::from_str(&format!("IndexedDB put failed: {:?}", e)))?;

        // untyped `db.close()`
        let _ = js_sys::Reflect::get(&db, &JsValue::from_str("close"))
            .and_then(|c| c.unchecked_into::<js_sys::Function>().call0(&db));

        Ok(JsValue::UNDEFINED)
    };
    wasm_bindgen_futures::future_to_promise(future)
}

/// Load serialized data from an object store.
/// Returns a Promise that resolves with the data as a `Uint8Array`.
pub fn load(db_name: &str, store_name: &str, key: &str) -> js_sys::Promise {
    let db_name = db_name.to_string();
    let store_name = store_name.to_string();
    let key = key.to_string();

    let open_promise = open(&db_name, &store_name);
    let future = async move {
        let db: JsValue = JsFuture::from(open_promise)
            .await
            .map_err(|e| JsValue::from_str(&format!("IndexedDB open error: {:?}", e)))?;

        let transaction: JsValue = js_sys::Reflect::get(&db, &JsValue::from_str("transaction"))
            .map_err(|_| JsValue::from_str("db has no transaction method"))?
            .unchecked_into::<js_sys::Function>()
            .call2(
                &db,
                &JsValue::from_str(&store_name),
                &JsValue::from_str("readonly"),
            )
            .map_err(|e| JsValue::from_str(&format!("failed to create transaction: {:?}", e)))?;

        let store: JsValue = js_sys::Reflect::get(&transaction, &JsValue::from_str("objectStore"))
            .map_err(|_| JsValue::from_str("transaction has no objectStore method"))?
            .unchecked_into::<js_sys::Function>()
            .call1(&transaction, &JsValue::from_str(&store_name))
            .map_err(|e| JsValue::from_str(&format!("failed to get object store: {:?}", e)))?;

        // untyped `store.get(key)`
        let get_promise: js_sys::Promise = js_sys::Reflect::get(&store, &JsValue::from_str("get"))
            .map_err(|_| JsValue::from_str("store has no get method"))?
            .unchecked_into::<js_sys::Function>()
            .call1(&store, &JsValue::from_str(&key))
            .map_err(|e| JsValue::from_str(&format!("failed to get data: {:?}", e)))?
            .into();

        let result: JsValue = JsFuture::from(get_promise)
            .await
            .map_err(|e| JsValue::from_str(&format!("IndexedDB get failed: {:?}", e)))?;

        if result.is_undefined() || result.is_null() {
            let _ = js_sys::Reflect::get(&db, &JsValue::from_str("close"))
                .and_then(|c| c.unchecked_into::<js_sys::Function>().call0(&db));
            return Err(JsValue::from_str("key not found in IndexedDB"));
        }

        let uint8 = js_sys::Uint8Array::new(&result);
        let js_buf = js_sys::Uint8Array::new_with_length(uint8.length());
        js_buf.set(&uint8, 0);

        let _ = js_sys::Reflect::get(&db, &JsValue::from_str("close"))
            .and_then(|c| c.unchecked_into::<js_sys::Function>().call0(&db));

        Ok(js_buf.into())
    };
    wasm_bindgen_futures::future_to_promise(future)
}

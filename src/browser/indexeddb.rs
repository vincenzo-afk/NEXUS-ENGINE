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

/// Wrap an `IDBRequest` (e.g. from `store.put()` / `store.get()`) in a
/// hand-built `Promise`. `IDBRequest` is **not** a thenable `Promise`, so
/// converting it with `.into()` and awaiting via `JsFuture` would hang
/// forever; drive it with `onsuccess` / `onerror` handlers instead.
fn wrap_request(request: &JsValue) -> js_sys::Promise {
    js_sys::Promise::new(&mut move |resolve, reject| {
        let req = request.clone();
        let err_cb = Closure::wrap(Box::new(move |event: web_sys::Event| {
            let msg = js_sys::Reflect::get(&event, &JsValue::from_str("target"))
                .ok()
                .and_then(|t: JsValue| js_sys::Reflect::get(&t, &JsValue::from_str("error")).ok())
                .map(|err| format!("{:?}", err))
                .unwrap_or_else(|| "IndexedDB request error".to_string());
            reject.call1(&JsValue::NULL, &JsValue::from_str(&msg));
        }) as Box<dyn FnMut(_)>);
        let _ = js_sys::Reflect::set(&req, &JsValue::from_str("onerror"), err_cb.as_ref());
        err_cb.forget();

        let req_ok = request.clone();
        let ok_cb = Closure::wrap(Box::new(move |event: web_sys::Event| {
            let target: JsValue = event
                .target()
                .expect("event has target")
                .unchecked_into();
            let result = js_sys::Reflect::get(&target, &JsValue::from_str("result"))
                .unwrap_or(JsValue::UNDEFINED);
            let _ = resolve.call1(&JsValue::NULL, &result);
        }) as Box<dyn FnMut(_)>);
        let _ = js_sys::Reflect::set(&req_ok, &JsValue::from_str("onsuccess"), ok_cb.as_ref());
        ok_cb.forget();
    })
}

/// Open (or create) an IndexedDB database, ensuring `store_name` exists.
///
/// Returns a `js_sys::Promise` that resolves to an untyped `JsValue`
/// holding the `IDBDatabase` connection.
///
/// Important implementation detail: `factory.open()` returns an
/// `IDBOpenDBRequest`, which is **not** a thenable `Promise` — wrapping
/// it with `JsFuture::from` would therefore never resolve. Instead we
/// drive the request with explicit `onsuccess` / `onerror` handlers and
/// resolve a hand-built `js_sys::Promise` when the connection opens.
fn open(db_name: &str, store_name: &str) -> js_sys::Promise {
    let db_name = db_name.to_string();
    let store_name = store_name.to_string();

    let factory = match idb_factory() {
        Ok(f) => f,
        Err(e) => {
            return js_sys::Promise::reject(&e);
        }
    };

    let request: JsValue = match factory.open(&db_name) {
        Ok(r) => r.unchecked_into(),
        Err(e) => {
            return js_sys::Promise::reject(&e);
        }
    };

    // Single hand-built promise whose executor installs all three event
    // handlers on the `IDBOpenDBRequest`. The crucial detail is that
    // every handler closure is kept alive with `.forget()` — if the
    // `Closure` wrapper is dropped before the event fires, wasm-bindgen
    // unregisters the callback and the open request hangs forever.
    js_sys::Promise::new(&mut move |resolve, reject| {
        // `onupgradeneeded`: create the object store if it doesn't exist yet.
        let store_name_for_upgrade = store_name.clone();
        let upgrade_cb = Closure::wrap(Box::new(move |event: web_sys::Event| {
            let req: JsValue = event
                .target()
                .expect("event has target")
                .unchecked_into();
            let db: JsValue = js_sys::Reflect::get(&req, &JsValue::from_str("result"))
                .expect("request has result");
            // `objectStoreNames` is a `DOMStringList`, not a JS Array —
            // calling `Array.prototype.includes` on it throws a TypeError,
            // which aborts the versionchange transaction. Use its own
            // `contains()` method instead.
            let has_store = js_sys::Reflect::get(&db, &JsValue::from_str("objectStoreNames"))
                .ok()
                .and_then(|names: JsValue| {
                    js_sys::Reflect::get(&names, &JsValue::from_str("contains")).ok().and_then(
                        |contains_fn: JsValue| {
                            contains_fn
                                .unchecked_into::<js_sys::Function>()
                                .call1(&names, &JsValue::from_str(&store_name_for_upgrade))
                                .ok()
                                .map(|v| v.is_truthy())
                        },
                    )
                })
                .unwrap_or(false);
            if !has_store {
                js_sys::Reflect::get(&db, &JsValue::from_str("createObjectStore"))
                    .expect("db has createObjectStore")
                    .unchecked_into::<js_sys::Function>()
                    .call1(&db, &JsValue::from_str(&store_name_for_upgrade))
                    .expect("failed to create object store");
            }
        }) as Box<dyn FnMut(_)>);
        let _ = js_sys::Reflect::set(
            &request,
            &JsValue::from_str("onupgradeneeded"),
            upgrade_cb.as_ref(),
        );
        upgrade_cb.forget();

        // `onerror` on the request rejects the wrapper promise.
        let request_for_err = request.clone();
        let err_cb = Closure::wrap(Box::new(move |event: web_sys::Event| {
            let msg = js_sys::Reflect::get(&event, &JsValue::from_str("target"))
                .ok()
                .and_then(|t: JsValue| js_sys::Reflect::get(&t, &JsValue::from_str("error")).ok())
                .map(|err| format!("{:?}", err))
                .unwrap_or_else(|| "IndexedDB request error".to_string());
            // `IDBOpenDBRequest` keeps its last error on `request.error`.
            let detail = js_sys::Reflect::get(&request_for_err, &JsValue::from_str("error"))
                .ok()
                .map(|e| format!("{:?}", e))
                .unwrap_or_default();
            reject.call1(&JsValue::NULL, &JsValue::from_str(&format!("{} {}", msg, detail)));
        }) as Box<dyn FnMut(_)>);
        let _ = js_sys::Reflect::set(&request, &JsValue::from_str("onerror"), err_cb.as_ref());
        err_cb.forget();

        // `onsuccess`: resolve with the new database connection.
        let request_for_ok = request.clone();
        let inner: Closure<dyn FnMut(JsValue)> = Closure::wrap(Box::new(move |_: JsValue| {
            let db = js_sys::Reflect::get(&request_for_ok, &JsValue::from_str("result"))
                .unwrap_or(JsValue::UNDEFINED);
            let _ = resolve.call1(&JsValue::NULL, &db);
        }) as Box<dyn FnMut(JsValue)>);
        let _ = js_sys::Reflect::set(&request, &JsValue::from_str("onsuccess"), inner.as_ref());
        inner.forget();
    })
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
        // untyped `store.put(data, key)` — returns an `IDBRequest`, NOT a
        // Promise; wrap it with explicit onsuccess/onerror handlers.
        let put_request: JsValue = js_sys::Reflect::get(&store, &JsValue::from_str("put"))
            .map_err(|_| JsValue::from_str("store has no put method"))?
            .unchecked_into::<js_sys::Function>()
            .call2(&store, &js_data, &JsValue::from_str(&key))
            .map_err(|e| JsValue::from_str(&format!("failed to put data: {:?}", e)))?;

        JsFuture::from(wrap_request(&put_request))
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

        // untyped `store.get(key)` — returns an `IDBRequest`, NOT a
        // Promise; wrap it with explicit onsuccess/onerror handlers.
        let get_request: JsValue = js_sys::Reflect::get(&store, &JsValue::from_str("get"))
            .map_err(|_| JsValue::from_str("store has no get method"))?
            .unchecked_into::<js_sys::Function>()
            .call1(&store, &JsValue::from_str(&key))
            .map_err(|e| JsValue::from_str(&format!("failed to get data: {:?}", e)))?;

        let result: JsValue = JsFuture::from(wrap_request(&get_request))
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

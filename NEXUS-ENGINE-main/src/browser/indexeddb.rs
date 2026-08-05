use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    IdbDatabase, IdbFactory, IdbObjectStore, IdbOpenDbRequest, IdbRequest, IdbTransaction,
    IdbTransactionMode,
};

fn idb_factory() -> Result<IdbFactory, JsValue> {
    web_sys::window()
        .ok_or_else(|| JsValue::from_str("no window global"))?
        .indexed_db()
        .map_err(|_| JsValue::from_str("IndexedDB not available"))?
        .ok_or_else(|| JsValue::from_str("IndexedDB not supported in this browser"))
}

/// Open (or create) an IndexedDB database, ensuring `store_name` exists.
fn open(db_name: &str, store_name: &str) -> js_sys::Promise {
    let db_name = db_name.to_string();
    let store_name = store_name.to_string();
    let future = async move {
        let factory = idb_factory()?;
        let open_request: IdbOpenDbRequest = factory.open(&db_name)?;

        let upgrade_cb = {
            let store_name = store_name.clone();
            Closure::wrap(Box::new(move |event: web_sys::Event| {
                let request: &IdbOpenDbRequest =
                    event.target().expect("event has target").unchecked_ref();
                let db: IdbDatabase = request.result().unchecked_into();
                if !db.object_store_names().contains(&store_name) {
                    db.create_object_store(&store_name)
                        .expect("failed to create object store");
                }
            }) as Box<dyn FnMut(_)>)
        };
        open_request.set_onupgradeneeded(Some(upgrade_cb.as_ref().unchecked_ref()));

        let result: JsValue = JsFuture::from(open_request).await?;
        let db: IdbDatabase = result.unchecked_into();
        // The closure is kept alive until the DB connection is fully established,
        // after which it's safe to drop since the upgrade handler won't fire again.
        drop(upgrade_cb);
        Ok(db)
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
        let db: IdbDatabase = JsFuture::from(open_promise)
            .await
            .map_err(|e| JsValue::from_str(&format!("IndexedDB open error: {:?}", e)))?
            .unchecked_into();

        let transaction = db
            .transaction_with_string_and_mode(&store_name, IdbTransactionMode::Readwrite)
            .map_err(|e| JsValue::from_str(&format!("failed to create transaction: {:?}", e)))?;
        let store = transaction
            .object_store(&store_name)
            .map_err(|e| JsValue::from_str(&format!("failed to get object store: {:?}", e)))?;

        let js_data = js_sys::Uint8Array::from(&data[..]);
        let put_request = store
            .put_with_key(&js_data, &JsValue::from_str(&key))
            .map_err(|e| JsValue::from_str(&format!("failed to put data: {:?}", e)))?;

        JsFuture::from(put_request)
            .await
            .map_err(|e| JsValue::from_str(&format!("IndexedDB put failed: {:?}", e)))?;

        db.close();
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
        let db: IdbDatabase = JsFuture::from(open_promise)
            .await
            .map_err(|e| JsValue::from_str(&format!("IndexedDB open error: {:?}", e)))?
            .unchecked_into();

        let transaction = db
            .transaction_with_string_and_mode(&store_name, IdbTransactionMode::Readonly)
            .map_err(|e| JsValue::from_str(&format!("failed to create transaction: {:?}", e)))?;
        let store = transaction
            .object_store(&store_name)
            .map_err(|e| JsValue::from_str(&format!("failed to get object store: {:?}", e)))?;

        let get_request = store
            .get(&JsValue::from_str(&key))
            .map_err(|e| JsValue::from_str(&format!("failed to get data: {:?}", e)))?;

        let result: JsValue = JsFuture::from(get_request)
            .await
            .map_err(|e| JsValue::from_str(&format!("IndexedDB get failed: {:?}", e)))?;

        if result.is_undefined() || result.is_null() {
            db.close();
            return Err(JsValue::from_str("key not found in IndexedDB"));
        }

        let uint8 = js_sys::Uint8Array::new(&result);
        let js_buf = js_sys::Uint8Array::new_with_length(uint8.length());
        js_buf.set(&uint8, 0);

        db.close();
        Ok(js_buf.into())
    };
    wasm_bindgen_futures::future_to_promise(future)
}

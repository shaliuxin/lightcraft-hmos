//! Platform request/response bridge. The Rust render worker never touches the ArkTS thread;
//! it posts platform requests through this channel and blocks on the worker until the ArkTS
//! side answers (or the app is torn down).
use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    mpsc,
    Mutex, OnceLock,
};

pub type Notify = extern "C" fn(u64, *const u8, usize, *const u8, usize);
static NOTIFY: OnceLock<Notify> = OnceLock::new();
static NEXT: AtomicU64 = AtomicU64::new(1);
static CANCELLED: AtomicBool = AtomicBool::new(false);
static PENDING: OnceLock<Mutex<HashMap<u64, mpsc::Sender<Reply>>>> = OnceLock::new();
pub type Reply = Result<(String, Vec<u8>), String>;

fn pending() -> &'static Mutex<HashMap<u64, mpsc::Sender<Reply>>> {
    PENDING.get_or_init(Default::default)
}

pub fn init(notify: Notify) {
    let _ = NOTIFY.set(notify);
}

pub fn notify(id: u64, kind: &str, text: &str, data: &[u8]) {
    if let Some(f) = NOTIFY.get() {
        let message = serde_json::json!({ "kind": kind, "text": text }).to_string();
        f(id, message.as_ptr(), message.len(), data.as_ptr(), data.len());
    }
}

pub fn request(kind: &str, text: &str, data: &[u8]) -> Reply {
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let (tx, rx) = mpsc::channel();
    {
        let mut map = pending().lock().unwrap_or_else(|p| p.into_inner());
        if CANCELLED.load(Ordering::Acquire) {
            return Err("cancelled".into());
        }
        map.insert(id, tx);
    }
    notify(id, kind, text, data);
    rx.recv().unwrap_or_else(|_| Err("platform service disconnected".into()))
}

pub fn reply(id: u64, result: Reply) {
    if let Some(tx) = pending()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .remove(&id)
    {
        let _ = tx.send(result);
    }
}

pub fn cancel() {
    let mut map = pending().lock().unwrap_or_else(|p| p.into_inner());
    CANCELLED.store(true, Ordering::Release);
    for (_, tx) in map.drain() {
        let _ = tx.send(Err("cancelled".into()));
    }
}

pub fn resume() {
    CANCELLED.store(false, Ordering::Release);
}

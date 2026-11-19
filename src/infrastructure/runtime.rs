use std::future::Future;
use std::sync::OnceLock;

use tokio::runtime::Runtime;
use tokio::sync::oneshot;

static TOKIO_RT: OnceLock<Runtime> = OnceLock::new();

/// Returns the shared multi-threaded Tokio runtime for background I/O and SDK calls.
pub fn runtime() -> &'static Runtime {
    TOKIO_RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name("stitch-tokio")
            .build()
            .expect("failed to initialize Stitch Tokio runtime")
    })
}

/// Spawns a future onto the shared Tokio runtime and awaits its result safely from GPUI's executor.
pub async fn spawn_tokio<F, T>(fut: F) -> anyhow::Result<T>
where
    F: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let (tx, rx) = oneshot::channel();
    runtime().spawn(async move {
        let output = fut.await;
        let _ = tx.send(output);
    });
    rx.await
        .map_err(|_| anyhow::anyhow!("background Tokio task cancelled before completing"))
}

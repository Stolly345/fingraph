//! The ledger lives on one thread (`Db` isn't thread-safe); everything else sends it work and waits for
//! the answer. One writer, in order, so there's nothing to merge.
use fingraph_core::db::Db;
use std::path::Path;
use std::sync::mpsc;
use tokio::sync::oneshot;

type Job = Box<dyn FnOnce(&Db) + Send>;

#[derive(Clone)]
pub struct Ledger {
    tx: mpsc::Sender<Job>,
}

impl Ledger {
    pub fn open(path: &Path) -> Result<Ledger, String> {
        let (tx, rx) = mpsc::channel::<Job>();
        let (ready_tx, ready_rx) = mpsc::channel();
        let path = path.to_path_buf();
        std::thread::Builder::new()
            .name("ledger".into())
            .spawn(move || {
                let db = match Db::open(&path) {
                    Ok(db) => {
                        let _ = ready_tx.send(Ok(()));
                        db
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(format!("couldn't open the ledger at {}: {e}", path.display())));
                        return;
                    }
                };
                for job in rx {
                    // a job that panics must not take the ledger down with it
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| job(&db)));
                }
            })
            .map_err(|e| e.to_string())?;
        ready_rx.recv().map_err(|e| e.to_string())??;
        Ok(Ledger { tx })
    }

    /// Runs `f` on the ledger's thread and returns its answer.
    pub async fn call<R: Send + 'static>(&self, f: impl FnOnce(&Db) -> R + Send + 'static) -> Result<R, String> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(Box::new(move |db| {
                let _ = tx.send(f(db));
            }))
            .map_err(|_| "the ledger has stopped".to_string())?;
        rx.await.map_err(|_| "the ledger stopped while working on that".to_string())
    }
}

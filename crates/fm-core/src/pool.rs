use std::sync::mpsc::{channel, sync_channel, Receiver, SyncSender};
use std::sync::{Arc, Mutex};

/// A tiny bounded worker pool. Returns the job sender and the result receiver.
/// Backpressure: `send` blocks when `queue` jobs are waiting. When every clone of the sender is
/// dropped the workers finish the queue and exit, which closes the result channel — so a consumer
/// can simply `for r in results {}`.
pub fn spawn_pool<J, R, F>(workers: usize, queue: usize, f: F) -> (SyncSender<J>, Receiver<R>)
where
    J: Send + 'static,
    R: Send + 'static,
    F: Fn(J) -> R + Send + Sync + 'static,
{
    let (jtx, jrx) = sync_channel::<J>(queue.max(1));
    let (rtx, rrx) = channel::<R>();
    let jrx = Arc::new(Mutex::new(jrx));
    let f = Arc::new(f);
    for _ in 0..workers.max(1) {
        let (jrx, rtx, f) = (jrx.clone(), rtx.clone(), f.clone());
        std::thread::spawn(move || loop {
            let job = jrx.lock().unwrap().recv();
            match job {
                Ok(j) => {
                    if rtx.send(f(j)).is_err() {
                        return;
                    }
                }
                Err(_) => return,
            }
        });
    }
    (jtx, rrx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn processes_all_jobs_and_closes() {
        let (tx, rx) = spawn_pool(4, 2, |x: u32| x * 2);
        let h = std::thread::spawn(move || {
            for i in 0..100 {
                tx.send(i).unwrap();
            }
        });
        let mut got: Vec<u32> = rx.iter().collect();
        h.join().unwrap();
        got.sort();
        assert_eq!(got, (0..100).map(|x| x * 2).collect::<Vec<_>>());
    }
}

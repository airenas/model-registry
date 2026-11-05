use std::io;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread;

/// Runs `work` for every item on up to `jobs` threads and returns the results in item order.
/// `done` is called on the calling thread as each item completes (for progress output).
/// After the first error no new items are started; the first error is returned once
/// the items already in flight have finished.
pub fn run<T, R, W>(
    items: &[T],
    jobs: usize,
    work: W,
    mut done: impl FnMut(&T, &R) -> io::Result<()>,
) -> Result<Vec<R>, String>
where
    T: Sync,
    R: Send,
    W: Fn(&T) -> Result<R, String> + Sync,
{
    let next = AtomicUsize::new(0);
    let failed = AtomicBool::new(false);
    let mut results: Vec<Option<R>> = items.iter().map(|_| None).collect();
    let mut first_err: Option<String> = None;

    thread::scope(|s| {
        let (tx, rx) = mpsc::channel();
        for _ in 0..jobs.clamp(1, items.len().max(1)) {
            let tx = tx.clone();
            let (next, failed, work) = (&next, &failed, &work);
            s.spawn(move || {
                while !failed.load(Ordering::SeqCst) {
                    let i = next.fetch_add(1, Ordering::SeqCst);
                    let Some(item) = items.get(i) else { break };
                    if tx.send((i, work(item))).is_err() {
                        break;
                    }
                }
            });
        }
        drop(tx);
        for (i, res) in rx {
            match res {
                Ok(r) => {
                    if let Err(e) = done(&items[i], &r) {
                        failed.store(true, Ordering::SeqCst);
                        first_err.get_or_insert(e.to_string());
                    }
                    results[i] = Some(r);
                }
                Err(e) => {
                    failed.store(true, Ordering::SeqCst);
                    first_err.get_or_insert(e);
                }
            }
        }
    });

    match first_err {
        Some(e) => Err(e),
        None => Ok(results.into_iter().flatten().collect()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn results_keep_item_order() {
        let items: Vec<u32> = (0..50).collect();
        let mut seen = 0;
        let res = run(
            &items,
            8,
            |i| Ok(i * 2),
            |_, _| {
                seen += 1;
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(res, (0..50).map(|i| i * 2).collect::<Vec<_>>());
        assert_eq!(seen, 50);
    }

    #[test]
    fn stops_after_error() {
        let items: Vec<u32> = (0..1000).collect();
        let started = AtomicUsize::new(0);
        let err = run(
            &items,
            2,
            |i| {
                started.fetch_add(1, Ordering::SeqCst);
                if *i == 3 {
                    Err("boom".to_string())
                } else {
                    Ok(())
                }
            },
            |_, _| Ok(()),
        )
        .unwrap_err();
        assert_eq!(err, "boom");
        assert!(started.load(Ordering::SeqCst) < 1000);
    }

    #[test]
    fn empty_input() {
        let items: Vec<u32> = vec![];
        assert!(
            run(&items, 4, |_| Ok(()), |_, _| Ok(()))
                .unwrap()
                .is_empty()
        );
    }
}

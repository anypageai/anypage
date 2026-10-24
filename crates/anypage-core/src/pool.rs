use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

type Task = Box<dyn FnOnce() + Send + 'static>;

struct Shared {
    state: Mutex<(VecDeque<Task>, bool)>,
    cv: Condvar,
}

/// Fixed-size worker pool. Tasks run in FIFO order; dropping the pool lets
/// queued tasks finish and joins every worker.
pub struct ThreadPool {
    shared: Arc<Shared>,
    handles: Vec<JoinHandle<()>>,
}

impl ThreadPool {
    pub fn new(threads: usize, name: &str) -> Self {
        let shared = Arc::new(Shared { state: Mutex::new((VecDeque::new(), false)), cv: Condvar::new() });
        let handles = (0..threads.max(1))
            .map(|i| {
                let sh = shared.clone();
                std::thread::Builder::new()
                    .name(format!("{name}-{i}"))
                    .spawn(move || loop {
                        let task = {
                            let mut st = sh.state.lock().unwrap();
                            loop {
                                if let Some(t) = st.0.pop_front() {
                                    break Some(t);
                                }
                                if st.1 {
                                    break None;
                                }
                                st = sh.cv.wait(st).unwrap();
                            }
                        };
                        match task {
                            Some(t) => t(),
                            None => return,
                        }
                    })
                    .expect("spawn pool worker")
            })
            .collect();
        Self { shared, handles }
    }

    pub fn spawn(&self, f: impl FnOnce() + Send + 'static) {
        self.shared.state.lock().unwrap().0.push_back(Box::new(f));
        self.shared.cv.notify_one();
    }

    pub fn threads(&self) -> usize {
        self.handles.len()
    }
}

impl Drop for ThreadPool {
    fn drop(&mut self) {
        self.shared.state.lock().unwrap().1 = true;
        self.shared.cv.notify_all();
        for h in self.handles.drain(..) {
            let _ = h.join();
        }
    }
}

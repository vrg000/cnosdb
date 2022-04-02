use crate::direct_io::File;
use crate::error::Error;
use crate::error::Result;
use core::slice;
use crossbeam::queue::ArrayQueue;
use futures::channel::oneshot::Sender as OnceSender;
use parking_lot::Condvar;
use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;

const QUEUE_SIZE: usize = 1 << 10;

pub struct IoTask {
    pub task_type: TaskType,
    priority: u32,
    ptr: *mut u8,
    size: usize,
    offset: u64,
    fd: Arc<File>,
    cb: OnceSender<Result<usize>>,
}

#[derive(PartialEq)]
pub enum TaskType {
    BackRead,
    BackWrite,
    FrontRead,
    FrontWrite,
    Wal,
}

unsafe impl Send for IoTask {}
unsafe impl Sync for IoTask {}

impl IoTask {
    pub fn run(self) {
        unsafe {
            match self.task_type {
                TaskType::BackRead | TaskType::FrontRead => {
                    let buf = slice::from_raw_parts_mut(self.ptr, self.size);
                    let ret = self
                        .fd
                        .read_at(self.offset, buf)
                        .map_err(|e| Error::IO { source: e });
                    let _ = self.cb.send(ret);
                }
                TaskType::BackWrite | TaskType::FrontWrite | TaskType::Wal => {
                    let buf = slice::from_raw_parts(self.ptr, self.size);
                    let ret = self
                        .fd
                        .write_at(self.offset, buf)
                        .map_err(|e| Error::IO { source: e });
                    let _ = self.cb.send(ret);
                }
            }
        }
    }
    pub fn is_pri_high(&self) -> bool {
        let ret = self.task_type == TaskType::FrontWrite
            || self.task_type == TaskType::FrontRead
            || self.task_type == TaskType::Wal
            || self.priority > 0;
        ret
    }
}

pub struct AsyncContext {
    pub read_queue: ArrayQueue<IoTask>,
    pub write_queue: ArrayQueue<IoTask>,
    pub high_op_queue: ArrayQueue<IoTask>,
    worker_count: AtomicUsize,
    total_count: usize,
    thread_state: Mutex<Vec<bool>>,
    // thread_pool:  Mutex<Vec<thread::JoinHandle<()>>>,
    thread_conv: Condvar,
    closed: AtomicBool,
}

impl AsyncContext {
    pub fn new(thread_num: usize) -> Self {
        Self {
            read_queue: ArrayQueue::new(QUEUE_SIZE),
            write_queue: ArrayQueue::new(QUEUE_SIZE),
            high_op_queue: ArrayQueue::new(QUEUE_SIZE),
            worker_count: AtomicUsize::new(thread_num),
            total_count: thread_num,
            thread_state: Mutex::new(vec![false; thread_num]),
            // thread_pool: Mutex::new(spawn_in_pool(thread_num)),
            thread_conv: Condvar::default(),
            closed: AtomicBool::new(false),
        }
    }
    pub fn wait(&self, id: usize) {
        let mut state = self.thread_state.lock();
        if !self.high_op_queue.is_empty()
            || !self.read_queue.is_empty()
            || !self.write_queue.is_empty()
        {
            return;
        }
        state[id] = true;
        self.worker_count.fetch_sub(1, Ordering::SeqCst);
        while state[id] {
            self.thread_conv.wait(&mut state);
        }
        self.worker_count.fetch_add(1, Ordering::SeqCst);
    }

    pub fn wake_up_one(&self) {
        if self.worker_count.load(Ordering::SeqCst) >= self.total_count {
            return;
        }

        let mut state = self.thread_state.lock();
        for iter in state.iter_mut() {
            if *iter == false {
                *iter = true;
                break;
            }
        }
        self.thread_conv.notify_all();
    }

    fn closed(&self) {
        if !self.closed.swap(true, Ordering::SeqCst) {
            while let Some(t) = self.read_queue.pop() {
                let _ = t.cb.send(Err(Error::Cancel));
            }
            while let Some(t) = self.write_queue.pop() {
                let _ = t.cb.send(Err(Error::Cancel));
            }
            while let Some(t) = self.high_op_queue.pop() {
                let _ = t.cb.send(Err(Error::Cancel));
            }
        }
    }
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    pub fn get_active_thread(&self) -> usize {
        self.worker_count.load(Ordering::Acquire)
    }

    pub fn add_task(&mut self, task: IoTask) -> Result<()> {
        if !self.is_closed() {
            return Err(Error::Cancel);
        }
        if task.is_pri_high() {
            let _ = self.high_op_queue.push(task);
        } else if task.task_type == TaskType::BackRead {
            let _ = self.read_queue.push(task);
        } else if task.task_type == TaskType::BackWrite {
            let _ = self.write_queue.push(task);
        }
        Ok(())
    }
}

pub fn run_io_task(async_rt: Arc<AsyncContext>, index: usize) {
    while !async_rt.is_closed() {
        while let Some(t) = async_rt.high_op_queue.pop() {
            t.run();
        }
        if let Some(t) = async_rt.read_queue.pop() {
            t.run();
        }
        if let Some(t) = async_rt.write_queue.pop() {
            t.run();
        }
        if let Some(t) = spin_for_task(&async_rt.high_op_queue) {
            t.run();
            continue;
        }
        async_rt.wait(index);
    }
}

pub fn spin_for_task(queue: &ArrayQueue<IoTask>) -> Option<IoTask> {
    for _ in 0..50 {
        if let Some(t) = queue.pop() {
            return Some(t);
        }
        thread::yield_now();
    }
    None
}

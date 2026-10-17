//! Image thumbnails, decoded on background threads so a slow source (USB
//! stick, SD card) never freezes the window. Only cards that are on screen
//! request a thumbnail; requests that scrolled out of view are dropped.

use crate::ntfs::{self, RecoveredFile};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Condvar, Mutex};

const WORKERS: usize = 2;
const MAX_PREVIEW_BYTES: u64 = 16 * 1024 * 1024;
const THUMB_PX: u32 = 256;

/// What a file card shows in its thumbnail area.
pub enum Thumb<'a> {
    /// Not an image: show the category icon.
    None,
    Loading,
    Image(&'a egui::TextureHandle),
    /// Decoding failed (data overwritten, not a real image, …).
    Failed,
}

enum State {
    Loading,
    Ready(egui::TextureHandle),
    Failed,
}

struct Job {
    generation: u64,
    idx: usize,
    source: String,
    rec: RecoveredFile,
}

type JobQueue = Arc<(Mutex<VecDeque<Job>>, Condvar)>;

pub struct PreviewLoader {
    queue: JobQueue,
    results: Receiver<(u64, usize, Option<egui::ColorImage>)>,
    generation: Arc<AtomicU64>,
    states: HashMap<usize, State>,
}

impl PreviewLoader {
    pub fn new(ctx: &egui::Context) -> Self {
        let queue: JobQueue = Arc::new((Mutex::new(VecDeque::new()), Condvar::new()));
        let generation = Arc::new(AtomicU64::new(0));
        let (tx, results) = channel();
        for _ in 0..WORKERS {
            let (queue, generation, tx, ctx) = (queue.clone(), generation.clone(), tx.clone(), ctx.clone());
            std::thread::spawn(move || worker(&queue, &generation, &tx, &ctx));
        }
        Self {
            queue,
            results,
            generation,
            states: HashMap::new(),
        }
    }

    /// Forgets every thumbnail (a new scan started).
    pub fn reset(&mut self) {
        self.generation.fetch_add(1, Ordering::Relaxed);
        self.queue.0.lock().unwrap().clear();
        self.states.clear();
    }

    /// Call once per frame before drawing cards: uploads finished thumbnails
    /// and drops queued requests, so only cards visible this frame load.
    pub fn begin_frame(&mut self, ctx: &egui::Context) {
        let current = self.generation.load(Ordering::Relaxed);
        while let Ok((generation, idx, image)) = self.results.try_recv() {
            if generation != current {
                continue;
            }
            let state = match image {
                Some(img) => State::Ready(ctx.load_texture(format!("preview_{idx}"), img, egui::TextureOptions::LINEAR)),
                None => State::Failed,
            };
            self.states.insert(idx, state);
        }
        let mut queue = self.queue.0.lock().unwrap();
        for job in queue.drain(..) {
            self.states.remove(&job.idx);
        }
    }

    pub fn thumb(&self, idx: usize) -> Thumb<'_> {
        match self.states.get(&idx) {
            Some(State::Ready(tex)) => Thumb::Image(tex),
            Some(State::Failed) => Thumb::Failed,
            Some(State::Loading) | None => Thumb::Loading,
        }
    }

    /// Queues a thumbnail for a card that is on screen (no-op if known).
    pub fn request(&mut self, idx: usize, source: &str, rec: &RecoveredFile) {
        if self.states.contains_key(&idx) {
            return;
        }
        if rec.size == 0 || rec.size > MAX_PREVIEW_BYTES {
            self.states.insert(idx, State::Failed);
            return;
        }
        self.states.insert(idx, State::Loading);
        let (lock, cvar) = &*self.queue;
        lock.lock().unwrap().push_back(Job {
            generation: self.generation.load(Ordering::Relaxed),
            idx,
            source: source.to_string(),
            rec: rec.clone(),
        });
        cvar.notify_one();
    }
}

fn worker(
    queue: &JobQueue,
    generation: &AtomicU64,
    tx: &Sender<(u64, usize, Option<egui::ColorImage>)>,
    ctx: &egui::Context,
) {
    let (lock, cvar) = &**queue;
    loop {
        let job = {
            let mut q = lock.lock().unwrap();
            loop {
                if let Some(job) = q.pop_front() {
                    break job;
                }
                q = cvar.wait(q).unwrap();
            }
        };
        if job.generation != generation.load(Ordering::Relaxed) {
            continue;
        }
        let image = decode(&job.source, &job.rec);
        if tx.send((job.generation, job.idx, image)).is_err() {
            return;
        }
        ctx.request_repaint();
    }
}

fn decode(source: &str, rec: &RecoveredFile) -> Option<egui::ColorImage> {
    let bytes = ntfs::read_file_bytes(source, rec, MAX_PREVIEW_BYTES).ok()?;
    let image = image::load_from_memory(&bytes).ok()?;
    let thumb = image.thumbnail(THUMB_PX, THUMB_PX).to_rgba8();
    let size = [thumb.width() as usize, thumb.height() as usize];
    Some(egui::ColorImage::from_rgba_unmultiplied(size, thumb.as_raw()))
}

//! Full-duplex device I/O: capture, playback and instant barge-in.
//!
//! Both `cpal` streams are opened at the rate the device is *already* running at, mono
//! f32 callbacks, with a small fixed block. Everything expensive happens on worker
//! threads: the capture worker resamples device-rate frames down to `target_in_rate`
//! and hands them to `on_input` in `block_frames` blocks, and (only when the output
//! device rate differs from `target_out_rate`) a playback worker resamples the engine's
//! 48 kHz frames up to the device rate.
//!
//! When `target_out_rate` equals the device rate — the normal case on macOS, where both
//! are 48 kHz — the playback worker is skipped entirely and [`AudioIo::push_output`]
//! writes straight into the ring the audio callback consumes. That keeps the common path
//! free of the extra chunk of latency, and makes barge-in exact: the flush happens in the
//! callback on the very next block.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{anyhow, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, SampleFormat, StreamConfig, SupportedBufferSize};

use crate::resample::StreamResampler;
use crate::AudioConfig;

/// How long the input worker sleeps when the ring is empty.
const POLL_INTERVAL: Duration = Duration::from_millis(1);

/// Capacity of the engine-side output queue, in seconds of the engine's audio.
///
/// Deliberately generous so `push_output` can queue a whole utterance without ever
/// dropping audio. It is not a latency knob: the queue only holds what the engine has
/// queued *ahead* of the device, and barge-in discards it in O(1) (see [`drain`]), so a
/// deeper queue costs memory, not stop-to-silence time.
const OUTPUT_QUEUE_SECS: usize = 30;

/// Capacity of the device-side queue when the device rate differs from `target_out_rate`.
/// This one *is* a latency knob (the worker keeps it topped up), so it stays small.
const DEVICE_QUEUE_MS: usize = 50;

struct WorkerExit {
    stop: Arc<AtomicBool>,
    failed: Arc<AtomicBool>,
}
impl Drop for WorkerExit {
    fn drop(&mut self) {
        if !self.stop.load(Ordering::Acquire) {
            self.failed.store(true, Ordering::Release);
        }
    }
}

/// Barge-in control word.
///
/// The callback watches a generation counter, not a bool, so a rapid stop/start pair
/// cannot collapse into a lost update, and it flushes the ring while muted so no frame
/// that raced in from the playback worker can survive a barge-in.
#[derive(Default)]
struct PlaybackControl {
    flush_gen: AtomicU64,
    muted: AtomicBool,
    callback_ack: AtomicU64,
    worker_ack: AtomicU64,
    /// Input-rate frames buffered in DSP or awaiting device publication.
    buffered: AtomicUsize,
    /// Ordered end markers not yet fully published by the worker.
    finishing: AtomicUsize,
    /// Defer cancellation's final drain while an old-generation producer publishes.
    /// SeqCst pairs this flag with generation checks, so either the producer sees
    /// cancellation before writing or the final drain waits until its writes end.
    producer_active: AtomicBool,
}

struct OutputWrite<'a>(&'a AtomicBool);
impl Drop for OutputWrite<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

#[derive(Clone, Copy, Debug)]
enum OutputFrame {
    Sample(f32),
    End,
}

/// Own a newly spawned worker even before device startup succeeds.
struct StartingWorker {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}
impl Drop for StartingWorker {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            self.stop.store(true, Ordering::Release);
            let _ = handle.join();
        }
    }
}

/// Silence immediately, but acknowledge only after the worker can no longer publish
/// old-generation samples. Called on the actual callback path and in queue tests.
fn acknowledge_cancel(
    cons: &mut rtrb::Consumer<OutputFrame>,
    control: &PlaybackControl,
    queued: &AtomicUsize,
    last_gen: &mut u64,
    needs_resample: bool,
) {
    let gen = control.flush_gen.load(Ordering::SeqCst);
    if gen != *last_gen
        && !control.producer_active.load(Ordering::SeqCst)
        && (!needs_resample || control.worker_ack.load(Ordering::Acquire) == gen)
    {
        drain(cons);
        queued.store(0, Ordering::Relaxed);
        *last_gen = gen;
        control.callback_ack.store(gen, Ordering::Release);
    }
}

/// Output playback, independently owned from microphone [`Capture`].
///
/// Dropping this closes only the output stream and joins the playback worker,
/// without calling `pause()`. An independently retained [`Capture`] continues
/// recording; drop that handle separately to release the microphone.
pub struct AudioIo {
    output_rate: u32,
    target_out_rate: u32,
    output_stream: Option<cpal::Stream>,
    output_worker: Option<JoinHandle<()>>,
    /// Engine → (worker or callback) queue, at `target_out_rate`.
    engine_prod: Mutex<rtrb::Producer<OutputFrame>>,
    control: Arc<PlaybackControl>,
    /// Frames at `target_out_rate` not yet taken from `engine_prod`.
    queued: Arc<AtomicUsize>,
    /// Frames at the device rate waiting in the device ring (resampling path only).
    dev_queued: Arc<AtomicUsize>,
    underruns: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    failed: Arc<AtomicBool>,
}

/// Independently owned microphone capture. Dropping this releases the input
/// stream/device without touching output playback or any inference model.
pub struct Capture {
    stream: Option<cpal::Stream>,
    worker: Option<JoinHandle<()>>,
    stop: Arc<AtomicBool>,
    failed: Arc<AtomicBool>,
    failure: Arc<Mutex<Option<String>>>,
}

/// Worker-side capture delivery. `Gap` invalidates incomplete capture processing;
/// it is never an empty PCM block. Fresh `Frame`s follow after DSP/block reset.
#[derive(Debug)]
pub enum CaptureEvent<'a> {
    Frame(&'a [f32]),
    Gap,
}

#[derive(Clone, Copy, Debug)]
struct InputFrame {
    sample: f32,
    generation: u64,
}

/// Audio callback path: tag accepted frames and publish an out-of-band gap when
/// the bounded ring fills. No marker needs space in an already-full ring.
fn capture_samples(
    producer: &mut rtrb::Producer<InputFrame>,
    data: &[f32],
    channels: usize,
    generation: &mut u64,
    gap_generation: &AtomicU64,
) -> usize {
    let mut accepted = 0;
    for frame in data.chunks_exact(channels) {
        let mono = frame.iter().sum::<f32>() / channels as f32;
        if producer
            .push(InputFrame {
                sample: mono,
                generation: *generation,
            })
            .is_err()
        {
            *generation = gap_generation
                .fetch_add(1, Ordering::Release)
                .wrapping_add(1);
            break;
        }
        accepted += 1;
    }
    accepted
}

impl Capture {
    pub fn start(
        cfg: &AudioConfig,
        on_input: Box<dyn FnMut(CaptureEvent<'_>) + Send>,
    ) -> Result<Self> {
        anyhow::ensure!(
            cfg.block_frames > 0,
            "Capture block_frames must be non-zero"
        );
        let host = cpal::default_host();
        let device = pick_device(&host, cfg.input_device.as_deref(), true)?;
        let chosen = choose_config(&device, true, cfg.block_frames)?;
        let rate = chosen.config.sample_rate;
        let channels = chosen.config.channels as usize;
        anyhow::ensure!(channels > 0, "Input device returned zero channels");
        let stop = Arc::new(AtomicBool::new(false));
        let failed = Arc::new(AtomicBool::new(false));
        let failure = Arc::new(Mutex::new(None::<String>));
        let capture_failed = failed.clone();
        let capture_failure = failure.clone();
        let (mut producer, consumer) =
            rtrb::RingBuffer::<InputFrame>::new((rate as usize / 2).max(4096));
        let gap_generation = Arc::new(AtomicU64::new(0));
        let callback_gap = gap_generation.clone();
        let mut generation = 0;
        let stream = device
            .build_input_stream::<f32, _, _>(
                chosen.config,
                move |data: &[f32], _| {
                    capture_samples(
                        &mut producer,
                        data,
                        channels,
                        &mut generation,
                        &callback_gap,
                    );
                },
                move |stream_error| {
                    // Keep the device's own text; a bool alone is undiagnosable.
                    if let Ok(mut slot) = capture_failure.lock() {
                        slot.get_or_insert_with(|| stream_error.to_string());
                    }
                    capture_failed.store(true, Ordering::Release);
                },
                None,
            )
            .map_err(|e| anyhow!("Cannot open capture: {e}"))?;
        stream
            .play()
            .map_err(|e| anyhow!("Cannot start capture: {e}"))?;
        let worker = spawn_input_worker(
            consumer,
            on_input,
            rate,
            cfg.target_in_rate,
            cfg.block_frames,
            gap_generation,
            stop.clone(),
            failed.clone(),
        )?;
        Ok(Self {
            stream: Some(stream),
            worker: Some(worker),
            stop,
            failed,
            failure,
        })
    }
    pub fn failed(&self) -> bool {
        self.failed.load(Ordering::Acquire)
    }
    /// Device-reported reason the input stream stopped, when the device gave one.
    pub fn failure(&self) -> Option<String> {
        self.failure.lock().ok().and_then(|slot| slot.clone())
    }
}
impl Drop for Capture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.stream.take(); // Close, not pause: the OS capture handle is released.
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl AudioIo {
    /// Opens only output at its current device rate. Capture is opened/closed
    /// separately so mute releases the OS microphone stream rather than pausing it.
    pub fn start(cfg: AudioConfig) -> Result<Self> {
        let block_frames = cfg.block_frames.max(1);
        let host = cpal::default_host();

        let out_dev = pick_device(&host, cfg.output_device.as_deref(), false)?;
        // Read the device's *current* config and open exactly that rate. Never request a
        // different rate: on macOS that either fails or silently re-clocks the device.
        let out_chosen = choose_config(&out_dev, false, block_frames)?;

        let out_rate = out_chosen.config.sample_rate;
        let out_channels = out_chosen.config.channels;

        let stop = Arc::new(AtomicBool::new(false));
        let failed = Arc::new(AtomicBool::new(false));

        // ---- playback ----
        let needs_resample = out_rate != cfg.target_out_rate;
        let control = Arc::new(PlaybackControl::default());
        let queued = Arc::new(AtomicUsize::new(0));
        let dev_queued = Arc::new(AtomicUsize::new(0));
        let underruns = Arc::new(AtomicU64::new(0));

        let (engine_prod, engine_cons) = rtrb::RingBuffer::<OutputFrame>::new(
            (cfg.target_out_rate as usize * OUTPUT_QUEUE_SECS).max(48_000),
        );

        let (mut dev_cons, active_queued, output_worker) = if needs_resample {
            let (dev_prod, dev_cons) = rtrb::RingBuffer::<OutputFrame>::new(
                (out_rate as usize * DEVICE_QUEUE_MS / 1000).max(1024),
            );
            let worker = spawn_output_worker(
                engine_cons,
                dev_prod,
                cfg.target_out_rate,
                out_rate,
                control.clone(),
                queued.clone(),
                dev_queued.clone(),
                stop.clone(),
                failed.clone(),
            )?;
            (dev_cons, dev_queued.clone(), Some(worker))
        } else {
            (engine_cons, queued.clone(), None)
        };
        let mut starting_worker = StartingWorker {
            stop: stop.clone(),
            handle: output_worker,
        };

        let cb_control = control.clone();
        let cb_queued = active_queued.clone();
        let cb_underruns = underruns.clone();
        let callback_failed = failed.clone();
        let mut last_gen = 0u64;
        let output_stream = out_dev
            .build_output_stream::<f32, _, _>(
                out_chosen.config,
                move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    acknowledge_cancel(
                        &mut dev_cons,
                        &cb_control,
                        &cb_queued,
                        &mut last_gen,
                        needs_resample,
                    );
                    let channels = out_channels as usize;
                    let frames = data.len() / channels;

                    if cb_control.muted.load(Ordering::Acquire)
                        || cb_control.flush_gen.load(Ordering::SeqCst) != last_gen
                    {
                        // Barge-in in progress: silence now, and do not consume the queue.
                        data.fill(0.0);
                        return;
                    }

                    let before = cb_queued.load(Ordering::Relaxed);
                    let mut got = 0usize;
                    for frame in 0..frames {
                        // End markers on the equal-rate path carry no audio frame.
                        let sample = loop {
                            match dev_cons.pop() {
                                Ok(OutputFrame::End) => continue,
                                next => break next,
                            }
                        };
                        match sample {
                            Ok(OutputFrame::Sample(sample)) => {
                                let base = frame * channels;
                                for c in 0..channels {
                                    data[base + c] = sample;
                                }
                                got += 1;
                            }
                            Err(_) => {
                                // Starved mid-block: silence the rest. `data` starts out
                                // zeroed, but be explicit about the tail we did not write.
                                data[frame * channels..].fill(0.0);
                                break;
                            }
                            Ok(OutputFrame::End) => unreachable!(),
                        }
                    }
                    // Only count a real underrun: the engine believed audio was in flight.
                    // Running dry while nothing has been queued at all is just silence.
                    if got < frames && before > 0 {
                        cb_underruns.fetch_add(1, Ordering::Relaxed);
                    }
                    if got > 0 {
                        let _ = cb_queued.try_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                            Some(v.saturating_sub(got))
                        });
                    }
                },
                move |_| {
                    callback_failed.store(true, Ordering::Release);
                },
                None,
            )
            .map_err(|e| anyhow!("cannot open output stream ({}): {e}", out_chosen.note))?;

        output_stream
            .play()
            .map_err(|e| anyhow!("cannot start output stream: {e}"))?;

        Ok(Self {
            output_rate: out_rate,
            target_out_rate: cfg.target_out_rate,
            output_stream: Some(output_stream),
            output_worker: starting_worker.handle.take(),
            engine_prod: Mutex::new(engine_prod),
            control,
            queued,
            dev_queued,
            underruns,
            stop,
            failed,
        })
    }

    /// Includes device callback errors and unexpected DSP worker exits.
    pub fn failed(&self) -> bool {
        self.failed.load(Ordering::Acquire)
    }

    /// Enqueue mono f32 samples at `target_out_rate` for playback.
    ///
    /// Never resamples: the device takes them at its own rate, and if it differs from
    /// `target_out_rate` a worker converts them. May wait up to 250 ms for cancellation
    /// acknowledgement off the realtime callback. The queue holds up to 30
    /// seconds, so a whole utterance can be pushed in one call; only audio beyond that
    /// (i.e. tens of seconds ahead of realtime) is dropped.
    pub fn push_output(&self, samples: &[f32]) -> Result<()> {
        // A rapid cancel/new-speech pair must not let the pending consumer flush
        // discard the new generation's audio. Wait off the realtime callback.
        let gen = self.control.flush_gen.load(Ordering::SeqCst);
        let deadline = std::time::Instant::now() + Duration::from_millis(250);
        while self.control.callback_ack.load(Ordering::Acquire) != gen
            || (self.output_worker.is_some()
                && self.control.worker_ack.load(Ordering::Acquire) != gen)
        {
            if std::time::Instant::now() >= deadline {
                return Err(anyhow!(
                    "Output device did not acknowledge playback cancellation"
                ));
            }
            std::thread::sleep(POLL_INTERVAL);
        }
        let Ok(mut prod) = self.engine_prod.lock() else {
            return Err(anyhow!("Playback queue lock poisoned"));
        };
        self.control.producer_active.store(true, Ordering::SeqCst);
        let _write = OutputWrite(&self.control.producer_active);
        if self.control.flush_gen.load(Ordering::SeqCst) != gen {
            return Err(anyhow!("Playback cancelled while queuing output"));
        }
        self.control.muted.store(false, Ordering::Release);
        self.queued.fetch_add(samples.len(), Ordering::Relaxed);
        let mut accepted = 0usize;
        for &sample in samples {
            match prod.push(OutputFrame::Sample(sample)) {
                Ok(()) => accepted += 1,
                Err(_) => break,
            }
        }
        let _ = self
            .queued
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                Some(v.saturating_sub(samples.len() - accepted))
            });
        if accepted != samples.len() {
            return Err(anyhow!("Bounded playback queue is full"));
        }
        anyhow::ensure!(
            self.control.flush_gen.load(Ordering::SeqCst) == gen,
            "Playback cancelled while queuing output"
        );
        Ok(())
    }

    /// Mark the end of an utterance, after its last samples. Streaming gaps do not
    /// flush DSP: only this ordered marker recovers the tail and resets the stream.
    pub fn finish_output(&self) -> Result<()> {
        let mut prod = self
            .engine_prod
            .lock()
            .map_err(|_| anyhow!("Playback queue lock poisoned"))?;
        self.control.producer_active.store(true, Ordering::SeqCst);
        let _write = OutputWrite(&self.control.producer_active);
        let gen = self.control.flush_gen.load(Ordering::SeqCst);
        anyhow::ensure!(
            self.control.callback_ack.load(Ordering::Acquire) == gen
                && (self.output_worker.is_none()
                    || self.control.worker_ack.load(Ordering::Acquire) == gen),
            "Playback cancellation has not been acknowledged"
        );
        if self.output_worker.is_some() {
            self.control.finishing.fetch_add(1, Ordering::Relaxed);
            if prod.push(OutputFrame::End).is_err() {
                self.control.finishing.fetch_sub(1, Ordering::Relaxed);
                return Err(anyhow!("Bounded playback queue is full"));
            }
        }
        anyhow::ensure!(
            self.control.flush_gen.load(Ordering::SeqCst) == gen,
            "Playback cancelled while finishing output"
        );
        Ok(())
    }

    /// Barge-in: instantly silence output and discard queued audio.
    ///
    /// This is the replacement for `Stream::pause()`, which blocks the caller for
    /// 1.9–15 ms. It only updates atomics; the output callback writes silence on its
    /// next block, then final-drains once old-generation producers have acknowledged.
    /// Actual stop-to-silence latency depends on the device.
    /// The discard itself is a pointer bump, so queue depth does not affect it.
    pub fn flush_output(&self) {
        self.flush_output_published(|| {});
    }

    fn flush_output_published(&self, published: impl FnOnce()) {
        self.control.muted.store(true, Ordering::Release);
        self.queued.store(0, Ordering::Relaxed);
        self.dev_queued.store(0, Ordering::Relaxed);
        // No counter reset may follow publication: the acknowledgements can already
        // release a fresh producer while this caller is still returning.
        self.control.flush_gen.fetch_add(1, Ordering::SeqCst);
        published();
    }

    /// Number of output frames still queued, for "is it still speaking".
    ///
    /// Reported at `target_out_rate`. In the resampling path the device-side queue is
    /// converted back from the device rate, so this is accurate to within one chunk.
    pub fn output_queued_frames(&self) -> usize {
        self.output_queued_snapshot(|| {})
    }

    fn output_queued_snapshot(&self, midway: impl FnOnce()) -> usize {
        // Read upstream -> downstream. Each worker transfer increments downstream
        // before a Release decrement upstream; an Acquire observing that decrement
        // carries the downstream increment into the subsequent read. This prevents
        // a torn all-zero result without callback locks, retries, or extra versions.
        let finishing = self.control.finishing.load(Ordering::Acquire);
        let engine = self.queued.load(Ordering::Acquire);
        if self.output_rate == self.target_out_rate {
            engine
        } else {
            midway();
            let buffered = self.control.buffered.load(Ordering::Acquire);
            let dev = self.dev_queued.load(Ordering::Acquire);
            engine
                // Round up so even the last device sample remains pending.
                + (dev * self.target_out_rate as usize).div_ceil(self.output_rate.max(1) as usize)
                + buffered + finishing
        }
    }

    /// Playback underruns observed so far (diagnostics).
    pub fn underruns(&self) -> u64 {
        self.underruns.load(Ordering::Relaxed)
    }
}

impl Drop for AudioIo {
    fn drop(&mut self) {
        // Wake workers, stop callbacks by dropping the streams, then join. Never pause().
        self.stop.store(true, Ordering::Release);
        self.output_stream.take();
        if let Some(handle) = self.output_worker.take() {
            let _ = handle.join();
        }
    }
}

/// Resamples `in_rate` → `out_rate` off the audio thread and emits `block_frames` blocks.
fn spawn_input_worker(
    mut cons: rtrb::Consumer<InputFrame>,
    mut on_input: Box<dyn FnMut(CaptureEvent<'_>) + Send>,
    in_rate: u32,
    out_rate: u32,
    block_frames: usize,
    gap_generation: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    failed: Arc<AtomicBool>,
) -> Result<JoinHandle<()>> {
    anyhow::ensure!(block_frames > 0, "Capture block_frames must be non-zero");
    let mut processor = CaptureProcessor {
        resampler: StreamResampler::new(in_rate, out_rate)?,
        acc: Vec::with_capacity(block_frames * 4),
        block_frames,
        generation: 0,
    };
    Ok(std::thread::spawn(move || {
        let _health = WorkerExit {
            stop: stop.clone(),
            failed,
        };
        let mut pull = vec![0.0f32; 2048];
        while !stop.load(Ordering::Acquire) {
            processor.check_gap(&gap_generation, &mut on_input);
            let mut n = 0;
            while n < pull.len() && !stop.load(Ordering::Acquire) {
                match cons.pop() {
                    Ok(frame) => {
                        if frame.generation != processor.generation {
                            if processor.check_gap(&gap_generation, &mut on_input) {
                                n = 0;
                            }
                            if frame.generation != processor.generation {
                                continue; // Accepted before an observed overrun: never splice it back in.
                            }
                        }
                        pull[n] = frame.sample;
                        n += 1;
                    }
                    Err(_) => break,
                }
            }
            if processor.check_gap(&gap_generation, &mut on_input) {
                continue;
            }
            if n == 0 {
                std::thread::sleep(POLL_INTERVAL);
                continue;
            }
            processor
                .acc
                .extend_from_slice(&processor.resampler.process(&pull[..n]));
            processor.emit_blocks(&gap_generation, &mut on_input);
        }
        // Hand over the resampler's tail so the last few milliseconds are not lost.
        processor.check_gap(&gap_generation, &mut on_input);
        processor
            .acc
            .extend_from_slice(&processor.resampler.flush());
        processor.emit_blocks(&gap_generation, &mut on_input);
        if !processor.check_gap(&gap_generation, &mut on_input) && !processor.acc.is_empty() {
            on_input(CaptureEvent::Frame(&processor.acc));
        }
    }))
}

struct CaptureProcessor {
    resampler: StreamResampler,
    acc: Vec<f32>,
    block_frames: usize,
    generation: u64,
}
impl CaptureProcessor {
    fn check_gap(
        &mut self,
        gap_generation: &AtomicU64,
        on_input: &mut dyn FnMut(CaptureEvent<'_>),
    ) -> bool {
        let generation = gap_generation.load(Ordering::Acquire);
        if generation == self.generation {
            return false;
        }
        self.generation = generation;
        self.resampler.reset();
        self.acc.clear();
        on_input(CaptureEvent::Gap);
        true
    }

    fn emit_blocks(
        &mut self,
        gap_generation: &AtomicU64,
        on_input: &mut dyn FnMut(CaptureEvent<'_>),
    ) {
        while self.acc.len() >= self.block_frames {
            if self.check_gap(gap_generation, on_input) {
                return;
            }
            on_input(CaptureEvent::Frame(&self.acc[..self.block_frames]));
            self.acc.drain(..self.block_frames);
        }
    }
}

/// Resamples the engine's `target_out_rate` frames to the device rate, off the audio thread.
///
/// Only used when the device rate differs from `target_out_rate`; the common 48 kHz case
/// bypasses this worker completely.
#[allow(clippy::too_many_arguments)]
fn spawn_output_worker(
    mut cons: rtrb::Consumer<OutputFrame>,
    mut prod: rtrb::Producer<OutputFrame>,
    in_rate: u32,
    out_rate: u32,
    control: Arc<PlaybackControl>,
    queued: Arc<AtomicUsize>,
    dev_queued: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    failed: Arc<AtomicBool>,
) -> Result<JoinHandle<()>> {
    let mut resampler = StreamResampler::new(in_rate, out_rate)?;
    Ok(std::thread::spawn(move || {
        let _health = WorkerExit {
            stop: stop.clone(),
            failed,
        };
        let mut pull = vec![0.0f32; 2048];
        let mut leftover: Vec<f32> = Vec::new();
        let mut last_gen = 0;
        let mut ending = false;
        let mut input_total = 0usize;
        let mut published = 0usize;
        while !stop.load(Ordering::Acquire) {
            let gen = control.flush_gen.load(Ordering::SeqCst);
            if gen != last_gen {
                if control.producer_active.load(Ordering::SeqCst) {
                    std::thread::sleep(POLL_INTERVAL);
                    continue;
                }
                // Barge-in: throw away the engine's queued audio and the resampler state so
                // the stale tail cannot leak into the next utterance.
                drain(&mut cons);
                queued.store(0, Ordering::Release);
                resampler.reset();
                leftover.clear();
                control.buffered.store(0, Ordering::Release);
                control.finishing.store(0, Ordering::Release);
                ending = false;
                input_total = 0;
                published = 0;
                last_gen = gen;
                control.worker_ack.store(gen, Ordering::Release);
            }

            if !leftover.is_empty() {
                // Never hand the device audio the engine has already discarded.
                if control.flush_gen.load(Ordering::Acquire) != last_gen {
                    leftover.clear();
                    continue;
                }
                dev_queued.fetch_add(leftover.len(), Ordering::Relaxed);
                let pushed = push_some(&mut prod, &leftover);
                published += pushed;
                control.buffered.store(
                    input_total.saturating_sub(published * in_rate as usize / out_rate as usize),
                    Ordering::Release,
                );
                let _ = dev_queued.try_update(Ordering::Release, Ordering::Relaxed, |v| {
                    Some(v.saturating_sub(leftover.len() - pushed))
                });
                leftover.drain(..pushed);
                if !leftover.is_empty() {
                    std::thread::sleep(POLL_INTERVAL);
                    continue;
                }
            }

            if ending {
                control.buffered.store(0, Ordering::Release);
                let _ = control
                    .finishing
                    .try_update(Ordering::Release, Ordering::Relaxed, |n| {
                        Some(n.saturating_sub(1))
                    });
                ending = false;
                input_total = 0;
                published = 0;
            }

            let mut n = 0;
            while n < pull.len() {
                match cons.pop() {
                    Ok(OutputFrame::Sample(sample)) => {
                        pull[n] = sample;
                        n += 1;
                    }
                    Ok(OutputFrame::End) => {
                        ending = true;
                        break;
                    }
                    Err(_) => break,
                }
            }
            if n == 0 && !ending {
                std::thread::sleep(POLL_INTERVAL);
                continue;
            }
            input_total += n;
            control.buffered.store(
                input_total.saturating_sub(published * in_rate as usize / out_rate as usize),
                Ordering::Release,
            );
            let _ = queued.try_update(Ordering::Release, Ordering::Relaxed, |v| {
                Some(v.saturating_sub(n))
            });
            let mut resampled = resampler.process(&pull[..n]);
            if ending {
                resampled.extend(resampler.flush());
                resampler.reset();
            }
            if resampled.is_empty() {
                continue;
            }
            // Re-check the generation: if barge-in landed while we were resampling, this
            // audio has already been discarded and must not be handed to the device.
            if control.flush_gen.load(Ordering::Acquire) != last_gen {
                continue;
            }
            leftover.extend_from_slice(&resampled);
        }
    }))
}

/// Pushes as much of `data` as fits, returning how many frames were accepted.
fn push_some(prod: &mut rtrb::Producer<OutputFrame>, data: &[f32]) -> usize {
    let mut pushed = 0;
    for &sample in data {
        match prod.push(OutputFrame::Sample(sample)) {
            Ok(()) => pushed += 1,
            Err(_) => break,
        }
    }
    pushed
}

/// Discards everything currently queued.
///
/// Uses `read_chunk`/`commit_all` rather than a `pop` loop so the cost is a pointer bump
/// (plus a no-op `drop_in_place` for `f32`), not one atomic-ish pop per frame. That is what
/// lets the callback flush an arbitrarily deep queue in one block and keeps `push_output`
/// free to accept a whole utterance.
fn drain<T>(cons: &mut rtrb::Consumer<T>) {
    loop {
        let available = cons.slots();
        if available == 0 {
            break;
        }
        match cons.read_chunk(available) {
            Ok(chunk) => chunk.commit_all(),
            Err(_) => break,
        }
    }
}

/// Finds a device by name, or falls back to the system default.
fn pick_device(host: &cpal::Host, want: Option<&str>, input: bool) -> Result<cpal::Device> {
    let direction = if input { "input" } else { "output" };
    if let Some(name) = want {
        let devices: Vec<cpal::Device> = if input {
            host.input_devices()
                .map_err(|e| anyhow!("cannot enumerate {direction} devices: {e}"))?
                .collect()
        } else {
            host.output_devices()
                .map_err(|e| anyhow!("cannot enumerate {direction} devices: {e}"))?
                .collect()
        };
        for device in devices {
            if let Ok(desc) = device.description() {
                if desc.name() == name {
                    return Ok(device);
                }
            }
        }
        return Err(anyhow!("no {direction} device matching {name:?}"));
    }
    let default = if input {
        host.default_input_device()
    } else {
        host.default_output_device()
    };
    default.ok_or_else(|| anyhow!("no default {direction} device"))
}

/// The stream layout we will open: the device's current rate, mono-compatible f32, small
/// fixed block if the device allows one.
struct ChosenConfig {
    config: StreamConfig,
    note: String,
}

/// Picks the config to open, and never invents a sample rate.
///
/// The rate comes from the device's own `default_*_config()`. If the device's default
/// sample format is not f32 we look for an f32 range that covers that same rate; we do
/// **not** look for a different rate, because opening one would move the shared clock.
fn choose_config(device: &cpal::Device, input: bool, block_frames: usize) -> Result<ChosenConfig> {
    let direction = if input { "input" } else { "output" };
    let default = if input {
        device.default_input_config()
    } else {
        device.default_output_config()
    }
    .map_err(|e| anyhow!("cannot read the default {direction} config: {e}"))?;

    let rate = default.sample_rate();
    let (channels, buffer_range) = if default.sample_format() == SampleFormat::F32 {
        (default.channels(), *default.buffer_size())
    } else {
        let ranges: Vec<cpal::SupportedStreamConfigRange> = if input {
            device
                .supported_input_configs()
                .map_err(|e| anyhow!("cannot query input configs: {e}"))?
                .collect()
        } else {
            device
                .supported_output_configs()
                .map_err(|e| anyhow!("cannot query output configs: {e}"))?
                .collect()
        };
        ranges
            .iter()
            .filter(|r| r.sample_format() == SampleFormat::F32)
            .find(|r| r.min_sample_rate() <= rate && rate <= r.max_sample_rate())
            .map(|r| (r.channels(), *r.buffer_size()))
            .ok_or_else(|| {
                anyhow!(
                    "no f32 {direction} config at the device's current {rate} Hz \
                     (ve-audio never changes the device sample rate)"
                )
            })?
    };

    let buffer_size = match buffer_range {
        SupportedBufferSize::Range { min, max }
            if block_frames as u32 >= min && block_frames as u32 <= max =>
        {
            BufferSize::Fixed(block_frames as u32)
        }
        _ => BufferSize::Default,
    };

    let note = format!(
        "{direction} device at its native {rate} Hz, {channels} ch, buffer {buffer_size:?}"
    );
    Ok(ChosenConfig {
        config: StreamConfig {
            channels,
            sample_rate: rate,
            buffer_size,
        },
        note,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wait(mut ready: impl FnMut() -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !ready() {
            assert!(std::time::Instant::now() < deadline, "worker timed out");
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    #[test]
    fn acknowledged_cancel_cannot_erase_new_equal_rate_pcm_count() {
        let (io, mut dev) = equal_playback();
        let mut last_gen = 0;
        io.flush_output_published(|| {
            acknowledge_cancel(&mut dev, &io.control, &io.queued, &mut last_gen, false);
            io.push_output(&[0.25, -0.5, 0.75]).unwrap();
        });
        assert_eq!(
            io.output_queued_frames(),
            3,
            "acknowledged new PCM was erased from accounting"
        );
        for expected in [0.25, -0.5, 0.75] {
            assert!(matches!(dev.pop(), Ok(OutputFrame::Sample(s)) if s == expected));
        }
    }

    fn equal_playback() -> (AudioIo, rtrb::Consumer<OutputFrame>) {
        let (prod, cons) = rtrb::RingBuffer::new(128);
        (
            AudioIo {
                output_rate: 24_000,
                target_out_rate: 24_000,
                output_stream: None,
                output_worker: None,
                engine_prod: Mutex::new(prod),
                control: Arc::new(PlaybackControl::default()),
                queued: Arc::new(AtomicUsize::new(0)),
                dev_queued: Arc::new(AtomicUsize::new(0)),
                underruns: Arc::new(AtomicU64::new(0)),
                stop: Arc::new(AtomicBool::new(false)),
                failed: Arc::new(AtomicBool::new(false)),
            },
            cons,
        )
    }

    #[test]
    fn snapshot_cannot_claim_drained_across_final_tail_publication() {
        let (io, mut dev) = playback(Arc::new(PlaybackControl::default()));
        io.push_output(&[0.5; 37]).unwrap();
        wait(|| {
            io.queued.load(Ordering::Relaxed) == 0
                && io.control.buffered.load(Ordering::Relaxed) == 37
        });
        assert_eq!(dev.slots(), 0);
        let pending = io.output_queued_snapshot(|| {
            io.finish_output().unwrap();
            wait(|| dev.slots() == 74 && io.control.finishing.load(Ordering::Relaxed) == 0);
        });
        assert!(
            pending > 0,
            "snapshot reported drained while 74 tail PCM frames await playback"
        );
        let actual = collect(&io, &mut dev);
        let mut rs = StreamResampler::new(24_000, 48_000).unwrap();
        let mut expected = rs.process(&[0.5; 37]);
        expected.extend(rs.flush());
        assert_eq!(actual, expected);
    }

    #[test]
    fn one_unconsumed_device_pcm_frame_remains_pending_after_rate_conversion() {
        let (io, mut dev) = playback(Arc::new(PlaybackControl::default()));
        io.push_output(&[0.5; 37]).unwrap();
        io.finish_output().unwrap();
        wait(|| dev.slots() == 74 && io.control.finishing.load(Ordering::Acquire) == 0);
        for _ in 0..73 {
            dev.pop().unwrap();
        }
        io.dev_queued.fetch_sub(73, Ordering::Relaxed);
        assert_eq!(dev.slots(), 1);
        assert!(
            io.output_queued_frames() > 0,
            "the last real playback sample rounded down to drained"
        );
        assert!(matches!(dev.pop(), Ok(OutputFrame::Sample(s)) if s.abs() > 0.1));
        io.dev_queued.fetch_sub(1, Ordering::Relaxed);
        assert_eq!(io.output_queued_frames(), 0);
    }

    #[test]
    fn capture_overrun_does_not_splice_old_pcm_into_fresh_signal() {
        let (mut prod, cons) = rtrb::RingBuffer::new(1024);
        let stop = Arc::new(AtomicBool::new(false));
        let failed = Arc::new(AtomicBool::new(false));
        let phase = Arc::new(AtomicBool::new(false));
        let gap_generation = Arc::new(AtomicU64::new(0));
        let gaps = Arc::new(AtomicUsize::new(0));
        let actual = Arc::new(Mutex::new(Vec::new()));
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (resume_tx, resume_rx) = std::sync::mpsc::channel();
        let capture_phase = phase.clone();
        let capture_actual = actual.clone();
        let capture_gaps = gaps.clone();
        let mut generation = 0;
        assert_eq!(
            capture_samples(&mut prod, &[0.9; 600], 1, &mut generation, &gap_generation),
            600
        );
        let mut first = true;
        let worker = spawn_input_worker(
            cons,
            Box::new(move |event| match event {
                CaptureEvent::Gap => {
                    capture_gaps.fetch_add(1, Ordering::Release);
                }
                CaptureEvent::Frame(samples) => {
                    if first {
                        first = false;
                        ready_tx.send(()).unwrap();
                        resume_rx.recv_timeout(Duration::from_secs(2)).unwrap();
                    } else if capture_phase.load(Ordering::Acquire) {
                        assert_eq!(
                            capture_gaps.load(Ordering::Acquire),
                            1,
                            "post-overrun PCM arrived before the typed gap"
                        );
                        capture_actual.lock().unwrap().extend_from_slice(samples);
                    }
                }
            }),
            48_000,
            16_000,
            64,
            gap_generation.clone(),
            stop.clone(),
            failed.clone(),
        )
        .unwrap();
        let capture = Capture {
            stream: None,
            worker: Some(worker),
            stop,
            failed,
            failure: Arc::new(Mutex::new(None)),
        };
        ready_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(
            capture_samples(&mut prod, &[0.9; 1025], 1, &mut generation, &gap_generation),
            1024,
            "fixture must really overrun the raw microphone ring"
        );
        phase.store(true, Ordering::Release);
        resume_tx.send(()).unwrap();
        wait(|| prod.slots() == 1024);
        assert_eq!(
            gaps.load(Ordering::Acquire),
            1,
            "overrun must be delivered even before fresh input resumes"
        );
        let mut fresh = vec![0.0; 977];
        fresh[965] = -1.0;
        assert_eq!(
            capture_samples(&mut prod, &fresh, 1, &mut generation, &gap_generation),
            fresh.len()
        );
        wait(|| prod.slots() == 1024);
        assert!(!capture.failed());
        drop(capture);
        let mut rs = StreamResampler::new(48_000, 16_000).unwrap();
        let mut expected = rs.process(&fresh);
        expected.extend(rs.flush());
        assert_eq!(
            *actual.lock().unwrap(),
            expected,
            "overrun spliced positive stale PCM/DSP state into the new negative impulse"
        );
    }

    fn playback(control: Arc<PlaybackControl>) -> (AudioIo, rtrb::Consumer<OutputFrame>) {
        let (engine_prod, engine_cons) = rtrb::RingBuffer::new(10_000);
        let (dev_prod, dev_cons) = rtrb::RingBuffer::new(128);
        let queued = Arc::new(AtomicUsize::new(0));
        let dev_queued = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let failed = Arc::new(AtomicBool::new(false));
        let worker = spawn_output_worker(
            engine_cons,
            dev_prod,
            24_000,
            48_000,
            control.clone(),
            queued.clone(),
            dev_queued.clone(),
            stop.clone(),
            failed.clone(),
        )
        .unwrap();
        (
            AudioIo {
                output_rate: 48_000,
                target_out_rate: 24_000,
                output_stream: None,
                output_worker: Some(worker),
                engine_prod: Mutex::new(engine_prod),
                control,
                queued,
                dev_queued,
                underruns: Arc::new(AtomicU64::new(0)),
                stop,
                failed,
            },
            dev_cons,
        )
    }

    #[test]
    fn cancellation_final_drain_waits_for_worker_publication() {
        let control = PlaybackControl::default();
        let (mut prod, mut cons) = rtrb::RingBuffer::new(8);
        let queued = AtomicUsize::new(1);
        let mut last_gen = 0;
        prod.push(OutputFrame::Sample(0.8)).unwrap();
        control.muted.store(true, Ordering::Release);
        control.flush_gen.store(1, Ordering::Release);
        acknowledge_cancel(&mut cons, &control, &queued, &mut last_gen, true);
        assert_eq!(control.callback_ack.load(Ordering::Acquire), 0);
        // The worker had checked the old generation just before cancellation, and
        // publishes afterwards. Only its subsequent ack makes the final drain safe.
        prod.push(OutputFrame::Sample(0.9)).unwrap();
        control.worker_ack.store(1, Ordering::Release);
        control.producer_active.store(true, Ordering::SeqCst);
        acknowledge_cancel(&mut cons, &control, &queued, &mut last_gen, true);
        assert_eq!(control.callback_ack.load(Ordering::Acquire), 0);
        prod.push(OutputFrame::Sample(0.95)).unwrap();
        control.producer_active.store(false, Ordering::SeqCst);
        acknowledge_cancel(&mut cons, &control, &queued, &mut last_gen, true);
        assert_eq!(control.callback_ack.load(Ordering::Acquire), 1);
        prod.push(OutputFrame::Sample(-0.2)).unwrap();
        assert!(matches!(cons.pop(), Ok(OutputFrame::Sample(s)) if s == -0.2));
        assert!(
            cons.pop().is_err(),
            "cancelled samples survived in the playback ring"
        );
    }

    #[test]
    fn early_cancel_is_acknowledged_and_next_speech_is_clean() {
        let control = Arc::new(PlaybackControl::default());
        control.muted.store(true, Ordering::Release);
        control.flush_gen.store(1, Ordering::Release);
        let (io, mut dev) = playback(control.clone());
        wait(|| control.worker_ack.load(Ordering::Acquire) == 1);
        let mut last_gen = 0;
        acknowledge_cancel(&mut dev, &control, &io.dev_queued, &mut last_gen, true);
        io.push_output(&[0.5; 37]).unwrap();
        io.finish_output().unwrap();
        let actual = collect(&io, &mut dev);
        assert_eq!(actual.len(), 74);
        assert!(actual.iter().any(|s| s.abs() > 0.4));
    }

    #[test]
    fn cancelling_full_playback_rings_does_not_contaminate_next_signal() {
        let control = Arc::new(PlaybackControl::default());
        let (io, mut dev) = playback(control.clone());
        let mut last_gen = 0;
        for _ in 0..8 {
            io.push_output(&[0.9; 4000]).unwrap();
            wait(|| dev.slots() == 128);
            io.flush_output();
            let gen = control.flush_gen.load(Ordering::Acquire);
            wait(|| control.worker_ack.load(Ordering::Acquire) == gen);
            acknowledge_cancel(&mut dev, &control, &io.dev_queued, &mut last_gen, true);
            assert!(dev.pop().is_err());
            let mut fresh = vec![0.0; 37];
            fresh[25] = -1.0;
            io.push_output(&fresh).unwrap();
            io.finish_output().unwrap();
            let actual = collect(&io, &mut dev);
            let mut reference = StreamResampler::new(24_000, 48_000).unwrap();
            let mut expected = reference.process(&fresh);
            expected.extend(reference.flush());
            assert_eq!(
                actual, expected,
                "cancelled positive speech leaked into the next negative impulse"
            );
        }
    }

    fn collect(io: &AudioIo, dev: &mut rtrb::Consumer<OutputFrame>) -> Vec<f32> {
        let mut out = Vec::new();
        wait(|| {
            while let Ok(frame) = dev.pop() {
                if let OutputFrame::Sample(s) = frame {
                    out.push(s);
                    let _ = io
                        .dev_queued
                        .try_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                            Some(v.saturating_sub(1))
                        });
                }
            }
            io.output_queued_frames() == 0
        });
        // Counters must not report completion before all ring samples are consumed.
        assert!(dev.pop().is_err());
        assert!(!io.failed());
        out
    }

    #[test]
    fn completion_flushes_short_clips_and_resets_between_utterances() {
        let (io, mut dev) = playback(Arc::new(PlaybackControl::default()));
        for length in [37, 977, 480] {
            let mut input = vec![0.0; length];
            input[length - 12] = 1.0;
            io.push_output(&input[..length / 2]).unwrap();
            wait(|| io.queued.load(Ordering::Relaxed) == 0);
            // A temporary streaming gap must neither declare drained nor flush DSP.
            assert!(io.output_queued_frames() > 0);
            io.push_output(&input[length / 2..]).unwrap();
            io.finish_output().unwrap();
            let actual = collect(&io, &mut dev);
            let mut reference = StreamResampler::new(24_000, 48_000).unwrap();
            let mut expected = reference.process(&input);
            expected.extend(reference.flush());
            assert_eq!(
                actual, expected,
                "actual device queue differs for {length}-sample clip"
            );
            assert!(actual.iter().any(|s| s.abs() > 0.5));
        }
    }

    #[test]
    fn capture_rejects_zero_blocks_without_opening_devices() {
        let mut cfg = AudioConfig::default();
        cfg.block_frames = 0;
        assert!(
            Capture::start(&cfg, Box::new(|_| panic!("must not capture")))
                .err()
                .unwrap()
                .to_string()
                .contains("non-zero")
        );
        let (_, cons) = rtrb::RingBuffer::new(8);
        assert!(spawn_input_worker(
            cons,
            Box::new(|_| panic!("must not capture")),
            48_000,
            16_000,
            0,
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false))
        )
        .is_err());
    }

    #[test]
    fn failed_device_startup_guard_stops_and_joins_worker() {
        let stop = Arc::new(AtomicBool::new(false));
        let exited = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let worker_exited = exited.clone();
        let guard = StartingWorker {
            stop: stop.clone(),
            handle: Some(std::thread::spawn(move || {
                while !worker_stop.load(Ordering::Acquire) {
                    std::thread::yield_now();
                }
                worker_exited.store(true, Ordering::Release);
            })),
        };
        // Both CPAL build/play failures unwind through this same owner.
        drop(guard);
        assert!(stop.load(Ordering::Acquire));
        assert!(
            exited.load(Ordering::Acquire),
            "worker was detached rather than joined"
        );
    }
}

//! The main end-user interface to the meta-tracing system.

use std::{
    assert_matches::debug_assert_matches,
    cell::RefCell,
    cmp,
    collections::{HashMap, VecDeque},
    env,
    error::Error,
    ffi::c_void,
    marker::PhantomData,
    sync::{
        atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicU64, AtomicUsize, Ordering},
        Arc,
    },
    thread::{self, JoinHandle},
};

use parking_lot::{Condvar, Mutex, MutexGuard};
#[cfg(not(all(feature = "yk_testing", not(test))))]
use parking_lot_core::SpinWait;

use crate::{
    aotsmp::{load_aot_stackmaps, AOT_STACKMAPS},
    compile::{default_compiler, CompilationError, CompiledTrace, Compiler, GuardIdx},
    location::{HotLocation, HotLocationKind, Location, TraceFailed},
    log::{
        stats::{Stats, TimingState},
        Log, Verbosity,
    },
    trace::{default_tracer, AOTTraceIterator, TraceRecorder, Tracer},
};

// The HotThreshold must be less than a machine word wide for [`Location::Location`] to do its
// pointer tagging thing. We therefore choose a type which makes this statically clear to
// users rather than having them try to use (say) u64::max() on a 64 bit machine and get a run-time
// error.
#[cfg(target_pointer_width = "64")]
pub type HotThreshold = u32;
#[cfg(target_pointer_width = "64")]
type AtomicHotThreshold = AtomicU32;

/// How often can a [HotLocation] or [Guard] lead to an error in tracing or compilation before we
/// give up trying to trace (or compile...) it?
pub type TraceCompilationErrorThreshold = u16;
pub type AtomicTraceCompilationErrorThreshold = AtomicU16;

/// How many basic blocks long can a trace be before we give up trying to compile it? Note that the
/// slower our compiler, the lower this will have to be in order to give the perception of
/// reasonable performance.
/// FIXME: needs to be configurable.
pub(crate) const DEFAULT_TRACE_TOO_LONG: usize = 20000;
const DEFAULT_HOT_THRESHOLD: HotThreshold = 131;
const DEFAULT_SIDETRACE_THRESHOLD: HotThreshold = 5;
/// How often can a [HotLocation] or [Guard] lead to an error in tracing or compilation before we
/// give up trying to trace (or compile...) it?
const DEFAULT_TRACECOMPILATION_ERROR_THRESHOLD: TraceCompilationErrorThreshold = 5;
static REG64_SIZE: usize = 8;

thread_local! {
    /// This thread's [MTThread]. Do not access this directly: use [MTThread::with_borrow] or
    /// [MTThread::with_borrow_mut].
    static THREAD_MTTHREAD: RefCell<MTThread> = RefCell::new(MTThread::new());
}

/// A meta-tracer. This is always passed around stored in an [Arc].
///
/// When you are finished with this meta-tracer, it is best to explicitly call [MT::shutdown] to
/// perform shutdown tasks (though the correctness of the system is not impacted if you do not call
/// this function).
pub struct MT {
    /// Have we been requested to shutdown this meta-tracer? In a sense this is merely advisory:
    /// since [MT] is contained within an [Arc], if we're able to read this value, then the
    /// meta-tracer is still working and it may go on doing so for an arbitrary period of time.
    /// However, it means that some "shutdown" activities, such as printing statistics and checking
    /// for failed compilation threads, have already occurred, and should not be repeated.
    shutdown: AtomicBool,
    hot_threshold: AtomicHotThreshold,
    sidetrace_threshold: AtomicHotThreshold,
    trace_failure_threshold: AtomicTraceCompilationErrorThreshold,
    /// The ordered queue of compilation worker functions.
    job_queue: Arc<(Condvar, Mutex<VecDeque<Box<dyn FnOnce() + Send>>>)>,
    /// The hard cap on the number of worker threads.
    max_worker_threads: AtomicUsize,
    /// [JoinHandle]s to each worker thread so that when an [MT] value is dropped, we can try
    /// joining each worker thread and see if it caused an error or not. If it did,  we can
    /// percolate the error upwards, making it more likely that the main thread exits with an
    /// error. In other words, this [Vec] makes it harder for errors to be missed.
    active_worker_threads: Mutex<Vec<JoinHandle<()>>>,
    /// The [Tracer] that should be used for creating future traces. Note that this might not be
    /// the same as the tracer(s) used to create past traces.
    tracer: Mutex<Arc<dyn Tracer>>,
    /// The [Compiler] that will be used for compiling future `IRTrace`s. Note that this might not
    /// be the same as the compiler(s) used to compile past `IRTrace`s.
    compiler: Mutex<Arc<dyn Compiler>>,
    /// A monotonically increasing integer that uniquely identifies each compiled trace.
    compiled_trace_id: AtomicU64,
    /// The currently available compiled traces. This is a [HashMap] because it is potentially a
    /// sparse mapping due to (1) (one day!) we might garbage collect traces (2) some
    /// [CompiledTraceId]s that we hand out are "lost" because a trace failed to compile.
    compiled_traces: Mutex<HashMap<CompiledTraceId, Arc<dyn CompiledTrace>>>,
    pub(crate) log: Log,
    pub(crate) stats: Stats,
}

impl std::fmt::Debug for MT {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "MT")
    }
}

impl MT {
    // Create a new meta-tracer instance. Arbitrarily many of these can be created, though there
    // are no guarantees as to whether they will share resources effectively or fairly.
    pub fn new() -> Result<Arc<Self>, Box<dyn Error>> {
        load_aot_stackmaps();
        let hot_threshold = match env::var("YK_HOT_THRESHOLD") {
            Ok(s) => s
                .parse::<HotThreshold>()
                .map_err(|e| format!("Invalid hot threshold '{s}': {e}"))?,
            Err(_) => DEFAULT_HOT_THRESHOLD,
        };
        Ok(Arc::new(Self {
            shutdown: AtomicBool::new(false),
            hot_threshold: AtomicHotThreshold::new(hot_threshold),
            sidetrace_threshold: AtomicHotThreshold::new(DEFAULT_SIDETRACE_THRESHOLD),
            trace_failure_threshold: AtomicTraceCompilationErrorThreshold::new(
                DEFAULT_TRACECOMPILATION_ERROR_THRESHOLD,
            ),
            job_queue: Arc::new((Condvar::new(), Mutex::new(VecDeque::new()))),
            max_worker_threads: AtomicUsize::new(cmp::max(1, num_cpus::get() - 1)),
            active_worker_threads: Mutex::new(Vec::new()),
            tracer: Mutex::new(default_tracer()?),
            compiler: Mutex::new(default_compiler()?),
            compiled_trace_id: AtomicU64::new(0),
            compiled_traces: Mutex::new(HashMap::new()),
            log: Log::new()?,
            stats: Stats::new(),
        }))
    }

    /// Put this meta-tracer into shutdown mode, panicking if any problems are discovered. This
    /// will perform actions such as printing summary statistics and checking whether any worker
    /// threads have caused an error. The best place to do this is likely to be on the main thread,
    /// though this is not mandatory.
    ///
    /// Note: this method does not stop all of the meta-tracer's activities. For example, -- but
    /// not only! -- other threads will continue compiling and executing traces.
    ///
    /// Only the first call of this method performs meaningful actions: any subsequent calls will
    /// note the previous shutdown and immediately return.
    pub fn shutdown(&self) {
        if !self.shutdown.swap(true, Ordering::Relaxed) {
            self.stats.timing_state(TimingState::None);
            self.stats.output();
            let mut lk = self.active_worker_threads.lock();
            for hdl in lk.drain(..) {
                if hdl.is_finished() {
                    if let Err(e) = hdl.join() {
                        // Despite the name `resume_unwind` will abort if the unwind strategy in
                        // Rust is set to `abort`.
                        std::panic::resume_unwind(e);
                    }
                }
            }
        }
    }

    /// Return this `MT` instance's current hot threshold. Notice that this value can be changed by
    /// other threads and is thus potentially stale as soon as it is read.
    pub fn hot_threshold(self: &Arc<Self>) -> HotThreshold {
        self.hot_threshold.load(Ordering::Relaxed)
    }

    /// Set the threshold at which `Location`'s are considered hot.
    pub fn set_hot_threshold(self: &Arc<Self>, hot_threshold: HotThreshold) {
        self.hot_threshold.store(hot_threshold, Ordering::Relaxed);
    }

    /// Return this `MT` instance's current side-trace threshold. Notice that this value can be
    /// changed by other threads and is thus potentially stale as soon as it is read.
    pub fn sidetrace_threshold(self: &Arc<Self>) -> HotThreshold {
        self.sidetrace_threshold.load(Ordering::Relaxed)
    }

    /// Set the threshold at which guard failures are considered hot and side-tracing should start.
    pub fn set_sidetrace_threshold(self: &Arc<Self>, hot_threshold: HotThreshold) {
        self.sidetrace_threshold
            .store(hot_threshold, Ordering::Relaxed);
    }

    /// Return this `MT` instance's current trace failure threshold. Notice that this value can be
    /// changed by other threads and is thus potentially stale as soon as it is read.
    pub fn trace_failure_threshold(self: &Arc<Self>) -> TraceCompilationErrorThreshold {
        self.trace_failure_threshold.load(Ordering::Relaxed)
    }

    /// Set the threshold at which a `Location` from which tracing has failed multiple times is
    /// marked as "do not try tracing again".
    pub fn set_trace_failure_threshold(
        self: &Arc<Self>,
        trace_failure_threshold: TraceCompilationErrorThreshold,
    ) {
        if trace_failure_threshold < 1 {
            panic!("Trace failure threshold must be >= 1.");
        }
        self.trace_failure_threshold
            .store(trace_failure_threshold, Ordering::Relaxed);
    }

    /// Return this meta-tracer's maximum number of worker threads. Notice that this value can be
    /// changed by other threads and is thus potentially stale as soon as it is read.
    pub fn max_worker_threads(self: &Arc<Self>) -> usize {
        self.max_worker_threads.load(Ordering::Relaxed)
    }

    /// Return the unique ID for the next compiled trace.
    pub(crate) fn next_compiled_trace_id(self: &Arc<Self>) -> CompiledTraceId {
        // Note: fetch_add is documented to wrap on overflow.
        let ctr_id = self.compiled_trace_id.fetch_add(1, Ordering::Relaxed);
        if ctr_id == u64::MAX {
            // OK, OK, technically we have 1 ID left that we could use, but if we've actually
            // managed to compile u64::MAX traces, it's probable that something's gone wrong.
            panic!("Ran out of trace IDs");
        }
        CompiledTraceId(ctr_id)
    }

    /// Queue `job` to be run on a worker thread.
    fn queue_job(self: &Arc<Self>, job: Box<dyn FnOnce() + Send>) {
        #[cfg(feature = "yk_testing")]
        if let Ok(true) = env::var("YKD_SERIALISE_COMPILATION").map(|x| x.as_str() == "1") {
            // To ensure that we properly test that compilation can occur in another thread, we
            // spin up a new thread for each compilation. This is only acceptable because a)
            // `SERIALISE_COMPILATION` is an internal yk testing feature b) when we use it we're
            // checking correctness, not performance.
            thread::spawn(job).join().unwrap();
            return;
        }

        // Push the job onto the queue.
        let (cv, mtx) = &*self.job_queue;
        mtx.lock().push_back(job);
        cv.notify_one();

        // Do we have enough active worker threads? If not, spin another up.

        let mut lk = self.active_worker_threads.lock();
        if lk.len() < self.max_worker_threads.load(Ordering::Relaxed) {
            // We only keep a weak reference alive to `self`, as otherwise an active compiler job
            // causes `self` to never be dropped.
            let mt = Arc::downgrade(self);
            let jq = Arc::clone(&self.job_queue);
            let hdl = thread::spawn(move || {
                let (cv, mtx) = &*jq;
                let mut lock = mtx.lock();
                // If the strong count for `mt` is 0 then it has been dropped and there is no
                // point trying to do further work, even if there is work in the queue.
                while mt.upgrade().is_some() {
                    match lock.pop_front() {
                        Some(x) => {
                            MutexGuard::unlocked(&mut lock, x);
                        }
                        None => cv.wait(&mut lock),
                    }
                }
            });
            lk.push(hdl);
        }
    }

    /// Add a compilation job for a root trace where `hl_arc` is the [HotLocation] this compilation
    /// job is related to.
    fn queue_root_compile_job(
        self: &Arc<Self>,
        // FIXME: this tuple is too long.
        trace_iter: (Box<dyn AOTTraceIterator>, Box<[u8]>, Vec<String>, usize),
        hl_arc: Arc<Mutex<HotLocation>>,
    ) {
        self.stats.trace_recorded_ok();
        let mt = Arc::clone(self);
        let do_compile = move || {
            let compiler = {
                let lk = mt.compiler.lock();
                Arc::clone(&*lk)
            };
            mt.stats.timing_state(TimingState::Compiling);
            match compiler.root_compile(
                Arc::clone(&mt),
                trace_iter.0,
                Arc::clone(&hl_arc),
                trace_iter.1,
                trace_iter.2,
                trace_iter.3,
            ) {
                Ok(ctr) => {
                    mt.compiled_traces
                        .lock()
                        .insert(ctr.ctrid(), Arc::clone(&ctr));
                    let mut hl = hl_arc.lock();
                    debug_assert_matches!(hl.kind, HotLocationKind::Compiling);
                    hl.kind = HotLocationKind::Compiled(ctr);
                    mt.stats.trace_compiled_ok();
                }
                Err(e) => {
                    mt.stats.trace_compiled_err();
                    let mut hl = hl_arc.lock();
                    debug_assert_matches!(hl.kind, HotLocationKind::Compiling);
                    if let TraceFailed::DontTrace = hl.tracecompilation_error(&mt) {
                        hl.kind = HotLocationKind::DontTrace;
                    } else {
                        hl.kind = HotLocationKind::Counting(0);
                    }
                    match e {
                        CompilationError::General(e) | CompilationError::LimitExceeded(e) => {
                            mt.log.log(
                                Verbosity::Warning,
                                &format!("trace-compilation-aborted: {e}"),
                            );
                        }
                        CompilationError::InternalError(e) => {
                            #[cfg(feature = "ykd")]
                            panic!("{e}");
                            #[cfg(not(feature = "ykd"))]
                            {
                                mt.log.log(
                                    Verbosity::Error,
                                    &format!("trace-compilation-aborted: {e}"),
                                );
                            }
                        }
                        CompilationError::ResourceExhausted(e) => {
                            mt.log
                                .log(Verbosity::Error, &format!("trace-compilation-aborted: {e}"));
                        }
                    }
                }
            }

            mt.stats.timing_state(TimingState::None);
        };

        self.queue_job(Box::new(do_compile));
    }

    /// Add a compilation job for a sidetrace where: `hl_arc` is the [HotLocation] this compilation
    ///   * `hl_arc` is the [HotLocation] this compilation job is related to.
    ///   * `root_ctr` is the root [CompiledTrace].
    ///   * `parent_ctr` is the parent [CompiledTrace] of the side-trace that's about to be
    ///     compiled. Because side-traces can nest, this may or may not be the same [CompiledTrace]
    ///     as `root_ctr`.
    ///   * `guardid` is the ID of the guard in `parent_ctr` which failed.
    fn queue_sidetrace_compile_job(
        self: &Arc<Self>,
        trace_iter: (Box<dyn AOTTraceIterator>, Box<[u8]>, Vec<String>),
        hl_arc: Arc<Mutex<HotLocation>>,
        root_ctr: Arc<dyn CompiledTrace>,
        parent_ctr: Arc<dyn CompiledTrace>,
        guardid: GuardIdx,
    ) {
        self.stats.trace_recorded_ok();
        let mt = Arc::clone(self);
        let do_compile = move || {
            let compiler = {
                let lk = mt.compiler.lock();
                Arc::clone(&*lk)
            };
            mt.stats.timing_state(TimingState::Compiling);
            let sti = parent_ctr.sidetraceinfo(Arc::clone(&root_ctr), guardid);
            // FIXME: Can we pass in the root trace address, root trace entry variable locations,
            // and the base stack-size from here, rather than spreading them out via
            // DeoptInfo/SideTraceInfo, and CompiledTrace?
            match compiler.sidetrace_compile(
                Arc::clone(&mt),
                trace_iter.0,
                sti,
                Arc::clone(&hl_arc),
                trace_iter.1,
                trace_iter.2,
            ) {
                Ok(ctr) => {
                    mt.compiled_traces
                        .lock()
                        .insert(ctr.ctrid(), Arc::clone(&ctr));
                    parent_ctr.guard(guardid).set_ctr(ctr, &parent_ctr, guardid);
                    mt.stats.trace_compiled_ok();
                }
                Err(e) => {
                    // FIXME: We need to track how often compiling a guard fails.
                    mt.stats.trace_compiled_err();
                    match e {
                        CompilationError::General(e) | CompilationError::LimitExceeded(e) => {
                            mt.log.log(
                                Verbosity::Warning,
                                &format!("trace-compilation-aborted: {e}"),
                            );
                        }
                        CompilationError::InternalError(e) => {
                            #[cfg(feature = "ykd")]
                            panic!("{e}");
                            #[cfg(not(feature = "ykd"))]
                            {
                                mt.log.log(
                                    Verbosity::Error,
                                    &format!("trace-compilation-aborted: {e}"),
                                );
                            }
                        }
                        CompilationError::ResourceExhausted(e) => {
                            mt.log
                                .log(Verbosity::Error, &format!("trace-compilation-aborted: {e}"));
                        }
                    }
                }
            }

            mt.stats.timing_state(TimingState::None);
        };

        self.queue_job(Box::new(do_compile));
    }

    #[allow(clippy::not_unsafe_ptr_arg_deref)]
    pub fn control_point(self: &Arc<Self>, loc: &Location, frameaddr: *mut c_void, smid: u64) {
        match self.transition_control_point(loc, frameaddr) {
            TransitionControlPoint::NoAction => (),
            TransitionControlPoint::AbortTracing(ak) => {
                let thread_tracer = MTThread::with_borrow_mut(|mtt| match mtt.pop_tstate() {
                    MTThreadState::Tracing { thread_tracer, .. } => thread_tracer,
                    _ => unreachable!(),
                });
                thread_tracer.stop().ok();
                self.log
                    .log(Verbosity::Warning, &format!("tracing-aborted: {ak}"));
            }
            TransitionControlPoint::Execute(ctr) => {
                self.log.log(Verbosity::JITEvent, "enter-jit-code");
                self.stats.trace_executed();

                // Compute the rsp of the control_point frame.
                let (rec, pinfo) = AOT_STACKMAPS
                    .as_ref()
                    .unwrap()
                    .get(usize::try_from(smid).unwrap());
                let mut rsp = unsafe { frameaddr.byte_sub(usize::try_from(rec.size).unwrap()) };
                if pinfo.hasfp {
                    rsp = unsafe { rsp.byte_add(REG64_SIZE) };
                }
                let trace_addr = ctr.entry();
                MTThread::with_borrow_mut(|mtt| {
                    mtt.push_tstate(MTThreadState::Executing { ctr });
                });
                self.stats.timing_state(TimingState::JitExecuting);

                // FIXME: Calling this function overwrites the current (Rust) function frame,
                // rather than unwinding it. https://github.com/ykjit/yk/issues/778.
                unsafe { __yk_exec_trace(frameaddr, rsp, trace_addr) };
            }
            TransitionControlPoint::StartTracing(hl) => {
                self.log.log(Verbosity::JITEvent, "start-tracing");
                let tracer = {
                    let lk = self.tracer.lock();
                    Arc::clone(&*lk)
                };
                match Arc::clone(&tracer).start_recorder() {
                    Ok(tt) => MTThread::with_borrow_mut(|mtt| {
                        mtt.push_tstate(MTThreadState::Tracing {
                            hl,
                            thread_tracer: tt,
                            promotions: Vec::new(),
                            debug_strs: Vec::new(),
                            frameaddr,
                            seen_hls: HashMap::new(),
                            cp_idx: 0,
                        });
                    }),
                    Err(e) => {
                        // FIXME: start_recorder needs a way of signalling temporary errors.
                        #[cfg(tracer_hwt)]
                        match e.downcast::<hwtracer::HWTracerError>() {
                            Ok(e) => {
                                if let hwtracer::HWTracerError::Temporary(_) = *e {
                                    let mut lk = hl.lock();
                                    debug_assert_matches!(lk.kind, HotLocationKind::Tracing);
                                    lk.tracecompilation_error(self);
                                    // FIXME: This is stupidly brutal.
                                    lk.kind = HotLocationKind::DontTrace;
                                    drop(lk);
                                    self.log.log(Verbosity::Warning, "start-tracing-abort");
                                } else {
                                    todo!("{e:?}");
                                }
                            }
                            Err(e) => todo!("{e:?}"),
                        }
                        #[cfg(not(tracer_hwt))]
                        todo!("{e:?}");
                    }
                }
            }
            TransitionControlPoint::StopTracing { start_cp_idx } => {
                // Assuming no bugs elsewhere, the `unwrap`s cannot fail, because `StartTracing`
                // will have put a `Some` in the `Rc`.
                let (hl, thread_tracer, promotions, debug_strs) =
                    MTThread::with_borrow_mut(|mtt| match mtt.pop_tstate() {
                        MTThreadState::Tracing {
                            hl,
                            thread_tracer,
                            promotions,
                            debug_strs,
                            frameaddr: tracing_frameaddr,
                            seen_hls: _,
                            cp_idx: _,
                        } => {
                            // If this assert fails then the code in `transition_control_point`,
                            // which rejects traces that end in another frame, didn't work.
                            assert_eq!(frameaddr, tracing_frameaddr);
                            (hl, thread_tracer, promotions, debug_strs)
                        }
                        _ => unreachable!(),
                    });
                match thread_tracer.stop() {
                    Ok(utrace) => {
                        self.stats.timing_state(TimingState::None);
                        if start_cp_idx == 0 {
                            self.log.log(Verbosity::JITEvent, "stop-tracing");
                        } else {
                            self.log
                                .log(Verbosity::JITEvent, "stop-tracing (inner loop detected)");
                        }
                        self.queue_root_compile_job(
                            (
                                utrace,
                                promotions.into_boxed_slice(),
                                debug_strs,
                                start_cp_idx,
                            ),
                            hl,
                        );
                    }
                    Err(e) => {
                        self.stats.timing_state(TimingState::None);
                        self.stats.trace_recorded_err();
                        self.log
                            .log(Verbosity::Warning, &format!("stop-tracing-aborted: {e}"));
                    }
                }
            }
            TransitionControlPoint::StopSideTracing {
                gidx: guardid,
                parent_ctr,
                root_ctr,
            } => {
                // Assuming no bugs elsewhere, the `unwrap`s cannot fail, because
                // `StartSideTracing` will have put a `Some` in the `Rc`.
                let (hl, thread_tracer, promotions, debug_strs) =
                    MTThread::with_borrow_mut(|mtt| match mtt.pop_tstate() {
                        MTThreadState::Tracing {
                            hl,
                            thread_tracer,
                            promotions,
                            debug_strs,
                            frameaddr: tracing_frameaddr,
                            seen_hls: _,
                            cp_idx: _,
                        } => {
                            assert_eq!(frameaddr, tracing_frameaddr);
                            (hl, thread_tracer, promotions, debug_strs)
                        }
                        _ => unreachable!(),
                    });
                self.stats.timing_state(TimingState::TraceMapping);
                match thread_tracer.stop() {
                    Ok(utrace) => {
                        self.stats.timing_state(TimingState::None);
                        self.log.log(Verbosity::JITEvent, "stop-tracing");
                        self.queue_sidetrace_compile_job(
                            (utrace, promotions.into_boxed_slice(), debug_strs),
                            hl,
                            root_ctr,
                            parent_ctr,
                            guardid,
                        );
                    }
                    Err(e) => {
                        self.stats.timing_state(TimingState::None);
                        self.stats.trace_recorded_err();
                        self.log
                            .log(Verbosity::Warning, &format!("stop-tracing-aborted: {e}"));
                    }
                }
            }
        }
    }

    /// Perform the next step to `loc` in the `Location` state-machine for a control point. If
    /// `loc` moves to the Compiled state, return a pointer to a [CompiledTrace] object.
    fn transition_control_point(
        self: &Arc<Self>,
        loc: &Location,
        frameaddr: *mut c_void,
    ) -> TransitionControlPoint {
        MTThread::with_borrow_mut(|mtt| {
            let is_tracing = mtt.is_tracing();
            match loc.hot_location() {
                Some(hl) => {
                    if is_tracing {
                        if let MTThreadState::Tracing {
                            frameaddr: tracing_frameaddr,
                            hl: ref mut tracing_hl,
                            seen_hls,
                            cp_idx,
                            ..
                        } = mtt.peek_mut_tstate()
                        {
                            let mut akind = None;
                            if frameaddr != *tracing_frameaddr {
                                // We're tracing but no longer in the frame we started in, so we
                                // need to stop tracing and report the original [HotLocation] as
                                // having failed to trace properly.
                                akind = Some(AbortKind::OutOfFrame);
                            }

                            if let Some(x) = loc.hot_location() {
                                let seen_key = x as *const Mutex<HotLocation>;
                                if seen_hls.contains_key(&seen_key) {
                                    // We found an inner loop. Compile that instead.
                                    let mut new_lk = hl.lock();
                                    // First check that the inner loop's start location is in the
                                    // counting state: i.e. it hasn't already been compiled and no
                                    // other thread is currently tracing/compiling that location
                                    // already.
                                    match new_lk.kind {
                                        HotLocationKind::Counting(_) => {
                                            // XXX is Counting(0) what we want?location.rs
                                            let mut old_lk = tracing_hl.lock();
                                            old_lk.kind = HotLocationKind::Counting(0);
                                            drop(old_lk);
                                            new_lk.kind = HotLocationKind::Compiling;
                                            *tracing_hl = loc.hot_location_arc_clone().unwrap();
                                            return TransitionControlPoint::StopTracing {
                                                start_cp_idx: seen_hls[&seen_key],
                                            };
                                        }
                                        _ => {
                                            // There's no point in tracing the inner loop.
                                            akind = Some(AbortKind::Unrolled);
                                        }
                                    }
                                } else {
                                    seen_hls.insert(x, *cp_idx);
                                }
                            }

                            if let Some(akind) = akind {
                                self.stats.trace_recorded_err();
                                let mut lk = tracing_hl.lock();
                                match &lk.kind {
                                    HotLocationKind::Compiled(_) => todo!(),
                                    HotLocationKind::Compiling => todo!(),
                                    HotLocationKind::Counting(_) => todo!(),
                                    HotLocationKind::DontTrace => todo!(),
                                    HotLocationKind::Tracing => {
                                        match lk.tracecompilation_error(self) {
                                            TraceFailed::KeepTrying => {
                                                lk.kind = HotLocationKind::Counting(0);
                                            }
                                            TraceFailed::DontTrace => {
                                                lk.kind = HotLocationKind::DontTrace;
                                            }
                                        }
                                    }
                                    HotLocationKind::SideTracing { root_ctr, .. } => {
                                        lk.kind = HotLocationKind::Compiled(Arc::clone(root_ctr));
                                    }
                                }

                                return TransitionControlPoint::AbortTracing(akind);
                            }
                        }
                    }

                    // If this thread is tracing something, we *must* grab the [HotLocation] lock,
                    // because we need to know for sure if `loc` is the point at which we should
                    // stop tracing. In most compilation modes, we are willing to give up trying to
                    // lock and return if it looks like it will take too long. When `yk_testing` is
                    // enabled, however, this introduces non-determinism, so in that compilation
                    // mode only we guarantee to grab the lock.
                    let mut lk;

                    #[cfg(not(all(feature = "yk_testing", not(test))))]
                    {
                        // If this thread is not tracing anything, however, it's not worth
                        // contending too much with other threads: we try moderately hard to grab
                        // the lock, but we don't want to park this thread.
                        if !is_tracing {
                            // This thread isn't tracing anything, so we try for a little while to grab the
                            // lock, before giving up and falling back to the interpreter. In general, we
                            // expect that we'll grab the lock rather quickly. However, there is one nasty
                            // use-case, which is when an army of threads all start executing the same
                            // piece of tiny code and end up thrashing away at a single Location,
                            // particularly when it's in a non-Compiled state: we can end up contending
                            // horribly for a single lock, and not making much progress. In that case, it's
                            // probably better to let some threads fall back to the interpreter for another
                            // iteration, and hopefully allow them to get sufficiently out-of-sync that
                            // they no longer contend on this one lock as much.
                            let mut sw = SpinWait::new();
                            loop {
                                if let Some(x) = hl.try_lock() {
                                    lk = x;
                                    break;
                                }
                                if !sw.spin() {
                                    return TransitionControlPoint::NoAction;
                                }
                            }
                        } else {
                            // This thread is tracing something, so we must grab the lock.
                            lk = hl.lock();
                        };
                    }

                    #[cfg(all(feature = "yk_testing", not(test)))]
                    {
                        lk = hl.lock();
                    }

                    match lk.kind {
                        HotLocationKind::Compiled(ref ctr) => {
                            if is_tracing {
                                // This thread is tracing something, so bail out as quickly as possible
                                TransitionControlPoint::AbortTracing(
                                    AbortKind::EncounteredCompiledTrace,
                                )
                            } else {
                                TransitionControlPoint::Execute(Arc::clone(ctr))
                            }
                        }
                        HotLocationKind::Compiling => TransitionControlPoint::NoAction,
                        HotLocationKind::Counting(c) => {
                            if is_tracing {
                                // This thread is tracing something, so bail out as quickly as possible
                                TransitionControlPoint::NoAction
                            } else if c < self.hot_threshold() {
                                lk.kind = HotLocationKind::Counting(c + 1);
                                TransitionControlPoint::NoAction
                            } else {
                                let hl = loc.hot_location_arc_clone().unwrap();
                                lk.kind = HotLocationKind::Tracing;
                                TransitionControlPoint::StartTracing(hl)
                            }
                        }
                        HotLocationKind::Tracing => {
                            let hl = loc.hot_location_arc_clone().unwrap();
                            match mtt.peek_tstate() {
                                MTThreadState::Tracing { hl: thread_hl, .. } => {
                                    // This thread is tracing something...
                                    if !Arc::ptr_eq(thread_hl, &hl) {
                                        // ...but not this Location.
                                        TransitionControlPoint::NoAction
                                    } else {
                                        // ...and it's this location...
                                        lk.kind = HotLocationKind::Compiling;
                                        TransitionControlPoint::StopTracing { start_cp_idx: 0 }
                                    }
                                }
                                _ => {
                                    // FIXME: This branch is also used by side tracing. That's not
                                    // necessarily wrong, but it wasn't what was intended. We
                                    // should at least explicitly think about whether this is the
                                    // best way of doing things or not.

                                    // This thread isn't tracing anything. Note that because we called
                                    // `hot_location_arc_clone` above, the strong count of an `Arc`
                                    // that's no longer being used by that thread will be 2.
                                    if Arc::strong_count(&hl) == 2 {
                                        // Another thread was tracing this location but it's terminated.
                                        self.stats.trace_recorded_err();
                                        match lk.tracecompilation_error(self) {
                                            TraceFailed::KeepTrying => {
                                                lk.kind = HotLocationKind::Tracing;
                                                TransitionControlPoint::StartTracing(hl)
                                            }
                                            TraceFailed::DontTrace => {
                                                // FIXME: This is stupidly brutal.
                                                lk.kind = HotLocationKind::DontTrace;
                                                TransitionControlPoint::NoAction
                                            }
                                        }
                                    } else {
                                        // Another thread is tracing this location.
                                        TransitionControlPoint::NoAction
                                    }
                                }
                            }
                        }
                        HotLocationKind::SideTracing {
                            ref root_ctr,
                            gidx,
                            ref parent_ctr,
                        } => {
                            let hl = loc.hot_location_arc_clone().unwrap();
                            match mtt.peek_tstate() {
                                MTThreadState::Tracing { hl: thread_hl, .. } => {
                                    // This thread is tracing something...
                                    if !Arc::ptr_eq(thread_hl, &hl) {
                                        // ...but not this Location.
                                        TransitionControlPoint::NoAction
                                    } else {
                                        // ...and it's this location.
                                        let parent_ctr = Arc::clone(parent_ctr);
                                        let root_ctr_cl = Arc::clone(root_ctr);
                                        lk.kind = HotLocationKind::Compiled(Arc::clone(root_ctr));
                                        TransitionControlPoint::StopSideTracing {
                                            gidx,
                                            parent_ctr,
                                            root_ctr: root_ctr_cl,
                                        }
                                    }
                                }
                                _ => {
                                    // This thread isn't tracing anything.
                                    assert!(!is_tracing);
                                    TransitionControlPoint::Execute(Arc::clone(root_ctr))
                                }
                            }
                        }
                        HotLocationKind::DontTrace => TransitionControlPoint::NoAction,
                    }
                }
                None => {
                    if is_tracing {
                        let hl_ptr = match loc.inc_count() {
                            Some(count) => {
                                let hl = HotLocation {
                                    kind: HotLocationKind::Counting(count),
                                    tracecompilation_errors: 0,
                                };
                                loc.count_to_hot_location(count, hl)
                                    .map(|x| Arc::as_ptr(&x))
                            }
                            None => loc.hot_location().map(|x| x as *const Mutex<HotLocation>),
                        };
                        if let Some(hl_ptr) = hl_ptr {
                            let MTThreadState::Tracing {
                                frameaddr: tracing_frameaddr,
                                seen_hls,
                                cp_idx,
                                ..
                            } = mtt.peek_mut_tstate()
                            else {
                                panic!()
                            };
                            if frameaddr != *tracing_frameaddr {
                                // We're tracing but no longer in the frame we started in, so we
                                // need to stop tracing and report the original [HotLocation] as
                                // having failed to trace properly.
                                return TransitionControlPoint::AbortTracing(AbortKind::OutOfFrame);
                            }
                            if seen_hls.contains_key(&hl_ptr) {
                                todo!();
                            } else {
                                seen_hls.insert(hl_ptr as *const Mutex<HotLocation>, *cp_idx);
                            }
                        }
                        return TransitionControlPoint::NoAction;
                    }
                    match loc.inc_count() {
                        Some(x) => {
                            debug_assert!(self.hot_threshold() < HotThreshold::MAX);
                            if x < self.hot_threshold() + 1 {
                                TransitionControlPoint::NoAction
                            } else {
                                let hl = HotLocation {
                                    kind: HotLocationKind::Tracing,
                                    tracecompilation_errors: 0,
                                };
                                if let Some(hl) = loc.count_to_hot_location(x, hl) {
                                    debug_assert!(!is_tracing);
                                    TransitionControlPoint::StartTracing(hl)
                                } else {
                                    // We raced with another thread which has started tracing this
                                    // location. We leave it to do the tracing.
                                    TransitionControlPoint::NoAction
                                }
                            }
                        }
                        None => {
                            // `loc` is being updated by another thread and we've caught it in the
                            // middle of that. We could spin but we might as well let the other thread
                            // do its thing and go around the interpreter again.
                            TransitionControlPoint::NoAction
                        }
                    }
                }
            }
        })
    }

    /// Perform the next step in the guard failure statemachine.
    pub(crate) fn transition_guard_failure(
        self: &Arc<Self>,
        parent_ctr: Arc<dyn CompiledTrace>,
        gidx: GuardIdx,
    ) -> TransitionGuardFailure {
        if parent_ctr.guard(gidx).inc_failed(self) {
            if let Some(hl) = parent_ctr.hl().upgrade() {
                MTThread::with_borrow_mut(|mtt| {
                    // This thread should not be tracing anything.
                    debug_assert!(!mtt.is_tracing());
                    let mut lk = hl.lock();
                    if let HotLocationKind::Compiled(ref root_ctr) = lk.kind {
                        lk.kind = HotLocationKind::SideTracing {
                            root_ctr: Arc::clone(root_ctr),
                            gidx,
                            parent_ctr,
                        };
                        drop(lk);
                        TransitionGuardFailure::StartSideTracing(hl)
                    } else {
                        // The top-level trace's [HotLocation] might have changed to another state while
                        // the associated trace was executing; or we raced with another thread (which is
                        // most likely to have started side tracing itself).
                        TransitionGuardFailure::NoAction
                    }
                })
            } else {
                // The parent [HotLocation] has been garbage collected.
                TransitionGuardFailure::NoAction
            }
        } else {
            // We're side-tracing
            TransitionGuardFailure::NoAction
        }
    }

    /// Inform this `MT` instance that `deopt` has occurred: this updates the stack of
    /// [MTThreadState]s.
    pub(crate) fn deopt(self: &Arc<Self>) {
        loop {
            let st = MTThread::with_borrow_mut(|mtt| mtt.pop_tstate());
            match st {
                MTThreadState::Interpreting => todo!(),
                MTThreadState::Tracing {
                    hl, thread_tracer, ..
                } => {
                    let mut lk = hl.lock();
                    match &lk.kind {
                        HotLocationKind::Compiled(_) => todo!(),
                        HotLocationKind::Compiling => todo!(),
                        HotLocationKind::Counting(_) => todo!(),
                        HotLocationKind::DontTrace => todo!(),
                        HotLocationKind::Tracing => match lk.tracecompilation_error(self) {
                            TraceFailed::KeepTrying => {
                                lk.kind = HotLocationKind::Counting(0);
                            }
                            TraceFailed::DontTrace => {
                                lk.kind = HotLocationKind::DontTrace;
                            }
                        },
                        HotLocationKind::SideTracing { root_ctr, .. } => {
                            lk.kind = HotLocationKind::Compiled(Arc::clone(root_ctr));
                        }
                    }
                    drop(lk);
                    thread_tracer.stop().ok();
                    self.log.log(
                        Verbosity::Warning,
                        &format!("tracing-aborted: {}", AbortKind::BackIntoExecution),
                    );
                }
                MTThreadState::Executing { .. } => return,
            }
        }
    }

    /// Inform this meta-tracer that guard `gidx` has failed.
    ///
    // FIXME: Don't side trace the last guard of a side-trace as this guard always fails.
    // FIXME: Don't side-trace after switch instructions: not every guard failure is equal
    // and a trace compiled for case A won't work for case B.
    pub(crate) fn guard_failure(
        self: &Arc<Self>,
        parent: Arc<dyn CompiledTrace>,
        gidx: GuardIdx,
        frameaddr: *mut c_void,
    ) {
        match self.transition_guard_failure(parent, gidx) {
            TransitionGuardFailure::NoAction => (),
            TransitionGuardFailure::StartSideTracing(hl) => {
                self.log.log(Verbosity::JITEvent, "start-side-tracing");
                let tracer = {
                    let lk = self.tracer.lock();
                    Arc::clone(&*lk)
                };
                match Arc::clone(&tracer).start_recorder() {
                    Ok(tt) => MTThread::with_borrow_mut(|mtt| {
                        mtt.push_tstate(MTThreadState::Tracing {
                            hl,
                            thread_tracer: tt,
                            promotions: Vec::new(),
                            debug_strs: Vec::new(),
                            frameaddr,
                            seen_hls: HashMap::new(),
                            cp_idx: 0,
                        })
                    }),
                    Err(e) => todo!("{e:?}"),
                }
            }
        }
    }
}

#[cfg(target_arch = "x86_64")]
#[naked]
#[no_mangle]
unsafe extern "C" fn __yk_exec_trace(
    frameaddr: *const c_void,
    rsp: *const c_void,
    trace: *const c_void,
) -> ! {
    std::arch::naked_asm!(
        // Reset RBP
        "mov rbp, rdi",
        // Reset RSP to the end of the control point frame (this includes the registers we pushed
        // just before the control point)
        "mov rsp, rsi",
        "sub rsp, 8",   // Return address of control point call
        "sub rsp, 104", // Registers pushed in naked cp call (includes alignment)
        // Restore registers which were pushed to the stack in [ykcapi::__ykrt_control_point].
        "pop r15",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop r11",
        "pop r10",
        "pop r9",
        "pop r8",
        "pop rsi",
        "pop rdi",
        "pop rbx",
        "pop rcx",
        "pop rax",
        "add rsp, 8", // Remove return pointer
        // Call the trace function.
        "jmp rdx",
        "ret",
    )
}

/// [MTThread]'s major job is to record what state in the "interpreting/tracing/executing"
/// state-machine this thread is in. This enum contains the states.
enum MTThreadState {
    /// This thread is executing in the normal interpreter: it is not executing a trace or
    /// recording a trace.
    Interpreting,
    /// This thread is recording a trace.
    Tracing {
        /// Which [Location]s have we seen so far in this trace? If we see a [Location] twice then
        /// we know we've found an inner loop (e.g. we started tracing an outer loop and have
        /// started to unroll an inner loop).
        ///
        /// Tracking [Location]s directly is tricky as they have no inherent ID. To solve that, for
        /// the time being we force every `Location` that we encounter in a trace to become a
        /// [HotLocation] (with kind [HotLocationKind::Counting]) if it is not already. We can then
        /// use the (unmoving) pointer to a [HotLocation]'s inner [Mutex] as an ID.
        ///
        /// Maps the [HotLocation]'s "ID" to the occurance of the control point where we last saw
        /// it (if we count the calls the control point in the trace, starting from zero).
        seen_hls: HashMap<*const Mutex<HotLocation>, usize>,
        /// How many times we've seen the control point since starting tracing.
        cp_idx: usize,
        /// The [HotLocation] the trace will end at. For a top-level trace, this will be the same
        /// [HotLocation] the trace started at; for a side-trace, tracing started elsewhere.
        hl: Arc<Mutex<HotLocation>>,
        /// What tracer is being used to record this trace? Needed for trace mapping.
        thread_tracer: Box<dyn TraceRecorder>,
        /// Records the content of data recorded via `yk_promote_*` and `yk_idempotent_promote_*`.
        promotions: Vec<u8>,
        /// Records the content of data recorded via `yk_debug_str`.
        debug_strs: Vec<String>,
        /// The `frameaddr` when tracing started. This allows us to tell if we're finishing tracing
        /// at the same point that we started.
        frameaddr: *mut c_void,
    },
    /// This thread is executing a trace. The `dyn CompiledTrace` allows another thread to tell
    /// whether the thread that started tracing a [Location] is still alive or not by inspecting
    /// its strong count (if the strong count is equal to 1 then the thread died while tracing).
    /// Note that this relies on thread local storage dropping the [MTThread] instance and (by
    /// implication) dropping the [Arc] and decrementing its strong count. Unfortunately, there is
    /// no guarantee that thread local storage will be dropped when a thread dies (and there is
    /// also significant platform variation in regard to dropping thread locals), so this mechanism
    /// can't be fully relied upon: however, we can't monitor thread death in any other reasonable
    /// way, so this will have to do.
    Executing {
        /// The root trace which started execution off. Note: the *actual* [CompiledTrace]
        /// currently executing might not be *this* [CompiledTrace] (e.g. it could be a sidetrace).
        /// However, whatever trace is executing will guarantee to have originated from the same
        /// [MT] instance.
        ctr: Arc<dyn CompiledTrace>,
    },
}

impl std::fmt::Debug for MTThreadState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Interpreting => write!(f, "Interpreting"),
            Self::Tracing { .. } => write!(f, "Tracing"),
            Self::Executing { .. } => write!(f, "Executing"),
        }
    }
}

/// Meta-tracer per-thread state. Note that this struct is neither `Send` nor `Sync`: it can only
/// be accessed from within a single thread.
pub struct MTThread {
    /// Where in the "interpreting/tracing/executing" is this thread? This `Vec` always has at
    /// least 1 element in it. It should not be access directly: use the `*_tstate` methods.
    tstate: Vec<MTThreadState>,
    // Raw pointers are neither send nor sync.
    _dont_send_or_sync_me: PhantomData<*mut ()>,
}

impl MTThread {
    fn new() -> Self {
        MTThread {
            tstate: vec![MTThreadState::Interpreting],
            _dont_send_or_sync_me: PhantomData,
        }
    }

    /// Call `f` with a `&` reference to this thread's [MTThread] instance.
    ///
    /// # Panics
    ///
    /// For the same reasons as [thread::local::LocalKey::with].
    pub(crate) fn with_borrow<F, R>(f: F) -> R
    where
        F: FnOnce(&MTThread) -> R,
    {
        THREAD_MTTHREAD.with_borrow(|mtt| f(mtt))
    }

    /// Call `f` with a `&mut` reference to this thread's [MTThread] instance.
    ///
    /// # Panics
    ///
    /// For the same reasons as [thread::local::LocalKey::with].
    pub(crate) fn with_borrow_mut<F, R>(f: F) -> R
    where
        F: FnOnce(&mut MTThread) -> R,
    {
        THREAD_MTTHREAD.with_borrow_mut(|mtt| f(mtt))
    }

    /// Increment the counter tracking how many times we've seen the control point since starting
    /// tracing.
    pub fn inc_cp_idx() {
        Self::with_borrow_mut(|mtt| {
            if let MTThreadState::Tracing { ref mut cp_idx, .. } = mtt.peek_mut_tstate() {
                *cp_idx += 1;
            }
        });
    }

    /// Is this thread currently tracing something?
    ///
    /// # Panics
    ///
    /// If the stack is empty. There should always be at least one element on the stack, so a panic
    /// here means that something has gone wrong elsewhere.
    pub(crate) fn is_tracing(&self) -> bool {
        self.tstate
            .iter()
            .any(|x| matches!(x, &MTThreadState::Tracing { .. }))
    }

    /// Return a reference to the [CompiledTrace] with ID `ctrid`.
    ///
    /// # Panics
    ///
    /// If the stack is empty. There should always be at least one element on the stack, so a panic
    /// here means that something has gone wrong elsewhere.
    pub(crate) fn running_trace(&self, ctrid: CompiledTraceId) -> Arc<dyn CompiledTrace> {
        for tstate in self.tstate.iter().rev() {
            if let MTThreadState::Executing { ctr } = tstate {
                return Arc::clone(&ctr.mt().as_ref().compiled_traces.lock()[&ctrid]);
            }
        }
        panic!();
    }

    /// Return a reference to the last element on the stack of [MTThreadState]s.
    ///
    /// # Panics
    ///
    /// If the stack is empty. There should always be at least one element on the stack, so a panic
    /// here means that something has gone wrong elsewhere.
    fn peek_tstate(&self) -> &MTThreadState {
        self.tstate.last().unwrap()
    }

    /// Return a mutable reference to the last element on the stack of [MTThreadState]s.
    ///
    /// # Panics
    ///
    /// If the stack is empty. There should always be at least one element on the stack, so a panic
    /// here means that something has gone wrong elsewhere.
    fn peek_mut_tstate(&mut self) -> &mut MTThreadState {
        self.tstate.last_mut().unwrap()
    }

    /// Pop the last element from the stack of [MTThreadState]s and return it.
    ///
    /// # Panics
    ///
    /// If the stack is empty. There should always be at least one element on the stack, so a panic
    /// here means that something has gone wrong elsewhere.
    fn pop_tstate(&mut self) -> MTThreadState {
        debug_assert!(self.tstate.len() > 1);
        self.tstate.pop().unwrap()
    }

    /// Push `tstate` to the end of the stack of [MTThreadState]s.
    fn push_tstate(&mut self, tstate: MTThreadState) {
        self.tstate.push(tstate);
    }

    /// Records `val` as a value to be promoted. Returns `true` if either: no trace is being
    /// recorded; or recording the promotion succeeded.
    ///
    /// If `false` is returned, the current trace is unable to record the promotion successfully
    /// and further calls are probably pointless, though they will not cause the tracer to enter
    /// undefined behaviour territory.
    ///
    /// # Panics
    ///
    /// If the stack is empty. There should always be at least one element on the stack, so a panic
    /// here means that something has gone wrong elsewhere.
    pub(crate) fn promote_i32(&mut self, val: i32) -> bool {
        if let MTThreadState::Tracing {
            ref mut promotions, ..
        } = self.peek_mut_tstate()
        {
            promotions.extend_from_slice(&val.to_ne_bytes());
        }
        true
    }

    /// Records `val` as a value to be promoted. Returns `true` if either: no trace is being
    /// recorded; or recording the promotion succeeded.
    ///
    /// If `false` is returned, the current trace is unable to record the promotion successfully
    /// and further calls are probably pointless, though they will not cause the tracer to enter
    /// undefined behaviour territory.
    ///
    /// # Panics
    ///
    /// If the stack is empty. There should always be at least one element on the stack, so a panic
    /// here means that something has gone wrong elsewhere.
    pub(crate) fn promote_u32(&mut self, val: u32) -> bool {
        if let MTThreadState::Tracing {
            ref mut promotions, ..
        } = self.peek_mut_tstate()
        {
            promotions.extend_from_slice(&val.to_ne_bytes());
        }
        true
    }

    /// Records `val` as a value to be promoted. Returns `true` if either: no trace is being
    /// recorded; or recording the promotion succeeded.
    ///
    /// If `false` is returned, the current trace is unable to record the promotion successfully
    /// and further calls are probably pointless, though they will not cause the tracer to enter
    /// undefined behaviour territory.
    ///
    /// # Panics
    ///
    /// If the stack is empty. There should always be at least one element on the stack, so a panic
    /// here means that something has gone wrong elsewhere.
    pub(crate) fn promote_i64(&mut self, val: i64) -> bool {
        if let MTThreadState::Tracing {
            ref mut promotions, ..
        } = self.peek_mut_tstate()
        {
            promotions.extend_from_slice(&val.to_ne_bytes());
        }
        true
    }

    /// Records `val` as a value to be promoted. Returns `true` if either: no trace is being
    /// recorded; or recording the promotion succeeded.
    ///
    /// If `false` is returned, the current trace is unable to record the promotion successfully
    /// and further calls are probably pointless, though they will not cause the tracer to enter
    /// undefined behaviour territory.
    ///
    /// # Panics
    ///
    /// If the stack is empty. There should always be at least one element on the stack, so a panic
    /// here means that something has gone wrong elsewhere.
    pub(crate) fn promote_usize(&mut self, val: usize) -> bool {
        if let MTThreadState::Tracing {
            ref mut promotions, ..
        } = self.peek_mut_tstate()
        {
            promotions.extend_from_slice(&val.to_ne_bytes());
        }
        true
    }

    /// Record a debug string.
    ///
    /// # Panics
    ///
    /// If the stack is empty. There should always be at least one element on the stack, so a panic
    /// here means that something has gone wrong elsewhere.
    pub(crate) fn insert_debug_str(&mut self, msg: String) -> bool {
        if let MTThreadState::Tracing {
            ref mut debug_strs, ..
        } = self.peek_mut_tstate()
        {
            debug_strs.push(msg);
        }
        true
    }
}

/// What action should a caller of [MT::transition_control_point] take?
#[derive(Debug)]
enum TransitionControlPoint {
    NoAction,
    AbortTracing(AbortKind),
    Execute(Arc<dyn CompiledTrace>),
    StartTracing(Arc<Mutex<HotLocation>>),
    StopTracing {
        /// The control point to start compiling from.
        ///
        ///  - 0 means compile the whole lot.
        ///  - >0 means we will be compiling an inner loop and the trace builder will have to skip
        ///    over some prefix of the trace, up until the inner loop starts.
        start_cp_idx: usize,
    },
    StopSideTracing {
        gidx: GuardIdx,
        parent_ctr: Arc<dyn CompiledTrace>,
        root_ctr: Arc<dyn CompiledTrace>,
    },
}

/// Why did we abort tracing?
#[derive(Debug)]
enum AbortKind {
    /// While tracing we fell back from an interpreter to a JIT frame.
    BackIntoExecution,
    /// While tracing, we encountered a compiled trace.
    EncounteredCompiledTrace,
    /// Tracing continued while the interpreter frame address changed.
    OutOfFrame,
    /// We discovered an inner loop during tracing (i.e. we traced a [Location] more than once) and
    /// we were unable to switch to tracing that loop.
    ///
    /// XXX: "Unrolled" probably isn't a good name any more.
    Unrolled,
}

impl std::fmt::Display for AbortKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match *self {
            AbortKind::BackIntoExecution => write!(f, "tracing continued into a JIT frame"),
            AbortKind::EncounteredCompiledTrace => write!(f, "encountered compiled trace"),
            AbortKind::OutOfFrame => write!(f, "tracing went outside of starting frame"),
            AbortKind::Unrolled => write!(f, "tracing unrolled a loop"),
        }
    }
}

/// What action should a caller of [MT::transition_guard_failure] take?
#[derive(Debug)]
pub(crate) enum TransitionGuardFailure {
    NoAction,
    StartSideTracing(Arc<Mutex<HotLocation>>),
}

/// The unique identifier of a compiled trace.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct CompiledTraceId(u64);

impl CompiledTraceId {
    /// Create a [CompiledTraceId] from a `u64`. This function should only be used by deopt
    /// modules, which have to take a value from a register.
    pub(crate) fn from_u64(ctrid: u64) -> Self {
        Self(ctrid)
    }

    /// Create a dummy [CompiledTraceId] for testing purposes. Note: duplicate IDs can, and
    /// probably will, be returned!
    #[cfg(test)]
    pub(crate) fn testing() -> Self {
        Self(0)
    }

    /// Return a `u64` which can later be turned back into a `CompiledTraceId`. This should only be
    /// used by code gen when creating guard/deopt code.
    pub(crate) fn as_u64(&self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for CompiledTraceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    extern crate test;
    use super::*;
    use crate::{
        compile::{CompiledTraceTestingBasicTransitions, CompiledTraceTestingMinimal},
        trace::TraceRecorderError,
    };
    use std::{assert_matches::assert_matches, hint::black_box, ptr};
    use test::bench::Bencher;

    // We only implement enough of the equality function for the tests we have.
    impl PartialEq for TransitionControlPoint {
        fn eq(&self, other: &Self) -> bool {
            match (self, other) {
                (TransitionControlPoint::NoAction, TransitionControlPoint::NoAction) => true,
                (TransitionControlPoint::Execute(p1), TransitionControlPoint::Execute(p2)) => {
                    std::ptr::eq(p1, p2)
                }
                (
                    TransitionControlPoint::StartTracing(_),
                    TransitionControlPoint::StartTracing(_),
                ) => true,
                (x, y) => todo!("{:?} {:?}", x, y),
            }
        }
    }

    #[derive(Debug)]
    struct DummyTraceRecorder;

    impl TraceRecorder for DummyTraceRecorder {
        fn stop(self: Box<Self>) -> Result<Box<dyn AOTTraceIterator>, TraceRecorderError> {
            todo!();
        }
    }

    fn expect_start_tracing(mt: &Arc<MT>, loc: &Location) {
        let TransitionControlPoint::StartTracing(hl) =
            mt.transition_control_point(loc, ptr::null_mut())
        else {
            panic!()
        };
        MTThread::with_borrow_mut(|mtt| {
            mtt.push_tstate(MTThreadState::Tracing {
                hl,
                thread_tracer: Box::new(DummyTraceRecorder),
                promotions: Vec::new(),
                debug_strs: Vec::new(),
                frameaddr: ptr::null_mut(),
                seen_hls: HashMap::new(),
                cp_idx: 0,
            });
        });
    }

    fn expect_stop_tracing(mt: &Arc<MT>, loc: &Location) {
        let TransitionControlPoint::StopTracing { .. } =
            mt.transition_control_point(loc, ptr::null_mut())
        else {
            panic!()
        };
        MTThread::with_borrow_mut(|mtt| {
            mtt.pop_tstate();
            mtt.push_tstate(MTThreadState::Interpreting);
        });
    }

    fn expect_start_side_tracing(mt: &Arc<MT>, ctr: Arc<dyn CompiledTrace>) {
        let TransitionGuardFailure::StartSideTracing(hl) =
            mt.transition_guard_failure(ctr, GuardIdx::from(0))
        else {
            panic!()
        };
        MTThread::with_borrow_mut(|mtt| {
            mtt.push_tstate(MTThreadState::Tracing {
                hl,
                thread_tracer: Box::new(DummyTraceRecorder),
                promotions: Vec::new(),
                debug_strs: Vec::new(),
                frameaddr: ptr::null_mut(),
                seen_hls: HashMap::new(),
                cp_idx: 0,
            });
        });
    }

    #[test]
    fn basic_transitions() {
        let hot_thrsh = 5;
        let mt = MT::new().unwrap();
        mt.set_hot_threshold(hot_thrsh);
        mt.set_sidetrace_threshold(1);
        let loc = Location::new();
        for i in 0..mt.hot_threshold() {
            assert_eq!(
                mt.transition_control_point(&loc, ptr::null_mut()),
                TransitionControlPoint::NoAction
            );
            assert_eq!(loc.count(), Some(i + 1));
        }
        expect_start_tracing(&mt, &loc);
        assert!(matches!(
            loc.hot_location().unwrap().lock().kind,
            HotLocationKind::Tracing
        ));
        expect_stop_tracing(&mt, &loc);
        assert!(matches!(
            loc.hot_location().unwrap().lock().kind,
            HotLocationKind::Compiling
        ));
        let ctr = Arc::new(CompiledTraceTestingBasicTransitions::new(Arc::downgrade(
            &loc.hot_location_arc_clone().unwrap(),
        )));
        loc.hot_location().unwrap().lock().kind = HotLocationKind::Compiled(ctr.clone());
        assert!(matches!(
            mt.transition_control_point(&loc, ptr::null_mut()),
            TransitionControlPoint::Execute(_)
        ));
        expect_start_side_tracing(&mt, ctr);

        match mt.transition_control_point(&loc, ptr::null_mut()) {
            TransitionControlPoint::StopSideTracing { .. } => {
                MTThread::with_borrow_mut(|mtt| {
                    mtt.pop_tstate();
                    mtt.push_tstate(MTThreadState::Interpreting);
                });
                assert!(matches!(
                    loc.hot_location().unwrap().lock().kind,
                    HotLocationKind::Compiled(_)
                ));
            }
            _ => unreachable!(),
        }
        assert!(matches!(
            mt.transition_control_point(&loc, ptr::null_mut()),
            TransitionControlPoint::Execute(_)
        ));
    }

    #[test]
    fn threaded_threshold() {
        // Aim for a situation where there's a lot of contention.
        let num_threads = u32::try_from(num_cpus::get() * 4).unwrap();
        let hot_thrsh = num_threads.saturating_mul(2500);
        let mt = Arc::new(MT::new().unwrap());
        mt.set_hot_threshold(hot_thrsh);
        let loc = Arc::new(Location::new());

        let mut thrs = vec![];
        for _ in 0..num_threads {
            let mt = Arc::clone(&mt);
            let loc = Arc::clone(&loc);
            let t = thread::spawn(move || {
                // The "*4" is the number of times we call `transition_location` in the loop: we
                // need to make sure that this loop cannot tip the Location over the threshold,
                // otherwise tracing will start, and the assertions will fail.
                for _ in 0..hot_thrsh / (num_threads * 4) {
                    assert_eq!(
                        mt.transition_control_point(&loc, ptr::null_mut()),
                        TransitionControlPoint::NoAction
                    );
                    let c1 = loc.count();
                    assert!(c1.is_some());
                    assert_eq!(
                        mt.transition_control_point(&loc, ptr::null_mut()),
                        TransitionControlPoint::NoAction
                    );
                    let c2 = loc.count();
                    assert!(c2.is_some());
                    assert_eq!(
                        mt.transition_control_point(&loc, ptr::null_mut()),
                        TransitionControlPoint::NoAction
                    );
                    let c3 = loc.count();
                    assert!(c3.is_some());
                    assert_eq!(
                        mt.transition_control_point(&loc, ptr::null_mut()),
                        TransitionControlPoint::NoAction
                    );
                    let c4 = loc.count();
                    assert!(c4.is_some());
                    assert!(c4.unwrap() >= c3.unwrap());
                    assert!(c3.unwrap() >= c2.unwrap());
                    assert!(c2.unwrap() >= c1.unwrap());
                }
            });
            thrs.push(t);
        }
        for t in thrs {
            t.join().unwrap();
        }
        // Thread contention and the use of `compare_exchange_weak` means that there is absolutely
        // no guarantee about what the location's count will be at this point other than it must be
        // at or below the threshold: it could even be (although it's rather unlikely) 0!
        assert!(loc.count().is_some());
        loop {
            match mt.transition_control_point(&loc, ptr::null_mut()) {
                TransitionControlPoint::NoAction => (),
                TransitionControlPoint::StartTracing(hl) => {
                    MTThread::with_borrow_mut(|mtt| {
                        mtt.push_tstate(MTThreadState::Tracing {
                            hl,
                            thread_tracer: Box::new(DummyTraceRecorder),
                            promotions: Vec::new(),
                            debug_strs: Vec::new(),
                            frameaddr: ptr::null_mut(),
                            seen_hls: HashMap::new(),
                            cp_idx: 0,
                        });
                    });
                    break;
                }
                _ => unreachable!(),
            }
        }
        expect_stop_tracing(&mt, &loc);
        // At this point, we have nothing to meaningfully test over the `basic_transitions` test.
    }

    #[test]
    fn locations_dont_get_stuck_tracing() {
        // If tracing a location fails too many times (e.g. because the thread terminates before
        // tracing is complete), the location must be marked as DontTrace.

        const THRESHOLD: HotThreshold = 5;
        let mt = Arc::new(MT::new().unwrap());
        mt.set_hot_threshold(THRESHOLD);
        let loc = Arc::new(Location::new());

        // Get the location to the point of being hot.
        for _ in 0..THRESHOLD {
            assert_eq!(
                mt.transition_control_point(&loc, ptr::null_mut()),
                TransitionControlPoint::NoAction
            );
        }

        // Start tracing in a thread and purposefully let the thread terminate before tracing is
        // complete.
        for i in 0..mt.trace_failure_threshold() + 1 {
            {
                let mt = Arc::clone(&mt);
                let loc = Arc::clone(&loc);
                thread::spawn(move || {
                    expect_start_tracing(&mt, &loc);
                })
                .join()
                .unwrap();
            }
            assert!(matches!(
                loc.hot_location().unwrap().lock().kind,
                HotLocationKind::Tracing
            ));
            assert_eq!(
                loc.hot_location().unwrap().lock().tracecompilation_errors,
                i
            );
        }

        assert!(matches!(
            loc.hot_location().unwrap().lock().kind,
            HotLocationKind::Tracing
        ));
        assert_eq!(
            mt.transition_control_point(&loc, ptr::null_mut()),
            TransitionControlPoint::NoAction
        );
        assert!(matches!(
            loc.hot_location().unwrap().lock().kind,
            HotLocationKind::DontTrace
        ));
    }

    #[test]
    fn locations_can_fail_tracing_before_succeeding() {
        // Test that a location can fail tracing multiple times before being successfully traced.

        const THRESHOLD: HotThreshold = 5;
        let mt = Arc::new(MT::new().unwrap());
        mt.set_hot_threshold(THRESHOLD);
        let loc = Arc::new(Location::new());

        // Get the location to the point of being hot.
        for _ in 0..THRESHOLD {
            assert_eq!(
                mt.transition_control_point(&loc, ptr::null_mut()),
                TransitionControlPoint::NoAction
            );
        }

        // Start tracing in a thread and purposefully let the thread terminate before tracing is
        // complete.
        for i in 0..mt.trace_failure_threshold() {
            {
                let mt = Arc::clone(&mt);
                let loc = Arc::clone(&loc);
                thread::spawn(move || expect_start_tracing(&mt, &loc))
                    .join()
                    .unwrap();
            }
            assert!(matches!(
                loc.hot_location().unwrap().lock().kind,
                HotLocationKind::Tracing
            ));
            assert_eq!(
                loc.hot_location().unwrap().lock().tracecompilation_errors,
                i
            );
        }

        assert!(matches!(
            loc.hot_location().unwrap().lock().kind,
            HotLocationKind::Tracing
        ));
        // Start tracing again...
        expect_start_tracing(&mt, &loc);
        assert!(matches!(
            loc.hot_location().unwrap().lock().kind,
            HotLocationKind::Tracing
        ));
        // ...and this time let tracing succeed.
        expect_stop_tracing(&mt, &loc);
        // If tracing succeeded, we'll now be in the Compiling state.
        assert!(matches!(
            loc.hot_location().unwrap().lock().kind,
            HotLocationKind::Compiling
        ));
    }

    #[test]
    fn locations_can_fail_multiple_times() {
        // Test that a location can fail tracing/compiling multiple times before we give up.

        let hot_thrsh = 5;
        let mt = MT::new().unwrap();
        mt.set_hot_threshold(hot_thrsh);
        let loc = Location::new();
        for i in 0..mt.hot_threshold() {
            assert_eq!(
                mt.transition_control_point(&loc, ptr::null_mut()),
                TransitionControlPoint::NoAction
            );
            assert_eq!(loc.count(), Some(i + 1));
        }
        expect_start_tracing(&mt, &loc);
        expect_stop_tracing(&mt, &loc);

        for _ in 0..mt.trace_failure_threshold() {
            assert_matches!(
                loc.hot_location()
                    .unwrap()
                    .lock()
                    .tracecompilation_error(&mt),
                TraceFailed::KeepTrying
            );
        }
        assert_matches!(
            loc.hot_location()
                .unwrap()
                .lock()
                .tracecompilation_error(&mt),
            TraceFailed::DontTrace
        );
    }

    #[test]
    fn dont_trace_two_locations_simultaneously_in_one_thread() {
        // A thread can only trace one Location at a time: if, having started tracing, it
        // encounters another Location which has reached its hot threshold, it just ignores it.
        // Once the first location is compiled, the second location can then be compiled.

        const THRESHOLD: HotThreshold = 5;
        let mt = MT::new().unwrap();
        mt.set_hot_threshold(THRESHOLD);
        let loc1 = Location::new();
        let loc2 = Location::new();

        for _ in 0..THRESHOLD {
            assert_eq!(
                mt.transition_control_point(&loc1, ptr::null_mut()),
                TransitionControlPoint::NoAction
            );
            assert_eq!(
                mt.transition_control_point(&loc2, ptr::null_mut()),
                TransitionControlPoint::NoAction
            );
        }
        expect_start_tracing(&mt, &loc1);
        assert_eq!(
            mt.transition_control_point(&loc2, ptr::null_mut()),
            TransitionControlPoint::NoAction
        );
        assert!(matches!(
            loc1.hot_location().unwrap().lock().kind,
            HotLocationKind::Tracing
        ));
        assert_eq!(loc2.count(), None);
        assert_matches!(
            loc2.hot_location().unwrap().lock().kind,
            HotLocationKind::Counting(6)
        );
        expect_stop_tracing(&mt, &loc1);
        assert!(matches!(
            loc1.hot_location().unwrap().lock().kind,
            HotLocationKind::Compiling
        ));
        expect_start_tracing(&mt, &loc2);
        expect_stop_tracing(&mt, &loc2);
    }

    #[test]
    fn only_one_thread_starts_tracing() {
        // If multiple threads hammer away at a location, only one of them can win the race to
        // trace it (and then compile it etc.).

        // We need to set a high enough threshold that the threads are likely to meaningfully
        // interleave when interacting with the location.
        const THRESHOLD: HotThreshold = 100000;
        let mt = Arc::new(MT::new().unwrap());
        mt.set_hot_threshold(THRESHOLD);
        let loc = Arc::new(Location::new());

        let mut thrs = Vec::new();
        let num_starts = Arc::new(AtomicU64::new(0));
        for _ in 0..num_cpus::get() {
            let loc = Arc::clone(&loc);
            let mt = Arc::clone(&mt);
            let num_starts = Arc::clone(&num_starts);
            thrs.push(thread::spawn(move || {
                for _ in 0..THRESHOLD {
                    match mt.transition_control_point(&loc, ptr::null_mut()) {
                        TransitionControlPoint::NoAction => (),
                        TransitionControlPoint::AbortTracing(_) => panic!(),
                        TransitionControlPoint::Execute(_) => (),
                        TransitionControlPoint::StartTracing(hl) => {
                            num_starts.fetch_add(1, Ordering::Relaxed);
                            MTThread::with_borrow_mut(|mtt| {
                                mtt.push_tstate(MTThreadState::Tracing {
                                    hl,
                                    thread_tracer: Box::new(DummyTraceRecorder),
                                    promotions: Vec::new(),
                                    debug_strs: Vec::new(),
                                    frameaddr: ptr::null_mut(),
                                    seen_hls: HashMap::new(),
                                    cp_idx: 0,
                                });
                            });
                            assert!(matches!(
                                loc.hot_location().unwrap().lock().kind,
                                HotLocationKind::Tracing
                            ));
                            expect_stop_tracing(&mt, &loc);
                            assert!(matches!(
                                loc.hot_location().unwrap().lock().kind,
                                HotLocationKind::Compiling
                            ));
                            assert_eq!(
                                mt.transition_control_point(&loc, ptr::null_mut()),
                                TransitionControlPoint::NoAction
                            );
                            assert!(matches!(
                                loc.hot_location().unwrap().lock().kind,
                                HotLocationKind::Compiling
                            ));
                            loc.hot_location().unwrap().lock().kind = HotLocationKind::Compiled(
                                Arc::new(CompiledTraceTestingMinimal::new()),
                            );
                            loop {
                                if let TransitionControlPoint::Execute(_) =
                                    mt.transition_control_point(&loc, ptr::null_mut())
                                {
                                    break;
                                }
                            }
                            break;
                        }
                        TransitionControlPoint::StopTracing { .. }
                        | TransitionControlPoint::StopSideTracing { .. } => unreachable!(),
                    }
                }
            }));
        }

        for t in thrs {
            t.join().unwrap();
        }

        assert_eq!(num_starts.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn two_tracing_threads_must_not_stop_each_others_tracing_location() {
        // A tracing thread can only stop tracing when it encounters the specific Location that
        // caused it to start tracing. If it encounters another Location that also happens to be
        // tracing, it must ignore it.

        const THRESHOLD: HotThreshold = 5;
        let mt = Arc::new(MT::new().unwrap());
        mt.set_hot_threshold(THRESHOLD);
        let loc1 = Arc::new(Location::new());
        let loc2 = Location::new();

        for _ in 0..THRESHOLD {
            assert_eq!(
                mt.transition_control_point(&loc1, ptr::null_mut()),
                TransitionControlPoint::NoAction
            );
            assert_eq!(
                mt.transition_control_point(&loc2, ptr::null_mut()),
                TransitionControlPoint::NoAction
            );
        }

        {
            let mt = Arc::clone(&mt);
            let loc1 = Arc::clone(&loc1);
            thread::spawn(move || expect_start_tracing(&mt, &loc1))
                .join()
                .unwrap();
        }

        expect_start_tracing(&mt, &loc2);
        assert_eq!(
            mt.transition_control_point(&loc1, ptr::null_mut()),
            TransitionControlPoint::NoAction
        );
        expect_stop_tracing(&mt, &loc2);
    }

    #[test]
    fn two_sidetracing_threads_must_not_stop_each_others_tracing_location() {
        // A side-tracing thread can only stop tracing when it encounters the specific Location
        // that caused it to start tracing. If it encounters another Location that also happens to
        // be tracing, it must ignore it.

        const THRESHOLD: HotThreshold = 5;
        let mt = MT::new().unwrap();
        mt.set_hot_threshold(THRESHOLD);
        mt.set_sidetrace_threshold(1);
        let loc1 = Arc::new(Location::new());
        let loc2 = Location::new();

        fn to_compiled(mt: &Arc<MT>, loc: &Location) -> Arc<dyn CompiledTrace> {
            for _ in 0..THRESHOLD {
                assert_eq!(
                    mt.transition_control_point(loc, ptr::null_mut()),
                    TransitionControlPoint::NoAction
                );
            }

            expect_start_tracing(mt, loc);
            assert!(matches!(
                loc.hot_location().unwrap().lock().kind,
                HotLocationKind::Tracing
            ));
            expect_stop_tracing(mt, loc);
            assert!(matches!(
                loc.hot_location().unwrap().lock().kind,
                HotLocationKind::Compiling
            ));
            let ctr = Arc::new(CompiledTraceTestingBasicTransitions::new(Arc::downgrade(
                &loc.hot_location_arc_clone().unwrap(),
            )));
            loc.hot_location().unwrap().lock().kind = HotLocationKind::Compiled(ctr.clone());
            ctr
        }

        let ctr1 = to_compiled(&mt, &loc1);
        let ctr2 = to_compiled(&mt, &loc2);

        {
            let mt = Arc::clone(&mt);
            thread::spawn(move || expect_start_side_tracing(&mt, ctr1))
                .join()
                .unwrap();
        }

        expect_start_side_tracing(&mt, ctr2);
        assert!(matches!(
            dbg!(mt.transition_control_point(&loc1, ptr::null_mut())),
            TransitionControlPoint::NoAction
        ));
        assert!(matches!(
            mt.transition_control_point(&loc2, ptr::null_mut()),
            TransitionControlPoint::StopSideTracing { .. }
        ));
    }

    #[bench]
    fn bench_single_threaded_control_point(b: &mut Bencher) {
        let mt = MT::new().unwrap();
        let loc = Location::new();
        b.iter(|| {
            for _ in 0..100000 {
                black_box(mt.transition_control_point(&loc, ptr::null_mut()));
            }
        });
    }

    #[bench]
    fn bench_multi_threaded_control_point(b: &mut Bencher) {
        let mt = Arc::new(MT::new().unwrap());
        let loc = Arc::new(Location::new());
        b.iter(|| {
            let mut thrs = vec![];
            for _ in 0..4 {
                let loc = Arc::clone(&loc);
                let mt = Arc::clone(&mt);
                thrs.push(thread::spawn(move || {
                    for _ in 0..100 {
                        black_box(mt.transition_control_point(&loc, ptr::null_mut()));
                    }
                }));
            }
            for t in thrs {
                t.join().unwrap();
            }
        });
    }

    #[test]
    fn traces_can_be_executed_during_tracing() {
        let mt = Arc::new(MT::new().unwrap());
        mt.set_hot_threshold(0);
        let loc1 = Location::new();
        let loc2 = Location::new();

        // Get `loc1` to the point where there's a compiled trace for it.
        expect_start_tracing(&mt, &loc1);
        expect_stop_tracing(&mt, &loc1);
        loc1.hot_location().unwrap().lock().kind =
            HotLocationKind::Compiled(Arc::new(CompiledTraceTestingMinimal::new()));

        expect_start_tracing(&mt, &loc2);
        assert_matches!(
            mt.transition_control_point(&loc1, ptr::null_mut()),
            TransitionControlPoint::AbortTracing(AbortKind::EncounteredCompiledTrace)
        );

        expect_stop_tracing(&mt, &loc2);
        assert_matches!(
            mt.transition_control_point(&loc1, ptr::null_mut()),
            TransitionControlPoint::Execute(_)
        );
    }
}

use crate::slots::OpenJDKSlot;
use crate::OpenJDK;
use crate::OpenJDK_Upcalls;
use crate::BUILDER;
use crate::UPCALLS;
use libc::c_char;
use mmtk::memory_manager;
use mmtk::plan::{BarrierSelector, Compressor};
use mmtk::scheduler::GCWorker;
use mmtk::util::alloc::AllocatorSelector;
use mmtk::util::api_util::NullableObjectReference;
use mmtk::util::opaque_pointer::*;
use mmtk::util::{Address, ObjectReference};
use mmtk::vm::{slot::Slot, Scanning};
use mmtk::AllocationSemantics;
use mmtk::Mutator;
use mmtk::MutatorContext;
use once_cell::sync;
use std::cell::RefCell;
use std::ffi::{CStr, CString};
use std::sync::atomic::{AtomicI32, Ordering};

macro_rules! with_singleton {
    (|$x: ident| $($expr:tt)*) => {
        if crate::use_compressed_oops() {
            let $x: &'static mmtk::MMTK<crate::OpenJDK<true>> = &*crate::SINGLETON_COMPRESSED;
            $($expr)*
        } else {
            let $x: &'static mmtk::MMTK<crate::OpenJDK<false>> = &*crate::SINGLETON_UNCOMPRESSED;
            $($expr)*
        }
    };
}

macro_rules! with_mutator {
    (|$x: ident| $($expr:tt)*) => {
        if crate::use_compressed_oops() {
            let $x = unsafe { &mut *($x as *mut Mutator<OpenJDK<true>>) };
            $($expr)*
        } else {
            let $x = unsafe { &mut *($x as *mut Mutator<OpenJDK<false>>) };
            $($expr)*
        }
    };
}

// Supported barriers:
static NO_BARRIER: sync::Lazy<CString> = sync::Lazy::new(|| CString::new("NoBarrier").unwrap());
static OBJECT_BARRIER: sync::Lazy<CString> =
    sync::Lazy::new(|| CString::new("ObjectBarrier").unwrap());
static SATB_BARRIER: sync::Lazy<CString> = sync::Lazy::new(|| CString::new("SATBBarrier").unwrap());

static PLAN_NOGC: sync::Lazy<CString> = sync::Lazy::new(|| CString::new("NoGC").unwrap());
static PLAN_SEMISPACE: sync::Lazy<CString> =
    sync::Lazy::new(|| CString::new("SemiSpace").unwrap());
static PLAN_GENCOPY: sync::Lazy<CString> = sync::Lazy::new(|| CString::new("GenCopy").unwrap());
static PLAN_GENIMMIX: sync::Lazy<CString> = sync::Lazy::new(|| CString::new("GenImmix").unwrap());
static PLAN_MARKSWEEP: sync::Lazy<CString> =
    sync::Lazy::new(|| CString::new("MarkSweep").unwrap());
static PLAN_PAGEPROTECT: sync::Lazy<CString> =
    sync::Lazy::new(|| CString::new("PageProtect").unwrap());
static PLAN_IMMIX: sync::Lazy<CString> = sync::Lazy::new(|| CString::new("Immix").unwrap());
static PLAN_MARKCOMPACT: sync::Lazy<CString> =
    sync::Lazy::new(|| CString::new("MarkCompact").unwrap());
static PLAN_COMPRESSOR: sync::Lazy<CString> =
    sync::Lazy::new(|| CString::new("Compressor").unwrap());
static PLAN_STICKYIMMIX: sync::Lazy<CString> =
    sync::Lazy::new(|| CString::new("StickyImmix").unwrap());
static PLAN_CONCURRENTIMMIX: sync::Lazy<CString> =
    sync::Lazy::new(|| CString::new("ConcurrentImmix").unwrap());

#[no_mangle]
pub extern "C" fn get_mmtk_version() -> *const c_char {
    crate::build_info::MMTK_OPENJDK_FULL_VERSION.as_ptr() as _
}

#[no_mangle]
pub extern "C" fn mmtk_active_barrier() -> *const c_char {
    with_singleton!(|singleton| {
        match singleton.get_plan().constraints().barrier {
            BarrierSelector::NoBarrier => NO_BARRIER.as_ptr(),
            BarrierSelector::ObjectBarrier => OBJECT_BARRIER.as_ptr(),
            BarrierSelector::SATBBarrier => SATB_BARRIER.as_ptr(),
            // In case we have more barriers in mmtk-core.
            #[allow(unreachable_patterns)]
            _ => unimplemented!(),
        }
    })
}

#[no_mangle]
pub extern "C" fn mmtk_active_plan() -> *const c_char {
    with_singleton!(|singleton| {
        use mmtk::util::options::PlanSelector;
        match *singleton.get_options().plan {
            PlanSelector::NoGC => PLAN_NOGC.as_ptr(),
            PlanSelector::SemiSpace => PLAN_SEMISPACE.as_ptr(),
            PlanSelector::GenCopy => PLAN_GENCOPY.as_ptr(),
            PlanSelector::GenImmix => PLAN_GENIMMIX.as_ptr(),
            PlanSelector::MarkSweep => PLAN_MARKSWEEP.as_ptr(),
            PlanSelector::PageProtect => PLAN_PAGEPROTECT.as_ptr(),
            PlanSelector::Immix => PLAN_IMMIX.as_ptr(),
            PlanSelector::MarkCompact => PLAN_MARKCOMPACT.as_ptr(),
            PlanSelector::Compressor => PLAN_COMPRESSOR.as_ptr(),
            PlanSelector::StickyImmix => PLAN_STICKYIMMIX.as_ptr(),
            PlanSelector::ConcurrentImmix => PLAN_CONCURRENTIMMIX.as_ptr(),
        }
    })
}

/// # Safety
/// Caller needs to make sure the ptr is a valid vector pointer.
#[no_mangle]
pub unsafe extern "C" fn release_buffer(ptr: *mut Address, length: usize, capacity: usize) {
    let _vec = Vec::<Address>::from_raw_parts(ptr, length, capacity);
}

#[no_mangle]
pub extern "C" fn openjdk_gc_init(calls: *const OpenJDK_Upcalls) {
    unsafe { UPCALLS = calls };
    crate::abi::validate_memory_layouts();

    // We don't really need this, as we can dynamically set plans. However, for compatability of our CI scripts,
    // we allow selecting a plan using feature at build time.
    // We should be able to remove this very soon.
    {
        use mmtk::util::options::PlanSelector;
        let force_plan = if cfg!(feature = "nogc") {
            Some(PlanSelector::NoGC)
        } else if cfg!(feature = "semispace") {
            Some(PlanSelector::SemiSpace)
        } else if cfg!(feature = "gencopy") {
            Some(PlanSelector::GenCopy)
        } else if cfg!(feature = "marksweep") {
            Some(PlanSelector::MarkSweep)
        } else if cfg!(feature = "markcompact") {
            Some(PlanSelector::MarkCompact)
        } else if cfg!(feature = "pageprotect") {
            Some(PlanSelector::PageProtect)
        } else if cfg!(feature = "immix") {
            Some(PlanSelector::Immix)
        } else if cfg!(feature = "genimmix") {
            Some(PlanSelector::GenImmix)
        } else if cfg!(feature = "stickyimmix") {
            Some(PlanSelector::StickyImmix)
        } else if cfg!(feature = "compressor") {
            Some(PlanSelector::Compressor)
        } else {
            None
        };
        if let Some(plan) = force_plan {
            BUILDER.lock().unwrap().options.plan.set(plan);
        }
    }

    // Make sure that we haven't initialized MMTk (by accident) yet
    assert!(!crate::MMTK_INITIALIZED.load(Ordering::SeqCst));
    // Make sure we initialize MMTk here
    if crate::use_compressed_oops() {
        lazy_static::initialize(&crate::SINGLETON_COMPRESSED);
    } else {
        lazy_static::initialize(&crate::SINGLETON_UNCOMPRESSED);
    }
}

#[no_mangle]
pub extern "C" fn openjdk_is_gc_initialized() -> bool {
    crate::MMTK_INITIALIZED.load(std::sync::atomic::Ordering::SeqCst)
}

#[no_mangle]
pub extern "C" fn mmtk_set_heap_size(min: usize, max: usize) -> bool {
    use mmtk::util::options::GCTriggerSelector;
    let mut builder = BUILDER.lock().unwrap();
    let policy = if min == max {
        GCTriggerSelector::FixedHeapSize(min)
    } else {
        GCTriggerSelector::DynamicHeapSize(min, max)
    };
    builder.options.gc_trigger.set(policy)
}

#[no_mangle]
pub extern "C" fn bind_mutator(tls: VMMutatorThread) -> *mut libc::c_void {
    with_singleton!(|singleton| {
        Box::into_raw(memory_manager::bind_mutator(singleton, tls)) as *mut libc::c_void
    })
}

#[no_mangle]
// It is fine we turn the pointer back to box, as we turned a boxed value to the raw pointer in bind_mutator()
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn destroy_mutator(mutator: *mut libc::c_void) {
    with_mutator!(|mutator| memory_manager::destroy_mutator(mutator))
}

#[no_mangle]
// We trust the mutator pointer is valid.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn flush_mutator(mutator: *mut libc::c_void) {
    with_mutator!(|mutator| memory_manager::flush_mutator(mutator))
}

#[no_mangle]
// We trust the mutator pointer is valid.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn alloc(
    mutator: *mut libc::c_void,
    size: usize,
    align: usize,
    offset: usize,
    allocator: AllocationSemantics,
) -> Address {
    with_mutator!(|mutator| memory_manager::alloc(mutator, size, align, offset, allocator))
}

#[no_mangle]
pub extern "C" fn get_allocator_mapping(allocator: AllocationSemantics) -> AllocatorSelector {
    with_singleton!(|singleton| memory_manager::get_allocator_mapping(singleton, allocator))
}

#[no_mangle]
pub extern "C" fn get_max_non_los_default_alloc_bytes() -> usize {
    with_singleton!(|singleton| {
        singleton
            .get_plan()
            .constraints()
            .max_non_los_default_alloc_bytes
    })
}

#[no_mangle]
// We trust the mutator pointer is valid.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn post_alloc(
    mutator: *mut libc::c_void,
    refer: ObjectReference,
    bytes: usize,
    allocator: AllocationSemantics,
) {
    with_mutator!(|mutator| memory_manager::post_alloc(mutator, refer, bytes, allocator))
}

#[no_mangle]
// We trust the mutator pointer is valid.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn post_alloc_initialized(_mutator: *mut libc::c_void, refer: ObjectReference) {
    with_singleton!(|singleton| singleton.get_plan().post_alloc_initialized(refer))
}

#[no_mangle]
pub extern "C" fn will_never_move(object: ObjectReference) -> bool {
    !object.is_movable()
}

#[no_mangle]
// We trust the worker pointer is valid.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn start_worker(tls: VMWorkerThread, worker: *mut libc::c_void) {
    if crate::use_compressed_oops() {
        let worker = unsafe { Box::from_raw(worker as *mut GCWorker<OpenJDK<true>>) };
        memory_manager::start_worker::<OpenJDK<true>>(crate::singleton::<true>(), tls, worker)
    } else {
        let worker = unsafe { Box::from_raw(worker as *mut GCWorker<OpenJDK<false>>) };
        memory_manager::start_worker::<OpenJDK<false>>(crate::singleton::<false>(), tls, worker)
    }
}

#[no_mangle]
pub extern "C" fn initialize_collection(tls: VMThread) {
    with_singleton!(|singleton| memory_manager::initialize_collection(singleton, tls))
}

#[no_mangle]
pub extern "C" fn used_bytes() -> usize {
    with_singleton!(|singleton| memory_manager::used_bytes(singleton))
}

#[no_mangle]
pub extern "C" fn free_bytes() -> usize {
    with_singleton!(|singleton| memory_manager::free_bytes(singleton))
}

#[no_mangle]
pub extern "C" fn total_bytes() -> usize {
    with_singleton!(|singleton| memory_manager::total_bytes(singleton))
}

#[no_mangle]
pub extern "C" fn handle_user_collection_request(tls: VMMutatorThread) {
    with_singleton!(|singleton| {
        memory_manager::handle_user_collection_request(singleton, tls);
    })
}

#[no_mangle]
pub extern "C" fn mmtk_enable_compressed_oops() {
    crate::slots::enable_compressed_oops()
}

#[no_mangle]
pub extern "C" fn mmtk_set_compressed_klass_base_and_shift(base: Address, shift: usize) {
    crate::abi::set_compressed_klass_base_and_shift(base, shift)
}

#[no_mangle]
pub extern "C" fn is_in_mmtk_spaces(object: ObjectReference) -> bool {
    memory_manager::is_in_mmtk_spaces(object)
}

#[no_mangle]
pub extern "C" fn is_mapped_address(addr: Address) -> bool {
    memory_manager::is_mapped_address(addr)
}

#[no_mangle]
pub extern "C" fn add_weak_candidate(reff: ObjectReference) {
    with_singleton!(|singleton| memory_manager::add_weak_candidate(singleton, reff))
}

#[no_mangle]
pub extern "C" fn add_soft_candidate(reff: ObjectReference) {
    with_singleton!(|singleton| memory_manager::add_soft_candidate(singleton, reff))
}

#[no_mangle]
pub extern "C" fn add_phantom_candidate(reff: ObjectReference) {
    with_singleton!(|singleton| memory_manager::add_phantom_candidate(singleton, reff))
}

// The harness_begin()/end() functions are different than other API functions in terms of the thread state.
// Other functions are called by the VM, thus the thread should already be in the VM state. But the harness
// functions are called by the probe, and the thread is in JNI/application/native state. Thus we need call
// into VM to switch the thread state and VM will then call into mmtk-core again to do the actual work of
// harness_begin() and harness_end()

#[no_mangle]
pub extern "C" fn harness_begin(_id: usize) {
    unsafe { ((*UPCALLS).harness_begin)() };
}

#[no_mangle]
pub extern "C" fn mmtk_harness_begin_impl() {
    // Pass null as tls, OpenJDK binding does not rely on the tls value to block the current thread and do a GC
    with_singleton!(|singleton| {
        memory_manager::harness_begin(singleton, VMMutatorThread(VMThread::UNINITIALIZED));
    })
}

#[no_mangle]
pub extern "C" fn harness_end(_id: usize) {
    unsafe { ((*UPCALLS).harness_end)() };
}

#[no_mangle]
pub extern "C" fn mmtk_harness_end_impl() {
    with_singleton!(|singleton| memory_manager::harness_end(singleton))
}

#[no_mangle]
// We trust the name/value pointer is valid.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn process(name: *const c_char, value: *const c_char) -> bool {
    let name_str: &CStr = unsafe { CStr::from_ptr(name) };
    let value_str: &CStr = unsafe { CStr::from_ptr(value) };
    let mut builder = BUILDER.lock().unwrap();
    memory_manager::process(
        &mut builder,
        name_str.to_str().unwrap(),
        value_str.to_str().unwrap(),
    )
}

#[no_mangle]
pub extern "C" fn mmtk_builder_read_env_var_settings() {
    let mut builder = BUILDER.lock().unwrap();
    builder.options.read_env_var_settings();
}

/// Pass hotspot `ParallelGCThreads` flag to mmtk
#[no_mangle]
pub extern "C" fn mmtk_builder_set_threads(value: usize) {
    let mut builder = BUILDER.lock().unwrap();
    builder.options.threads.set(value);
}

/// Pass hotspot `UseTransparentHugePages` flag to mmtk
#[no_mangle]
pub extern "C" fn mmtk_builder_set_transparent_hugepages(value: bool) {
    let mut builder = BUILDER.lock().unwrap();
    builder.options.transparent_hugepages.set(value);
}

#[no_mangle]
// We trust the name/value pointer is valid.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn process_bulk(options: *const c_char) -> bool {
    let options_str: &CStr = unsafe { CStr::from_ptr(options) };
    let mut builder = BUILDER.lock().unwrap();
    memory_manager::process_bulk(&mut builder, options_str.to_str().unwrap())
}

#[no_mangle]
pub extern "C" fn mmtk_narrow_oop_base() -> Address {
    debug_assert!(crate::use_compressed_oops());
    crate::slots::BASE.load(Ordering::Relaxed)
}

#[no_mangle]
pub extern "C" fn mmtk_narrow_oop_shift() -> usize {
    debug_assert!(crate::use_compressed_oops());
    crate::slots::SHIFT.load(Ordering::Relaxed)
}

#[no_mangle]
pub extern "C" fn starting_heap_address() -> Address {
    memory_manager::starting_heap_address()
}

#[no_mangle]
pub extern "C" fn last_heap_address() -> Address {
    memory_manager::last_heap_address()
}

#[no_mangle]
pub extern "C" fn openjdk_max_capacity() -> usize {
    with_singleton!(|singleton| memory_manager::total_bytes(singleton))
}

#[no_mangle]
pub extern "C" fn executable() -> bool {
    true
}

#[no_mangle]
pub extern "C" fn mmtk_load_reference(mutator: *mut libc::c_void, o: ObjectReference) {
    with_mutator!(|mutator| mutator.barrier().load_weak_reference(o))
}

fn trace_stale_uffd_write<const COMPRESSED: bool>(
    src: ObjectReference,
    slot: Address,
    target: NullableObjectReference,
) {
    static TRACE_BUDGET: AtomicI32 = AtomicI32::new(-1);

    let Some(target) = Option::<ObjectReference>::from(target) else {
        return;
    };
    let singleton = crate::singleton::<COMPRESSED>();
    let Some(plan) = singleton
        .get_plan()
        .downcast_ref::<Compressor<OpenJDK<COMPRESSED>>>()
    else {
        return;
    };
    if !plan.is_uffd_compaction_active() {
        return;
    }
    if !plan
        .compressor_space
        .debug_is_stale_compaction_source_ref(target)
    {
        return;
    }
    let budget = TRACE_BUDGET.load(Ordering::Relaxed);
    if budget < 0 {
        let configured_budget = std::env::var("MMTK_TRACE_UFFD_STALE_WRITES_BUDGET")
            .ok()
            .and_then(|value| value.parse::<i32>().ok())
            .unwrap_or(64);
        let _ = TRACE_BUDGET.compare_exchange(
            budget,
            configured_budget,
            Ordering::Relaxed,
            Ordering::Relaxed,
        );
    }

    let slot = OpenJDKSlot::<COMPRESSED>::from(slot);
    let old_value = slot.load();
    let slot_desc = <OpenJDK<COMPRESSED> as mmtk::vm::VMBinding>::VMScanning::describe_slot(
        src, slot,
    )
    .unwrap_or_else(|| format!("slot_addr=0x{:x}", slot.addr.as_usize()));
    let src_state = plan.compressor_space.debug_describe_compaction_object(src);
    let target_state = plan.compressor_space.debug_describe_compaction_object(target);
    let old_state = old_value.map(|value| plan.compressor_space.debug_describe_compaction_object(value));
    let src_dbg = format!("{:?}", crate::abi::Oop::from(src));
    let target_dbg = format!("{:?}", crate::abi::Oop::from(target));
    let old_dbg = old_value.map(|value| format!("{:?}", crate::abi::Oop::from(value)));
    let describe_role = |obj: ObjectReference| unsafe {
        let ptr = ((*UPCALLS).describe_object_role)(obj);
        if ptr.is_null() {
            None
        } else {
            Some(CStr::from_ptr(ptr).to_string_lossy().into_owned())
        }
    };
    let src_vm_role = describe_role(src);
    let target_vm_role = describe_role(target);
    let old_vm_role = old_value.and_then(describe_role);

    let trace_src = std::env::var("MMTK_TRACE_UFFD_WRITE_SRC_DBG_SUBSTR").ok();
    let trace_src_role = std::env::var("MMTK_TRACE_UFFD_WRITE_SRC_ROLE_SUBSTR").ok();
    let trace_new = std::env::var("MMTK_TRACE_UFFD_WRITE_NEW_DBG_SUBSTR").ok();
    let trace_new_role = std::env::var("MMTK_TRACE_UFFD_WRITE_NEW_ROLE_SUBSTR").ok();
    let trace_old_role = std::env::var("MMTK_TRACE_UFFD_WRITE_OLD_ROLE_SUBSTR").ok();
    let trace_slot = std::env::var("MMTK_TRACE_UFFD_WRITE_SLOT_SUBSTR").ok();
    let trace_require_all = std::env::var("MMTK_TRACE_UFFD_WRITE_REQUIRE_ALL")
        .ok()
        .is_some_and(|value| value != "0");
    let trace_src_matches = trace_src
        .as_ref()
        .is_none_or(|needle| src_dbg.contains(needle));
    let trace_src_role_matches = trace_src_role.as_ref().is_none_or(|needle| {
        src_vm_role
            .as_ref()
            .is_some_and(|role| role.contains(needle))
    });
    let trace_new_matches = trace_new
        .as_ref()
        .is_none_or(|needle| target_dbg.contains(needle));
    let trace_new_role_matches = trace_new_role.as_ref().is_none_or(|needle| {
        target_vm_role
            .as_ref()
            .is_some_and(|role| role.contains(needle))
    });
    let trace_old_role_matches = trace_old_role.as_ref().is_none_or(|needle| {
        old_vm_role
            .as_ref()
            .is_some_and(|role| role.contains(needle))
    });
    let trace_slot_matches = trace_slot
        .as_ref()
        .is_none_or(|needle| slot_desc.contains(needle));
    let any_trace_filter_set = trace_src.is_some()
        || trace_src_role.is_some()
        || trace_new.is_some()
        || trace_new_role.is_some()
        || trace_old_role.is_some()
        || trace_slot.is_some();
    let should_trace = if trace_require_all {
        any_trace_filter_set
            && trace_src_matches
            && trace_src_role_matches
            && trace_new_matches
            && trace_new_role_matches
            && trace_old_role_matches
            && trace_slot_matches
    } else if any_trace_filter_set {
        trace_src
            .as_ref()
            .is_some_and(|needle| src_dbg.contains(needle))
            || trace_src_role.as_ref().is_some_and(|needle| {
                src_vm_role
                    .as_ref()
                    .is_some_and(|role| role.contains(needle))
            })
            || trace_new
                .as_ref()
                .is_some_and(|needle| target_dbg.contains(needle))
            || trace_new_role.as_ref().is_some_and(|needle| {
                target_vm_role
                    .as_ref()
                    .is_some_and(|role| role.contains(needle))
            })
            || trace_old_role.as_ref().is_some_and(|needle| {
                old_vm_role
                    .as_ref()
                    .is_some_and(|role| role.contains(needle))
            })
            || trace_slot
                .as_ref()
                .is_some_and(|needle| slot_desc.contains(needle))
    } else {
        true
    };
    if should_trace {
        if TRACE_BUDGET.fetch_sub(1, Ordering::Relaxed) <= 0 {
            return;
        }
        log::info!(
            "Potential stale UFFD heap write: src={} src_kind={:?} src_dbg={} src_vm_role={:?} src_state=[{}] slot={} old={:?} old_dbg={:?} old_vm_role={:?} old_state={:?} new={} new_dbg={} new_vm_role={:?} new_state=[{}]",
            src,
            crate::abi::Oop::from(src).klass::<COMPRESSED>().kind,
            src_dbg,
            src_vm_role,
            src_state,
            slot_desc,
            old_value,
            old_dbg,
            old_vm_role,
            old_state,
            target,
            target_dbg,
            target_vm_role,
            target_state,
        );
    }

    let abort_src = std::env::var("MMTK_ABORT_ON_UFFD_WRITE_SRC_DBG_SUBSTR").ok();
    let abort_src_role = std::env::var("MMTK_ABORT_ON_UFFD_WRITE_SRC_ROLE_SUBSTR").ok();
    let abort_new = std::env::var("MMTK_ABORT_ON_UFFD_WRITE_NEW_DBG_SUBSTR").ok();
    let abort_new_role = std::env::var("MMTK_ABORT_ON_UFFD_WRITE_NEW_ROLE_SUBSTR").ok();
    let abort_old_role = std::env::var("MMTK_ABORT_ON_UFFD_WRITE_OLD_ROLE_SUBSTR").ok();
    let abort_slot = std::env::var("MMTK_ABORT_ON_UFFD_WRITE_SLOT_SUBSTR").ok();
    let require_all = std::env::var("MMTK_ABORT_ON_UFFD_WRITE_REQUIRE_ALL")
        .ok()
        .is_some_and(|value| value != "0");
    let src_matches = abort_src
        .as_ref()
        .is_none_or(|needle| src_dbg.contains(needle));
    let src_role_matches = abort_src_role.as_ref().is_none_or(|needle| {
        src_vm_role
            .as_ref()
            .is_some_and(|role| role.contains(needle))
    });
    let new_matches = abort_new
        .as_ref()
        .is_none_or(|needle| target_dbg.contains(needle));
    let new_role_matches = abort_new_role.as_ref().is_none_or(|needle| {
        target_vm_role
            .as_ref()
            .is_some_and(|role| role.contains(needle))
    });
    let old_role_matches = abort_old_role.as_ref().is_none_or(|needle| {
        old_vm_role
            .as_ref()
            .is_some_and(|role| role.contains(needle))
    });
    let slot_matches = abort_slot
        .as_ref()
        .is_none_or(|needle| slot_desc.contains(needle));
    let any_filter_set = abort_src.is_some()
        || abort_src_role.is_some()
        || abort_new.is_some()
        || abort_new_role.is_some()
        || abort_old_role.is_some()
        || abort_slot.is_some();
    let should_abort = if require_all {
        any_filter_set
            && src_matches
            && src_role_matches
            && new_matches
            && new_role_matches
            && old_role_matches
            && slot_matches
    } else {
        abort_src
            .as_ref()
            .is_some_and(|needle| src_dbg.contains(needle))
            || abort_src_role.as_ref().is_some_and(|needle| {
                src_vm_role
                    .as_ref()
                    .is_some_and(|role| role.contains(needle))
            })
            || abort_new
                .as_ref()
                .is_some_and(|needle| target_dbg.contains(needle))
            || abort_new_role.as_ref().is_some_and(|needle| {
                target_vm_role
                    .as_ref()
                    .is_some_and(|role| role.contains(needle))
            })
            || abort_old_role.as_ref().is_some_and(|needle| {
                old_vm_role
                    .as_ref()
                    .is_some_and(|role| role.contains(needle))
            })
            || abort_slot
                .as_ref()
                .is_some_and(|needle| slot_desc.contains(needle))
    };
    if should_abort {
        log::error!(
            "Aborting on matched UFFD write trace: require_all={} src_dbg={:?} src_role={:?} new_dbg={:?} new_role={:?} old_role={:?} slot={:?}",
            require_all,
            abort_src,
            abort_src_role,
            abort_new,
            abort_new_role,
            abort_old_role,
            abort_slot,
        );
        std::process::abort();
    }
}

#[no_mangle]
pub extern "C" fn mmtk_debug_trace_uffd_write(
    src: ObjectReference,
    slot: Address,
    target: NullableObjectReference,
) {
    if crate::use_compressed_oops() {
        trace_stale_uffd_write::<true>(src, slot, target)
    } else {
        trace_stale_uffd_write::<false>(src, slot, target)
    }
}

/// Full pre barrier
#[no_mangle]
pub extern "C" fn mmtk_object_reference_write_pre(
    mutator: *mut libc::c_void,
    src: ObjectReference,
    slot: Address,
    target: NullableObjectReference,
) {
    with_mutator!(|mutator| {
        mutator
            .barrier()
            .object_reference_write_pre(src, slot.into(), target.into());
    })
}

/// Full post barrier
#[no_mangle]
pub extern "C" fn mmtk_object_reference_write_post(
    mutator: *mut libc::c_void,
    src: ObjectReference,
    slot: Address,
    target: NullableObjectReference,
) {
    with_mutator!(|mutator| {
        mutator
            .barrier()
            .object_reference_write_post(src, slot.into(), target.into());
    })
}

/// Barrier slow-path call
#[no_mangle]
pub extern "C" fn mmtk_object_reference_write_slow(
    mutator: *mut libc::c_void,
    src: ObjectReference,
    slot: Address,
    target: NullableObjectReference,
) {
    with_mutator!(|mutator| {
        mutator
            .barrier()
            .object_reference_write_slow(src, slot.into(), target.into());
    })
}

fn log_bytes_in_slot() -> usize {
    if crate::use_compressed_oops() {
        OpenJDKSlot::<true>::LOG_BYTES_IN_SLOT
    } else {
        OpenJDKSlot::<false>::LOG_BYTES_IN_SLOT
    }
}

/// Array-copy pre-barrier
#[no_mangle]
pub extern "C" fn mmtk_array_copy_pre(
    mutator: *mut libc::c_void,
    src: Address,
    dst: Address,
    count: usize,
) {
    let bytes = count << log_bytes_in_slot();
    with_mutator!(|mutator| {
        mutator
            .barrier()
            .memory_region_copy_pre((src..src + bytes).into(), (dst..dst + bytes).into());
    })
}

/// Array-copy post-barrier
#[no_mangle]
pub extern "C" fn mmtk_array_copy_post(
    mutator: *mut libc::c_void,
    src: Address,
    dst: Address,
    count: usize,
) {
    with_mutator!(|mutator| {
        let bytes = count << log_bytes_in_slot();
        mutator
            .barrier()
            .memory_region_copy_post((src..src + bytes).into(), (dst..dst + bytes).into());
    })
}

/// C2 Slowpath allocation barrier
#[no_mangle]
pub extern "C" fn mmtk_object_probable_write(mutator: *mut libc::c_void, obj: ObjectReference) {
    with_mutator!(|mutator| mutator.barrier().object_probable_write(obj));
}

// finalization
#[no_mangle]
pub extern "C" fn add_finalizer(object: ObjectReference) {
    with_singleton!(|singleton| memory_manager::add_finalizer(singleton, object));
}

#[no_mangle]
pub extern "C" fn get_finalized_object() -> NullableObjectReference {
    with_singleton!(|singleton| memory_manager::get_finalized_object(singleton).into())
}

thread_local! {
    /// Cache reference slots of an nmethod while the current thread is executing
    /// `MMTkRegisterNMethodOopClosure`.
    static NMETHOD_SLOTS: RefCell<Vec<Address>> = const { RefCell::new(vec![]) };
}

/// Report one reference slot in an nmethod to MMTk.
#[no_mangle]
pub extern "C" fn mmtk_add_nmethod_oop(addr: Address) {
    NMETHOD_SLOTS.with_borrow_mut(|x| x.push(addr))
}

/// Register an nmethod.
///
/// The C++ part of the binding should have scanned the nmethod and reported all the reference slots
/// using `mmtk_add_nmethod_oop` before calling this function. This function will transfer all the
/// locally cached slots of this nmethod to the global storage.
#[no_mangle]
pub extern "C" fn mmtk_register_nmethod(nm: Address) {
    NMETHOD_SLOTS.with_borrow_mut(|slots| {
        if !slots.is_empty() {
            let mut roots = crate::NURSERY_CODE_CACHE_ROOTS.lock().unwrap();
            let mut mature_roots = crate::MATURE_CODE_CACHE_ROOTS.lock().unwrap();
            // The nmethod might already be in the mature roots, if we
            // are re-registering the nmethod due to code patching.
            // If the nmethod is already in the mature roots, we demote
            // the nmethod to the nursery roots, as the nmethod might
            // now refer to a young object.
            mature_roots.remove(&nm);
            roots.insert(nm, std::mem::take(slots));
        }
    });
}

/// Unregister an nmethod.
#[no_mangle]
pub extern "C" fn mmtk_unregister_nmethod(nm: Address) {
    {
        let mut roots = crate::NURSERY_CODE_CACHE_ROOTS.lock().unwrap();
        roots.remove(&nm);
    }
    {
        let mut roots = crate::MATURE_CODE_CACHE_ROOTS.lock().unwrap();
        roots.remove(&nm);
    }
}

use crate::OpenJDKSlot;

use super::abi::*;
use super::UPCALLS;
use mmtk::util::opaque_pointer::*;
use mmtk::util::{Address, ObjectReference};
use mmtk::vm::slot::{MemorySlice, Slot};
use mmtk::vm::SlotVisitor;
use std::cell::{Cell, UnsafeCell};
use std::sync::atomic::{AtomicI32, Ordering};
use std::mem;

type S<const COMPRESSED: bool> = OpenJDKSlot<COMPRESSED>;

trait OopIterate: Sized {
    fn oop_iterate<const COMPRESSED: bool>(
        &self,
        oop: Oop,
        closure: &mut impl SlotVisitor<OpenJDKSlot<COMPRESSED>>,
    );
}

impl OopIterate for OopMapBlock {
    fn oop_iterate<const COMPRESSED: bool>(
        &self,
        oop: Oop,
        closure: &mut impl SlotVisitor<S<COMPRESSED>>,
    ) {
        let log_bytes_in_oop = if COMPRESSED { 2 } else { 3 };
        let start = oop.get_field_address(self.offset);
        for i in 0..self.count as usize {
            let slot = (start + (i << log_bytes_in_oop)).into();
            closure.visit_slot(slot);
        }
    }
}

impl OopIterate for InstanceKlass {
    fn oop_iterate<const COMPRESSED: bool>(
        &self,
        oop: Oop,
        closure: &mut impl SlotVisitor<S<COMPRESSED>>,
    ) {
        let oop_maps = self.nonstatic_oop_maps();
        for map in oop_maps {
            map.oop_iterate::<COMPRESSED>(oop, closure)
        }
    }
}

impl OopIterate for InstanceMirrorKlass {
    fn oop_iterate<const COMPRESSED: bool>(
        &self,
        oop: Oop,
        closure: &mut impl SlotVisitor<S<COMPRESSED>>,
    ) {
        self.instance_klass.oop_iterate::<COMPRESSED>(oop, closure);

        // static fields
        let start = Self::start_of_static_fields(oop);
        let len = Self::static_oop_field_count(oop);
        let slot_bytes = OpenJDKSlot::<COMPRESSED>::BYTES_IN_SLOT;
        for i in 0..len {
            closure.visit_slot((start + i * slot_bytes).into());
        }
    }
}

impl OopIterate for InstanceClassLoaderKlass {
    fn oop_iterate<const COMPRESSED: bool>(
        &self,
        oop: Oop,
        closure: &mut impl SlotVisitor<S<COMPRESSED>>,
    ) {
        self.instance_klass.oop_iterate::<COMPRESSED>(oop, closure);
    }
}

impl OopIterate for ObjArrayKlass {
    fn oop_iterate<const COMPRESSED: bool>(
        &self,
        oop: Oop,
        closure: &mut impl SlotVisitor<S<COMPRESSED>>,
    ) {
        let array = unsafe { oop.as_array_oop() };
        let slots = unsafe { array.slice::<COMPRESSED>(BasicType::T_OBJECT) };
        for slot in slots.iter_slots() {
            closure.visit_slot(slot);
        }
    }
}

impl OopIterate for TypeArrayKlass {
    fn oop_iterate<const COMPRESSED: bool>(
        &self,
        _oop: Oop,
        _closure: &mut impl SlotVisitor<S<COMPRESSED>>,
    ) {
        // Performance tweak: We skip processing the klass pointer since all
        // TypeArrayKlasses are guaranteed processed via the null class loader.
    }
}

impl OopIterate for InstanceRefKlass {
    fn oop_iterate<const COMPRESSED: bool>(
        &self,
        oop: Oop,
        closure: &mut impl SlotVisitor<S<COMPRESSED>>,
    ) {
        use crate::abi::*;
        use crate::api::{add_phantom_candidate, add_soft_candidate, add_weak_candidate};
        self.instance_klass.oop_iterate::<COMPRESSED>(oop, closure);

        // Unlike OpenJDK's built-in collectors, we do not use the `discovered` field for
        // recording discovered references (we use `add_{soft,weak,phantom}_candidate`).
        // The `discovered` field links all `Reference` instances that are either in the
        // global "reference pending list" or given to the `ReferenceHandler` thread.
        // We treat it as a strong field.
        let discovered_addr: OpenJDKSlot<COMPRESSED> = Self::discovered_address::<COMPRESSED>(oop);
        if SLOT_REWRITE_MODE.with(|flag| flag.get())
            && std::env::var_os("MMTK_TRACE_COMPRESSOR_SLOT_REWRITE_DISCOVERED").is_some()
        {
            static DISCOVERED_TRACE_BUDGET: AtomicI32 = AtomicI32::new(64);
            if DISCOVERED_TRACE_BUDGET.fetch_sub(1, Ordering::Relaxed) > 0 {
                let reference = ObjectReference::from(oop);
                let discovered = discovered_addr.load();
                log::info!(
                    "Compressor slot rewrite discovered field: reference={} type={:?} discovered={:?}",
                    reference,
                    self.instance_klass.reference_type,
                    discovered,
                );
            }
        }
        closure.visit_slot(discovered_addr);

        if SLOT_REWRITE_MODE.with(|flag| flag.get()) {
            // In the concurrent UFFD slot-rewrite path, the reference processor is supposed to
            // have already updated/cleared weak referents in the source object before the shadow
            // snapshot is materialized. Preserve weak/soft/phantom referents by default, but keep
            // an opt-in experiment to rewrite them as ordinary strong slots so we can distinguish
            // stale preserved referents from other stale Reference fields.
            let rewrite_weak_referents = std::env::var_os(
                "MMTK_COMPRESSOR_SLOT_REWRITE_PROCESS_WEAK_REFERENTS",
            )
            .is_some();
            if matches!(self.instance_klass.reference_type, ReferenceType::Final)
                || rewrite_weak_referents
            {
                if rewrite_weak_referents
                    && !matches!(self.instance_klass.reference_type, ReferenceType::Final)
                    && std::env::var_os("MMTK_TRACE_COMPRESSOR_SLOT_REWRITE_REFS").is_some()
                {
                    static REWRITE_TRACE_BUDGET: AtomicI32 = AtomicI32::new(64);
                    if REWRITE_TRACE_BUDGET.fetch_sub(1, Ordering::Relaxed) > 0 {
                        let reference = ObjectReference::from(oop);
                        let referent = Self::referent_address::<COMPRESSED>(oop).load();
                        log::info!(
                            "Compressor slot rewrite processing weak referent as strong: reference={} type={:?} referent={:?}",
                            reference,
                            self.instance_klass.reference_type,
                            referent,
                        );
                    }
                }
                Self::process_ref_as_strong(oop, closure);
            } else if std::env::var_os("MMTK_TRACE_COMPRESSOR_SLOT_REWRITE_REFS").is_some() {
                static TRACE_BUDGET: AtomicI32 = AtomicI32::new(64);
                if TRACE_BUDGET.fetch_sub(1, Ordering::Relaxed) > 0 {
                    let reference = ObjectReference::from(oop);
                    let referent_addr = Self::referent_address::<COMPRESSED>(oop);
                    let referent = referent_addr.load();
                    log::info!(
                        "Compressor slot rewrite preserved referent: reference={} type={:?} referent={:?}",
                        reference,
                        self.instance_klass.reference_type,
                        referent,
                    );
                }
            }
            return;
        }

        if Self::should_scan_weak_refs::<COMPRESSED>() {
            let reference = ObjectReference::from(oop);
            match self.instance_klass.reference_type {
                ReferenceType::None => {
                    panic!("oop_iterate on InstanceRefKlass with reference_type as None")
                }
                ReferenceType::Weak => add_weak_candidate(reference),
                ReferenceType::Soft => add_soft_candidate(reference),
                ReferenceType::Phantom => add_phantom_candidate(reference),
                // Process final reference normally.
                // We will handle final reference later
                ReferenceType::Final => Self::process_ref_as_strong(oop, closure),
            }
        } else {
            Self::process_ref_as_strong(oop, closure);
        }
    }
}

impl InstanceRefKlass {
    fn should_scan_weak_refs<const COMPRESSED: bool>() -> bool {
        !*crate::singleton::<COMPRESSED>()
            .get_options()
            .no_reference_types
            && !SUPPRESS_WEAK_REF_DISCOVERY.with(|flag| flag.get())
    }
    fn process_ref_as_strong<const COMPRESSED: bool>(
        oop: Oop,
        closure: &mut impl SlotVisitor<S<COMPRESSED>>,
    ) {
        let referent_addr = Self::referent_address::<COMPRESSED>(oop);
        closure.visit_slot(referent_addr);
    }
}

#[allow(unused)]
fn oop_iterate_slow<const COMPRESSED: bool, V: SlotVisitor<S<COMPRESSED>>>(
    oop: Oop,
    closure: &mut V,
    tls: OpaquePointer,
) {
    unsafe {
        CLOSURE.with(|x| *x.get() = closure as *mut V as *mut u8);
        ((*UPCALLS).scan_object)(
            mem::transmute::<*const unsafe extern "C" fn(Address), *mut libc::c_void>(
                scan_object_fn::<COMPRESSED, V> as *const unsafe extern "C" fn(slot: Address),
            ),
            mem::transmute::<&OopDesc, ObjectReference>(oop),
            tls,
        );
    }
}

fn oop_iterate<const COMPRESSED: bool>(oop: Oop, closure: &mut impl SlotVisitor<S<COMPRESSED>>) {
    let klass = oop.klass::<COMPRESSED>();
    let klass_id = klass.kind;
    assert!(
        klass_id as i32 >= 0 && (klass_id as i32) < KlassKind::Unknown as i32,
        "Invalid klass-id: {:x} for oop: {:x}",
        klass_id as i32,
        unsafe { mem::transmute::<Oop, ObjectReference>(oop) }
    );
    match klass_id {
        KlassKind::Instance => {
            let instance_klass = unsafe { klass.cast::<InstanceKlass>() };
            instance_klass.oop_iterate::<COMPRESSED>(oop, closure);
        }
        KlassKind::InstanceClassLoader => {
            let instance_klass = unsafe { klass.cast::<InstanceClassLoaderKlass>() };
            instance_klass.oop_iterate::<COMPRESSED>(oop, closure);
        }
        KlassKind::InstanceMirror => {
            let instance_klass = unsafe { klass.cast::<InstanceMirrorKlass>() };
            instance_klass.oop_iterate::<COMPRESSED>(oop, closure);
        }
        KlassKind::ObjArray => {
            let array_klass = unsafe { klass.cast::<ObjArrayKlass>() };
            array_klass.oop_iterate::<COMPRESSED>(oop, closure);
        }
        KlassKind::TypeArray => {
            // Skip scanning primitive arrays as they contain no reference fields.
        }
        KlassKind::InstanceRef => {
            let instance_klass = unsafe { klass.cast::<InstanceRefKlass>() };
            instance_klass.oop_iterate::<COMPRESSED>(oop, closure);
        }
        KlassKind::InstanceStackChunk => {
            unreachable!("StackChunkOop requires slow-path oop_iterate with VM support")
        }
        KlassKind::Unknown => {
            unreachable!("Unknown KlassKind")
        }
    }
}

thread_local! {
    static CLOSURE: UnsafeCell<*mut u8> = const { UnsafeCell::new(std::ptr::null_mut()) };
    static SUPPRESS_WEAK_REF_DISCOVERY: Cell<bool> = const { Cell::new(false) };
    static SLOT_REWRITE_MODE: Cell<bool> = const { Cell::new(false) };
}

pub unsafe extern "C" fn scan_object_fn<
    const COMPRESSED: bool,
    V: SlotVisitor<OpenJDKSlot<COMPRESSED>>,
>(
    slot: Address,
) {
    let ptr: *mut u8 = CLOSURE.with(|x| *x.get());
    let closure = &mut *(ptr as *mut V);
    closure.visit_slot(slot.into());
}

pub fn scan_object<const COMPRESSED: bool>(
    object: ObjectReference,
    closure: &mut impl SlotVisitor<S<COMPRESSED>>,
    tls: VMWorkerThread,
) {
    unsafe {
        let oop = mem::transmute::<ObjectReference, &OopDesc>(object);
        if oop.klass::<COMPRESSED>().kind == KlassKind::InstanceStackChunk {
            oop_iterate_slow::<COMPRESSED, _>(oop, closure, tls.0.0);
        } else {
            oop_iterate::<COMPRESSED>(oop, closure)
        }
    }
}

pub fn slot_offset<const COMPRESSED: bool>(
    object: ObjectReference,
    slot: OpenJDKSlot<COMPRESSED>,
) -> usize {
    slot.addr - object.to_raw_address()
}

pub fn load_slot_at_offset<const COMPRESSED: bool>(
    object: ObjectReference,
    offset: usize,
) -> Option<ObjectReference> {
    OpenJDKSlot::<COMPRESSED> {
        addr: object.to_raw_address() + offset,
    }
    .load()
}

pub fn describe_slot<const COMPRESSED: bool>(
    object: ObjectReference,
    slot: OpenJDKSlot<COMPRESSED>,
) -> Option<String> {
    let oop = Oop::from(object);
    let klass = oop.klass::<COMPRESSED>();
    let slot_offset = slot_offset::<COMPRESSED>(object, slot);
    let mut desc = format!("klass={:?}, offset={}", klass.kind, slot_offset);
    if klass.kind == KlassKind::InstanceRef {
        let instance_klass = unsafe { klass.cast::<InstanceRefKlass>() };
        desc.push_str(&format!(", reference_type={:?}", instance_klass.instance_klass.reference_type));
        if slot.addr == InstanceRefKlass::referent_address::<COMPRESSED>(oop).addr {
            desc.push_str(", field=referent");
        } else if slot.addr == InstanceRefKlass::discovered_address::<COMPRESSED>(oop).addr {
            desc.push_str(", field=discovered");
        }
    }
    Some(desc)
}

pub fn scan_object_for_fixup<const COMPRESSED: bool>(
    object: ObjectReference,
    closure: &mut impl SlotVisitor<S<COMPRESSED>>,
    tls: VMWorkerThread,
) {
    SUPPRESS_WEAK_REF_DISCOVERY.with(|discovery_flag| {
        SLOT_REWRITE_MODE.with(|rewrite_flag| {
            let prev_discovery = discovery_flag.replace(true);
            let prev_rewrite = rewrite_flag.replace(true);
            unsafe {
                let oop = mem::transmute::<ObjectReference, &OopDesc>(object);
                if oop.klass::<COMPRESSED>().kind == KlassKind::InstanceStackChunk {
                    oop_iterate_slow::<COMPRESSED, _>(oop, closure, tls.0.0);
                } else {
                    oop_iterate::<COMPRESSED>(oop, closure)
                }
            }
            rewrite_flag.set(prev_rewrite);
            discovery_flag.set(prev_discovery);
        });
    });
}

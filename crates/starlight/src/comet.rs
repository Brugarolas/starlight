pub use comet as cometgc;

use comet::{
    api::{Gc, HeapObjectHeader, Trace as CometTrace, Visitor as CometVisitor, Weak},
    gc_base::{MarkingConstraint, MarkingConstraintRuns},
    immix::{instantiate_sticky_immix, StickyImmix, StickyImmixOptions},
    mutator::MutatorRef,
};
pub use comet::api::Collectable;
use mopa::mopafy;
use std::{
    collections::HashMap,
    marker::PhantomData,
    ops::{Deref, DerefMut},
    ptr::NonNull,
};

pub type CometHeap = StickyImmix<comet::gc_base::NoOpStackDecoder>;

pub trait GcCell: mopa::Any {
    fn compute_size(&self) -> usize {
        std::mem::size_of_val(self)
    }

    fn type_name(&self) -> &'static str {
        std::any::type_name::<Self>()
    }

    fn deser_pair(&self) -> (usize, usize) {
        (0, 0)
    }
}

mopafy!(GcCell);

pub mod cell {
    pub use super::*;
}

macro_rules! impl_basic_gc_cell {
    ($($ty:ty),* $(,)?) => {
        $(
            impl GcCell for $ty {}
        )*
    };
}

impl_basic_gc_cell!(
    String, bool, f32, f64, u8, i8, u16, i16, u32, i32, u64, i64, u128, i128
);

impl<K: GcCell, V: GcCell> GcCell for HashMap<K, V> {}
impl<T: GcCell> GcCell for Option<T> {}
impl<T: GcCell> GcCell for Vec<T> {}

pub trait Trace {
    fn trace(&self, visitor: &mut Visitor<'_>);
}

pub fn bridge_trace(value: &impl Trace, visitor: &mut dyn CometVisitor) {
    Trace::trace(value, &mut Visitor { inner: visitor });
}

pub type FinalizationCallback = unsafe fn(*mut u8);

pub trait FinalizeTrait<T> {
    const CALLBACK: Option<FinalizationCallback> = None;
}

pub trait Finalize<T> {}

pub struct Visitor<'a> {
    inner: &'a mut dyn CometVisitor,
}

impl Visitor<'_> {
    pub fn trace_untyped(&mut self, root: NonNull<HeapObjectHeader>) {
        let mut root = root;
        self.inner.mark_object(&mut root);
    }

    pub fn trace_weak(&mut self, root: NonNull<HeapObjectHeader>) {
        let mut root = root;
        self.inner.mark_weak(&mut root);
    }
}

pub struct SimpleMarkingConstraint {
    name: String,
    exec: Box<dyn FnMut(&mut Visitor<'_>)>,
}

impl SimpleMarkingConstraint {
    pub fn new(name: &str, exec: impl FnMut(&mut Visitor<'_>) + 'static) -> Self {
        Self {
            name: name.to_owned(),
            exec: Box::new(exec),
        }
    }
}

unsafe impl MarkingConstraint for SimpleMarkingConstraint {
    fn runs_at(&self) -> MarkingConstraintRuns {
        MarkingConstraintRuns::BeforeMark
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn is_over(&self) -> bool {
        false
    }

    fn run(&mut self, visitor: &mut dyn CometVisitor) {
        (self.exec)(&mut Visitor { inner: visitor });
    }
}

pub struct Heap {
    mutator: MutatorRef<CometHeap>,
    defer_count: usize,
    pending_collection: bool,
}

impl Heap {
    pub fn new(opts: &crate::options::Options) -> Self {
        let heap_size = opts.heap_size.max(4 * 1024 * 1024);
        let options = StickyImmixOptions::default()
            .with_heap_size(heap_size)
            .with_initial_size(heap_size.min(32 * 1024 * 1024))
            .with_min_heap_size(heap_size.min(4 * 1024 * 1024))
            .with_max_heap_size(heap_size)
            .with_verbose(u8::from(opts.verbose_gc));
        Self {
            mutator: instantiate_sticky_immix::<comet::gc_base::NoOpStackDecoder>(options),
            defer_count: 0,
            pending_collection: false,
        }
    }

    pub fn defer(&mut self) -> DeferPoint {
        self.defer_count += 1;
        DeferPoint {
            heap: self as *mut Self,
        }
    }

    pub fn gc(&mut self) {
        if self.defer_count > 0 {
            self.pending_collection = true;
        } else {
            let mut roots: [&mut dyn CometTrace; 0] = [];
            self.mutator.full_collection(&mut roots);
        }
    }

    pub fn allocate<T: GcCell + Collectable>(&mut self, value: T) -> GcPointer<T> {
        GcPointer {
            value: self
                .mutator
                .allocate(value, comet::gc_base::AllocationSpace::New),
        }
    }

    pub unsafe fn allocate_raw<T: GcCell + Collectable>(
        &mut self,
        size: usize,
    ) -> *mut GcPointerBase {
        self.mutator
            .allocate_raw::<T>(size)
            .unwrap_or_else(|| memory_oom())
            .as_raw()
            .as_ptr()
            .cast()
    }

    pub fn add_constraint(&mut self, constraint: impl MarkingConstraint + 'static) {
        self.mutator.add_constraint(constraint);
    }

    pub fn make_weak<T: GcCell + Collectable>(&mut self, target: GcPointer<T>) -> WeakRef<T> {
        WeakRef {
            ref_: self.mutator.allocate_weak(target.value),
            marker: PhantomData,
        }
    }

    pub fn collect_if_necessary(&mut self) {
        if self.defer_count > 0 {
            self.pending_collection = true;
            return;
        }

        self.mutator.safepoint();
        if self.pending_collection {
            self.pending_collection = false;
            self.gc();
        }
    }
}

pub struct DeferPoint {
    heap: *mut Heap,
}

impl Drop for DeferPoint {
    fn drop(&mut self) {
        unsafe {
            let heap = &mut *self.heap;
            heap.defer_count = heap.defer_count.saturating_sub(1);
        }
    }
}

#[repr(transparent)]
pub struct GcPointerBase {
    hdr: HeapObjectHeader,
}

impl GcPointerBase {
    pub fn allocation_size(&self) -> usize {
        unsafe {
            Gc::<dyn Collectable, CometHeap>::from_raw(NonNull::from(&self.hdr))
                .allocation_size()
        }
    }

    pub fn get_dyn(&mut self) -> &mut dyn Collectable {
        self.hdr.get_dyn()
    }

    pub fn data<T>(&self) -> *mut T {
        unsafe {
            (self as *const Self as *mut u8)
                .add(std::mem::size_of::<Self>())
                .cast()
        }
    }

    pub unsafe fn mark_initialized<T: Collectable>(&mut self) {
        let raw = NonNull::from(&mut self.hdr);
        let _: Gc<T, CometHeap> = Gc::<std::mem::MaybeUninit<T>, CometHeap>::from_raw(raw)
            .assume_init();
    }
}

pub struct GcPointer<T: Collectable + ?Sized> {
    value: Gc<T, CometHeap>,
}

impl<T: Collectable + ?Sized> GcPointer<T> {
    pub fn untyped(self) -> NonNull<HeapObjectHeader> {
        self.value.as_raw()
    }

    pub fn ptr_eq<U: Collectable + ?Sized>(this: &Self, other: &GcPointer<U>) -> bool {
        this.value.as_raw() == other.value.as_raw()
    }

    pub fn as_dyn(self) -> GcPointer<dyn Collectable> {
        GcPointer {
            value: self.value.to_dyn(),
        }
    }

    pub fn is<U: Collectable>(self) -> bool {
        self.value.is::<U>()
    }

    pub fn get_dyn(&self) -> &dyn Collectable {
        self.value.get_dyn()
    }

    pub fn get_dyn_mut(&mut self) -> &mut dyn Collectable {
        self.value.get_dyn_mut()
    }

    pub unsafe fn downcast_unchecked<U: Collectable>(self) -> GcPointer<U> {
        GcPointer {
            value: self.value.downcast_unchecked(),
        }
    }

    pub fn downcast<U: Collectable>(self) -> Option<GcPointer<U>> {
        self.value.downcast().map(|value| GcPointer { value })
    }

    pub unsafe fn from_raw(base: NonNull<GcPointerBase>) -> Self {
        GcPointer {
            value: Gc::from_raw(base.cast()),
        }
    }

    pub unsafe fn from_address(address: usize) -> Self {
        Self::from_raw(NonNull::new_unchecked(address as *mut GcPointerBase))
    }
}

impl<T: Collectable + ?Sized> Copy for GcPointer<T> {}

impl<T: Collectable + ?Sized> Clone for GcPointer<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T: Collectable> Deref for GcPointer<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.value
    }
}

impl<T: Collectable> DerefMut for GcPointer<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.value
    }
}

impl<T: Collectable + ?Sized> std::fmt::Pointer for GcPointer<T> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Pointer::fmt(&self.value, formatter)
    }
}

impl<T: Collectable + std::fmt::Debug> std::fmt::Debug for GcPointer<T> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&self.value, formatter)
    }
}

impl<T: Collectable + std::fmt::Display> std::fmt::Display for GcPointer<T> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.value, formatter)
    }
}

pub struct WeakRef<T: GcCell + Collectable> {
    ref_: Weak<T, CometHeap>,
    marker: PhantomData<T>,
}

impl<T: GcCell + Collectable> WeakRef<T> {
    pub fn upgrade(&self) -> Option<GcPointer<T>> {
        self.ref_.upgrade().map(|value| GcPointer { value })
    }
}

impl<T: GcCell + Collectable> Copy for WeakRef<T> {}

impl<T: GcCell + Collectable> Clone for WeakRef<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T: Collectable + ?Sized> PartialEq for GcPointer<T> {
    fn eq(&self, other: &Self) -> bool {
        self.value.as_raw() == other.value.as_raw()
    }
}

impl<T: Collectable + ?Sized> Eq for GcPointer<T> {}

impl<T: Collectable + ?Sized> Trace for GcPointer<T> {
    fn trace(&self, visitor: &mut Visitor<'_>) {
        visitor.trace_untyped(self.value.as_raw());
    }
}

impl<T: GcCell + Collectable> Trace for WeakRef<T> {
    fn trace(&self, visitor: &mut Visitor<'_>) {
        visitor.trace_weak(NonNull::new(unsafe { self.ref_.base() }).unwrap());
    }
}

impl<T: Trace> Trace for Option<T> {
    fn trace(&self, visitor: &mut Visitor<'_>) {
        if let Some(value) = self {
            value.trace(visitor);
        }
    }
}

impl<T: Trace> Trace for Vec<T> {
    fn trace(&self, visitor: &mut Visitor<'_>) {
        for value in self {
            value.trace(visitor);
        }
    }
}

impl<K: Trace, V: Trace, S> Trace for HashMap<K, V, S> {
    fn trace(&self, visitor: &mut Visitor<'_>) {
        for (key, value) in self {
            key.trace(visitor);
            value.trace(visitor);
        }
    }
}

macro_rules! impl_primitive_trace {
    ($($ty:ty),* $(,)?) => {
        $(
            impl Trace for $ty {
                fn trace(&self, _visitor: &mut Visitor<'_>) {}
            }
        )*
    };
}

impl_primitive_trace!(
    String, bool, f32, f64, u8, i8, u16, i16, u32, i32, u64, i64, u128, i128, char
);

#[macro_export]
macro_rules! impl_comet_compat {
    ($ty:ty) => {
        unsafe impl $crate::comet::cometgc::api::Trace for $ty {
            fn trace(
                &mut self,
                visitor: &mut dyn $crate::comet::cometgc::api::Visitor,
            ) {
                $crate::comet::bridge_trace(self, visitor);
            }
        }

        unsafe impl $crate::comet::cometgc::api::Finalize for $ty {}

        impl $crate::comet::cometgc::api::Collectable for $ty {
            fn allocation_size(&self) -> usize {
                $crate::comet::GcCell::compute_size(self)
            }
        }
    };
}

#[cold]
fn memory_oom() -> ! {
    eprintln!("Starlight: No memory left");
    std::process::abort();
}

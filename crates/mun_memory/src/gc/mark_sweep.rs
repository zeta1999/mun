use crate::{
    cast,
    gc::{Event, GcPtr, GcRuntime, Observer, RawGcPtr, Stats, TypeTrace},
    mapping::{self, FieldMapping, MemoryMapper},
    TypeLayout,
};
use mapping::{Conversion, Mapping};
use parking_lot::RwLock;
use std::{
    collections::{HashMap, VecDeque},
    hash::Hash,
    ops::Deref,
    pin::Pin,
    ptr::NonNull,
};

/// Implements a simple mark-sweep type garbage collector.
#[derive(Debug)]
pub struct MarkSweep<T, O>
where
    T: TypeLayout + TypeTrace + Clone,
    O: Observer<Event = Event>,
{
    objects: RwLock<HashMap<GcPtr, Pin<Box<ObjectInfo<T>>>>>,
    observer: O,
    stats: RwLock<Stats>,
}

impl<T, O> Default for MarkSweep<T, O>
where
    T: TypeLayout + TypeTrace + Clone,
    O: Observer<Event = Event> + Default,
{
    fn default() -> Self {
        MarkSweep {
            objects: RwLock::new(HashMap::new()),
            observer: O::default(),
            stats: RwLock::new(Stats::default()),
        }
    }
}

impl<T, O> MarkSweep<T, O>
where
    T: TypeLayout + TypeTrace + Clone,
    O: Observer<Event = Event>,
{
    /// Creates a `MarkSweep` memory collector with the specified `Observer`.
    pub fn with_observer(observer: O) -> Self {
        Self {
            objects: RwLock::new(HashMap::new()),
            observer,
            stats: RwLock::new(Stats::default()),
        }
    }

    /// Returns the observer
    pub fn observer(&self) -> &O {
        &self.observer
    }
}

impl<T, O> GcRuntime<T> for MarkSweep<T, O>
where
    T: TypeLayout + TypeTrace + Clone,
    O: Observer<Event = Event>,
{
    fn alloc(&self, ty: T) -> GcPtr {
        let layout = ty.layout();
        let ptr = unsafe { std::alloc::alloc(layout) };
        let object = Box::pin(ObjectInfo {
            ptr,
            ty,
            roots: 0,
            color: Color::White,
        });

        // We want to return a pointer to the `ObjectInfo`, to be used as handle.
        let handle = (object.as_ref().deref() as *const _ as RawGcPtr).into();

        {
            let mut objects = self.objects.write();
            objects.insert(handle, object);
        }

        {
            let mut stats = self.stats.write();
            stats.allocated_memory += layout.size();
        }

        self.observer.event(Event::Allocation(handle));
        handle
    }

    fn ptr_type(&self, handle: GcPtr) -> T {
        let _ = self.objects.read();

        // Convert the handle to our internal representation
        let object_info: *const ObjectInfo<T> = handle.into();

        // Return the type of the object
        unsafe { (*object_info).ty.clone() }
    }

    fn root(&self, handle: GcPtr) {
        let _ = self.objects.write();

        // Convert the handle to our internal representation
        let object_info: *mut ObjectInfo<T> = handle.into();

        unsafe { (*object_info).roots += 1 };
    }

    fn unroot(&self, handle: GcPtr) {
        let _ = self.objects.write();

        // Convert the handle to our internal representation
        let object_info: *mut ObjectInfo<T> = handle.into();

        unsafe { (*object_info).roots -= 1 };
    }

    fn stats(&self) -> Stats {
        self.stats.read().clone()
    }
}

impl<T, O> MarkSweep<T, O>
where
    T: TypeLayout + TypeTrace + Clone,
    O: Observer<Event = Event>,
{
    /// Collects all memory that is no longer referenced by rooted objects. Returns `true` if memory
    /// was reclaimed, `false` otherwise.
    pub fn collect(&self) -> bool {
        self.observer.event(Event::Start);

        let mut objects = self.objects.write();

        // Get all roots
        let mut roots = objects
            .iter()
            .filter_map(|(_, obj)| {
                if obj.roots > 0 {
                    Some(obj.as_ref().get_ref() as *const _ as *mut ObjectInfo<T>)
                } else {
                    None
                }
            })
            .collect::<VecDeque<_>>();

        // Iterate over all roots
        while let Some(next) = roots.pop_front() {
            let handle = (next as *const _ as RawGcPtr).into();

            // Trace all other objects
            for reference in unsafe { (*next).ty.trace(handle) } {
                let ref_ptr = objects
                    .get_mut(&reference)
                    .expect("found invalid reference");
                if ref_ptr.color == Color::White {
                    let ptr = ref_ptr.as_ref().get_ref() as *const _ as *mut ObjectInfo<T>;
                    unsafe { (*ptr).color = Color::Gray };
                    roots.push_back(ptr);
                }
            }

            // This object has been traced
            unsafe {
                (*next).color = Color::Black;
            }
        }

        // Sweep all non-reachable objects
        let size_before = objects.len();
        objects.retain(|h, obj| {
            if obj.color == Color::Black {
                unsafe {
                    obj.as_mut().get_unchecked_mut().color = Color::White;
                }
                true
            } else {
                unsafe { std::alloc::dealloc(obj.ptr, obj.ty.layout()) };
                self.observer.event(Event::Deallocation(*h));
                {
                    let mut stats = self.stats.write();
                    stats.allocated_memory -= obj.ty.layout().size();
                }
                false
            }
        });
        let size_after = objects.len();

        self.observer.event(Event::End);

        size_before != size_after
    }
}

impl<T, O> MemoryMapper<T> for MarkSweep<T, O>
where
    T: TypeLayout + TypeTrace + Clone + Eq + Hash,
    O: Observer<Event = Event>,
{
    fn map_memory(&self, mapping: Mapping<T, T>) -> Vec<GcPtr> {
        let mut objects = self.objects.write();

        // Determine which types are still allocated with deleted types
        let deleted = objects
            .iter()
            .filter_map(|(ptr, object_info)| {
                if mapping.deletions.contains(&object_info.ty) {
                    Some(*ptr)
                } else {
                    None
                }
            })
            .collect();

        for (old_ty, conversion) in mapping.conversions {
            for object_info in objects.values_mut() {
                if object_info.ty == old_ty {
                    map_fields(object_info, &conversion);
                }
            }
        }

        return deleted;

        fn map_fields<T: Clone + TypeLayout + TypeTrace>(
            object_info: &mut Pin<Box<ObjectInfo<T>>>,
            conversion: &Conversion<T>,
        ) {
            let ptr = unsafe { std::alloc::alloc_zeroed(conversion.new_ty.layout()) };

            for map in conversion.field_mapping.iter() {
                if let Some(FieldMapping {
                    old_offset,
                    new_offset,
                    action,
                }) = map
                {
                    let src = {
                        let mut src = object_info.ptr as usize;
                        src += old_offset;
                        src as *mut u8
                    };
                    let dest = {
                        let mut dest = ptr as usize;
                        dest += new_offset;
                        dest as *mut u8
                    };
                    match action {
                        mapping::Action::Cast { old, new } => {
                            if !cast::try_cast_from_to(
                                *old,
                                *new,
                                unsafe { NonNull::new_unchecked(src) },
                                unsafe { NonNull::new_unchecked(dest) },
                            ) {
                                // Failed to cast. Use the previously zero-initialized value instead
                            }
                        }
                        mapping::Action::Copy { size } => unsafe {
                            std::ptr::copy_nonoverlapping(src, dest, *size)
                        },
                    }
                }
            }
            object_info.set(ObjectInfo {
                ptr,
                roots: object_info.roots,
                color: object_info.color,
                ty: conversion.new_ty.clone(),
            });
        }
    }
}

/// Coloring used in the Mark Sweep phase.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Color {
    /// A white object has not been seen yet by the mark phase
    White,

    /// A gray object has been seen by the mark phase but has not yet been visited
    Gray,

    /// A black object has been visited by the mark phase
    Black,
}

/// An indirection table that stores the address to the actual memory, the type of the object and
/// meta information.
#[derive(Debug)]
#[repr(C)]
struct ObjectInfo<T: TypeLayout + TypeTrace + Clone> {
    pub ptr: *mut u8,
    pub roots: u32,
    pub color: Color,
    pub ty: T,
}

/// An `ObjectInfo` is thread-safe.
unsafe impl<T: TypeLayout + TypeTrace + Clone> Send for ObjectInfo<T> {}
unsafe impl<T: TypeLayout + TypeTrace + Clone> Sync for ObjectInfo<T> {}

impl<T: TypeLayout + TypeTrace + Clone> Into<*const ObjectInfo<T>> for GcPtr {
    fn into(self) -> *const ObjectInfo<T> {
        self.as_ptr() as *const ObjectInfo<T>
    }
}

impl<T: TypeLayout + TypeTrace + Clone> Into<*mut ObjectInfo<T>> for GcPtr {
    fn into(self) -> *mut ObjectInfo<T> {
        self.as_ptr() as *mut ObjectInfo<T>
    }
}

impl<T: TypeLayout + TypeTrace + Clone> Into<GcPtr> for *const ObjectInfo<T> {
    fn into(self) -> GcPtr {
        (self as RawGcPtr).into()
    }
}

impl<T: TypeLayout + TypeTrace + Clone> Into<GcPtr> for *mut ObjectInfo<T> {
    fn into(self) -> GcPtr {
        (self as RawGcPtr).into()
    }
}

use crate::Context;
use anyhow::{anyhow, Result};
use derive_more::{Deref, DerefMut};
use parking_lot::{RwLock, RwLockUpgradableReadGuard};
use slotmap::{SecondaryMap, SlotMap};
use std::{
    any::{Any, TypeId},
    marker::PhantomData,
    mem,
    sync::{
        atomic::{AtomicUsize, Ordering::SeqCst},
        Arc, Weak,
    },
};

slotmap::new_key_type! { pub struct EntityId; }

pub(crate) struct EntityMap(Arc<RwLock<EntityMapState>>);

struct EntityMapState {
    ref_counts: SlotMap<EntityId, AtomicUsize>,
    entities: SecondaryMap<EntityId, Box<dyn Any + Send + Sync>>,
    dropped_entities: Vec<(EntityId, Box<dyn Any + Send + Sync>)>,
}

impl EntityMap {
    pub fn new() -> Self {
        Self(Arc::new(RwLock::new(EntityMapState {
            ref_counts: SlotMap::with_key(),
            entities: SecondaryMap::new(),
            dropped_entities: Vec::new(),
        })))
    }

    /// Reserve a slot for an entity, which you can subsequently use with `insert`.
    pub fn reserve<T: 'static + Send + Sync>(&self) -> Slot<T> {
        let id = self.0.write().ref_counts.insert(1.into());
        Slot(Handle::new(id, Arc::downgrade(&self.0)))
    }

    /// Insert an entity into a slot obtained by calling `reserve`.
    pub fn insert<T: 'static + Any + Send + Sync>(&self, slot: Slot<T>, entity: T) -> Handle<T> {
        let handle = slot.0;
        self.0.write().entities.insert(handle.id, Box::new(entity));
        handle
    }

    /// Move an entity to the stack.
    pub fn lease<T: 'static + Send + Sync>(&self, handle: &Handle<T>) -> Lease<T> {
        let id = handle.id;
        let entity = Some(
            self.0
                .write()
                .entities
                .remove(id)
                .expect("Circular entity lease. Is the entity already being updated?")
                .downcast::<T>()
                .unwrap(),
        );
        Lease { id, entity }
    }

    /// Return an entity after moving it to the stack.
    pub fn end_lease<T: 'static + Send + Sync>(&mut self, mut lease: Lease<T>) {
        self.0
            .write()
            .entities
            .insert(lease.id, lease.entity.take().unwrap());
    }

    pub fn weak_handle<T: 'static + Send + Sync>(&self, id: EntityId) -> WeakHandle<T> {
        WeakHandle {
            any_handle: AnyWeakHandle {
                id,
                entity_type: TypeId::of::<T>(),
                entity_map: Arc::downgrade(&self.0),
            },
            entity_type: PhantomData,
        }
    }

    pub fn take_dropped(&self) -> Vec<(EntityId, Box<dyn Any + Send + Sync>)> {
        mem::take(&mut self.0.write().dropped_entities)
    }
}

pub struct Lease<T> {
    entity: Option<Box<T>>,
    pub id: EntityId,
}

impl<T> core::ops::Deref for Lease<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.entity.as_ref().unwrap()
    }
}

impl<T> core::ops::DerefMut for Lease<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.entity.as_mut().unwrap()
    }
}

impl<T> Drop for Lease<T> {
    fn drop(&mut self) {
        if self.entity.is_some() {
            // We don't panic here, because other panics can cause us to drop the lease without ending it cleanly.
            log::error!("Leases must be ended with EntityMap::end_lease")
        }
    }
}

#[derive(Deref, DerefMut)]
pub struct Slot<T: Send + Sync + 'static>(Handle<T>);

pub struct AnyHandle {
    pub(crate) id: EntityId,
    entity_type: TypeId,
    entity_map: Weak<RwLock<EntityMapState>>,
}

impl AnyHandle {
    fn new(id: EntityId, entity_type: TypeId, entity_map: Weak<RwLock<EntityMapState>>) -> Self {
        Self {
            id,
            entity_type,
            entity_map,
        }
    }

    pub fn downgrade(&self) -> AnyWeakHandle {
        AnyWeakHandle {
            id: self.id,
            entity_type: self.entity_type,
            entity_map: self.entity_map.clone(),
        }
    }

    pub fn downcast<T>(&self) -> Option<Handle<T>>
    where
        T: 'static + Send + Sync,
    {
        if TypeId::of::<T>() == self.entity_type {
            Some(Handle {
                any_handle: self.clone(),
                entity_type: PhantomData,
            })
        } else {
            None
        }
    }
}

impl Clone for AnyHandle {
    fn clone(&self) -> Self {
        if let Some(entity_map) = self.entity_map.upgrade() {
            let entity_map = entity_map.read();
            let count = entity_map
                .ref_counts
                .get(self.id)
                .expect("detected over-release of a handle");
            let prev_count = count.fetch_add(1, SeqCst);
            assert_ne!(prev_count, 0, "Detected over-release of a handle.");
        }

        Self {
            id: self.id,
            entity_type: self.entity_type,
            entity_map: self.entity_map.clone(),
        }
    }
}

impl Drop for AnyHandle {
    fn drop(&mut self) {
        if let Some(entity_map) = self.entity_map.upgrade() {
            let entity_map = entity_map.upgradable_read();
            let count = entity_map
                .ref_counts
                .get(self.id)
                .expect("Detected over-release of a handle.");
            let prev_count = count.fetch_sub(1, SeqCst);
            assert_ne!(prev_count, 0, "Detected over-release of a handle.");
            if prev_count == 1 {
                // We were the last reference to this entity, so we can remove it.
                let mut entity_map = RwLockUpgradableReadGuard::upgrade(entity_map);
                let entity = entity_map
                    .entities
                    .remove(self.id)
                    .expect("entity was removed twice");
                entity_map.ref_counts.remove(self.id);
                entity_map.dropped_entities.push((self.id, entity));
            }
        }
    }
}

impl<T> From<Handle<T>> for AnyHandle
where
    T: 'static + Send + Sync,
{
    fn from(handle: Handle<T>) -> Self {
        handle.any_handle
    }
}

#[derive(Deref, DerefMut)]
pub struct Handle<T: Send + Sync> {
    #[deref]
    #[deref_mut]
    any_handle: AnyHandle,
    entity_type: PhantomData<T>,
}

impl<T: 'static + Send + Sync> Handle<T> {
    fn new(id: EntityId, entity_map: Weak<RwLock<EntityMapState>>) -> Self {
        Self {
            any_handle: AnyHandle::new(id, TypeId::of::<T>(), entity_map),
            entity_type: PhantomData,
        }
    }

    pub fn downgrade(&self) -> WeakHandle<T> {
        WeakHandle {
            any_handle: self.any_handle.downgrade(),
            entity_type: self.entity_type,
        }
    }

    /// Update the entity referenced by this handle with the given function.
    ///
    /// The update function receives a context appropriate for its environment.
    /// When updating in an `AppContext`, it receives a `ModelContext`.
    /// When updating an a `WindowContext`, it receives a `ViewContext`.
    pub fn update<C: Context, R>(
        &self,
        cx: &mut C,
        update: impl FnOnce(&mut T, &mut C::EntityContext<'_, '_, T>) -> R,
    ) -> C::Result<R> {
        cx.update_entity(self, update)
    }
}

impl<T: Send + Sync> Clone for Handle<T> {
    fn clone(&self) -> Self {
        Self {
            any_handle: self.any_handle.clone(),
            entity_type: self.entity_type,
        }
    }
}

#[derive(Clone)]
pub struct AnyWeakHandle {
    pub(crate) id: EntityId,
    entity_type: TypeId,
    entity_map: Weak<RwLock<EntityMapState>>,
}

impl AnyWeakHandle {
    pub fn upgrade(&self) -> Option<AnyHandle> {
        let entity_map = &self.entity_map.upgrade()?;
        entity_map
            .read()
            .ref_counts
            .get(self.id)?
            .fetch_add(1, SeqCst);
        Some(AnyHandle {
            id: self.id,
            entity_type: self.entity_type,
            entity_map: self.entity_map.clone(),
        })
    }
}

impl<T> From<WeakHandle<T>> for AnyWeakHandle
where
    T: 'static + Send + Sync,
{
    fn from(handle: WeakHandle<T>) -> Self {
        handle.any_handle
    }
}

#[derive(Deref, DerefMut)]
pub struct WeakHandle<T> {
    #[deref]
    #[deref_mut]
    any_handle: AnyWeakHandle,
    entity_type: PhantomData<T>,
}

impl<T: 'static + Send + Sync> Clone for WeakHandle<T> {
    fn clone(&self) -> Self {
        Self {
            any_handle: self.any_handle.clone(),
            entity_type: self.entity_type,
        }
    }
}

impl<T: Send + Sync + 'static> WeakHandle<T> {
    pub fn upgrade(&self) -> Option<Handle<T>> {
        Some(Handle {
            any_handle: self.any_handle.upgrade()?,
            entity_type: self.entity_type,
        })
    }

    /// Update the entity referenced by this handle with the given function if
    /// the referenced entity still exists. Returns an error if the entity has
    /// been released.
    ///
    /// The update function receives a context appropriate for its environment.
    /// When updating in an `AppContext`, it receives a `ModelContext`.
    /// When updating an a `WindowContext`, it receives a `ViewContext`.
    pub fn update<C: Context, R>(
        &self,
        cx: &mut C,
        update: impl FnOnce(&mut T, &mut C::EntityContext<'_, '_, T>) -> R,
    ) -> Result<R>
    where
        Result<C::Result<R>>: crate::Flatten<R>,
    {
        crate::Flatten::flatten(
            self.upgrade()
                .ok_or_else(|| anyhow!("entity release"))
                .map(|this| cx.update_entity(&this, update)),
        )
    }
}

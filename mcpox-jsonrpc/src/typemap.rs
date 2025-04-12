use std::any::{Any, TypeId};
use std::collections::HashMap;

/// Simple type map for storing and retrieving values by their type.
///
/// Metadata that is added and modified at runtime is stored in this map, so that it can be looked
/// up in a type-safe way without this crate defining _a priori_ what types are available.
pub struct TypeMap {
    map: HashMap<TypeId, Box<dyn Any + Send + Sync + 'static>>,
}

impl TypeMap {
    /// Create a new empty `TypeMap`.
    pub fn new() -> Self {
        Self { map: HashMap::new() }
    }

    /// Insert a value into the map, replacing any existing value of the same type.
    pub fn insert<T: Send + Sync + 'static>(&mut self, value: T) {
        self.map.insert(TypeId::of::<T>(), Box::new(value));
    }

    /// Get a reference to a value of type `T` from the map.
    ///
    /// Returns `None` if no value of type `T` is in the map.
    pub fn get<T: 'static>(&self) -> Option<&T> {
        self.map
            .get(&TypeId::of::<T>())
            .and_then(|boxed| boxed.downcast_ref())
    }

    /// Get a mutable reference to a value of type `T` from the map.
    ///
    /// Returns `None` if no value of type `T` is in the map.
    pub fn get_mut<T: 'static>(&mut self) -> Option<&mut T> {
        self.map
            .get_mut(&TypeId::of::<T>())
            .and_then(|boxed| boxed.downcast_mut())
    }

    /// Get a cloned copy of a value of type `T` from the map, if present, or None if not.
    pub fn get_clone<T: Clone + 'static>(&self) -> Option<T> {
        self.map
            .get(&TypeId::of::<T>())
            .and_then(|boxed| boxed.downcast_ref())
            .cloned()
    }

    pub fn get_or_insert<T: Send + Sync + 'static>(&mut self, value: T) -> &T {
        self.map
            .entry(TypeId::of::<T>())
            .or_insert(Box::new(value))
            .downcast_ref()
            .unwrap()
    }

    /// Check if the map contains a value of type `T`.
    pub fn contains<T: 'static>(&self) -> bool {
        self.map.contains_key(&TypeId::of::<T>())
    }

    /// Remove a value of type `T` from the map.
    ///
    /// Returns the removed value if it existed, or `None` otherwise.
    pub fn remove<T: 'static>(&mut self) -> Option<Box<T>> {
        let type_id = TypeId::of::<T>();
        let boxed = self.map.remove(&type_id)?;

        boxed.downcast::<T>().ok()
    }

    /// Clear all values from the map.
    pub fn clear(&mut self) {
        self.map.clear();
    }

    /// Returns the number of entries in the map.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Returns true if the map is empty.
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

impl Default for TypeMap {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    #[test]
    fn test_typemap() {
        let mut map = TypeMap::new();

        assert!(map.is_empty());

        map.insert(42);
        assert_eq!(map.len(), 1);
        assert_eq!(map.get::<i32>(), Some(&42));

        map.insert("Hello".to_string());
        assert_eq!(map.len(), 2);
        assert_eq!(map.get::<String>(), Some(&"Hello".to_string()));

        map.remove::<i32>();
        assert_eq!(map.len(), 1);
        assert_eq!(map.get::<i32>(), None);

        map.clear();
        assert_eq!(map.len(), 0);
    }

    #[test]
    fn test_arc_mutex_typemap() {
        let map = Arc::new(Mutex::new(TypeMap::new()));

        #[derive(Clone, Debug)]
        struct Type1 {
            foo: String,
        }

        #[derive(Debug)]
        struct Type2 {
            bar: i32,
        }

        #[derive(Debug)]
        struct Type3;

        assert_eq!(None, map.lock().unwrap().get::<String>());
        map.lock().unwrap().insert("foo".to_string());
        assert_eq!(Some(&"foo".to_string()), map.lock().unwrap().get::<String>());

        assert!(map.lock().unwrap().get::<Type1>().is_none());

        let type1 = Type1 {
            foo: "foo".to_string(),
        };
        map.lock().unwrap().insert(type1.clone());
        assert_eq!("foo", map.lock().unwrap().get::<Type1>().unwrap().foo);
        assert_eq!(
            "foo",
            map.lock()
                .unwrap()
                .get_or_insert::<Type1>(Type1 {
                    foo: "baz".to_string()
                })
                .foo
        );

        assert_eq!(42, map.lock().unwrap().get_or_insert(Type2 { bar: 42 }).bar);
        assert_eq!(42, map.lock().unwrap().get_or_insert(Type2 { bar: 24 }).bar);
        assert_eq!(42, map.lock().unwrap().remove::<Type2>().unwrap().bar);
        assert_eq!(None, map.lock().unwrap().get::<Type2>().map(|t2| t2.bar));
    }
}

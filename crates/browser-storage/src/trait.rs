//! The [`StorageSystem`] trait: `save`, `retrieve`, `base_url`.
//!
//! This is Python's `StorageSystemMixin`, minus the parts that only make sense
//! in Python. The mixin is an `ABC` with two `@abstractmethod`s and one
//! concrete, LRU-memoised `_get_base_url`; here that becomes one object-safe
//! trait whose implementors compute their namespace once and hand it back by
//! reference.
//!
//! # What is deliberately *not* ported
//!
//! The reference implementation requires a custom backend to be decorated with
//! `functools.lru_cache`, and `Selector.__init__` rejects a storage class
//! without a `__wrapped__` attribute:
//!
//! ```python
//! raise ValueError("Storage class must be wrapped with lru_cache decorator, see docs for info")
//! raise ValueError("Storage system must be inherited from class `StorageSystemMixin`")
//! ```
//!
//! That decorator is Python's way of getting a singleton-per-arguments — and a
//! documented footgun, because `lru_cache(1)` evicts (and therefore `__del__`s,
//! closing the database of) the previous instance as soon as a second
//! `(file, url)` pair is constructed. `ARCHITECTURE.md` §8 drops the ritual: a
//! Rust caller hands out an [`Arc<dyn StorageSystem>`](std::sync::Arc) and owns
//! the lifetime explicitly, so neither `ValueError` can occur and the second
//! storage does not silently kill the first.
//!
//! # Implementing a backend
//!
//! The contract, from `docs/development/adaptive_storage_system.md`:
//!
//! * `(base_url, identifier)` is the primary key — a second `save` under the
//!   same pair must overwrite, not append;
//! * `retrieve` returns `Ok(None)`, not an error, when the key is absent;
//! * the implementation must be usable from many threads at once
//!   ([`Send`] + [`Sync`] is a supertrait, and the adaptive layer really does
//!   hammer it from a thread pool).

use browser_core::ElementFingerprint;

use crate::error::StorageError;

/// A pluggable backend for adaptive-selector fingerprints.
///
/// Fingerprints are keyed by `(base_url, identifier)`: the registrable domain
/// of the page they came from (or `"default"`), and whatever the caller passed
/// as the identifier — for `Selector.css(sel, identifier=…)` that is
/// `identifier or sel`, un-hashed.
///
/// The trait is object safe; the adaptive layer stores an
/// `Option<Arc<dyn StorageSystem>>`.
///
/// ```
/// use std::collections::HashMap;
/// use parking_lot::Mutex;
/// use browser_core::ElementFingerprint;
/// use browser_storage::error::StorageError;
/// use browser_storage::r#trait::StorageSystem;
///
/// /// A backend that forgets everything when it is dropped.
/// struct Ephemeral {
///     base_url: String,
///     rows: Mutex<HashMap<String, ElementFingerprint>>,
/// }
///
/// impl StorageSystem for Ephemeral {
///     fn save(&self, fp: &ElementFingerprint, identifier: &str) -> Result<(), StorageError> {
///         self.rows.lock().insert(identifier.to_owned(), fp.clone());
///         Ok(())
///     }
///
///     fn retrieve(&self, identifier: &str) -> Result<Option<ElementFingerprint>, StorageError> {
///         Ok(self.rows.lock().get(identifier).cloned())
///     }
///
///     fn base_url(&self) -> &str {
///         &self.base_url
///     }
/// }
///
/// let storage = Ephemeral {
///     base_url: "example.com".to_owned(),
///     rows: Mutex::new(HashMap::new()),
/// };
/// let fingerprint = ElementFingerprint { tag: "p".into(), ..Default::default() };
/// storage.save(&fingerprint, "p.price").unwrap();
/// assert_eq!(storage.retrieve("p.price").unwrap(), Some(fingerprint));
/// assert_eq!(storage.retrieve("absent").unwrap(), None);
/// ```
pub trait StorageSystem: Send + Sync {
    /// Stores `fingerprint` under `identifier`, replacing any fingerprint
    /// already stored under `(self.base_url(), identifier)`.
    ///
    /// # Errors
    ///
    /// Returns a [`StorageError`] if the backend could not write the row.
    fn save(&self, fingerprint: &ElementFingerprint, identifier: &str) -> Result<(), StorageError>;

    /// Looks up the fingerprint stored under `(self.base_url(), identifier)`.
    ///
    /// Returns `Ok(None)` when nothing was stored under that key — that is the
    /// normal "first visit" path, not an error.
    ///
    /// # Errors
    ///
    /// Returns a [`StorageError`] if the backend could not read the row, or
    /// read one it could not decode.
    fn retrieve(&self, identifier: &str) -> Result<Option<ElementFingerprint>, StorageError>;

    /// The namespace every key of this backend lives in: the registrable
    /// domain of the site, or `"default"`.
    ///
    /// See [`registrable_domain`](crate::base_url::registrable_domain).
    fn base_url(&self) -> &str;
}

impl<T: StorageSystem + ?Sized> StorageSystem for &T {
    fn save(&self, fingerprint: &ElementFingerprint, identifier: &str) -> Result<(), StorageError> {
        (**self).save(fingerprint, identifier)
    }

    fn retrieve(&self, identifier: &str) -> Result<Option<ElementFingerprint>, StorageError> {
        (**self).retrieve(identifier)
    }

    fn base_url(&self) -> &str {
        (**self).base_url()
    }
}

impl<T: StorageSystem + ?Sized> StorageSystem for Box<T> {
    fn save(&self, fingerprint: &ElementFingerprint, identifier: &str) -> Result<(), StorageError> {
        (**self).save(fingerprint, identifier)
    }

    fn retrieve(&self, identifier: &str) -> Result<Option<ElementFingerprint>, StorageError> {
        (**self).retrieve(identifier)
    }

    fn base_url(&self) -> &str {
        (**self).base_url()
    }
}

impl<T: StorageSystem + ?Sized> StorageSystem for std::sync::Arc<T> {
    fn save(&self, fingerprint: &ElementFingerprint, identifier: &str) -> Result<(), StorageError> {
        (**self).save(fingerprint, identifier)
    }

    fn retrieve(&self, identifier: &str) -> Result<Option<ElementFingerprint>, StorageError> {
        (**self).retrieve(identifier)
    }

    fn base_url(&self) -> &str {
        (**self).base_url()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use parking_lot::Mutex;

    use super::*;

    /// The smallest possible backend, used to pin the trait's shape.
    #[derive(Default)]
    struct Recorder {
        rows: Mutex<HashMap<String, ElementFingerprint>>,
    }

    impl StorageSystem for Recorder {
        fn save(
            &self,
            fingerprint: &ElementFingerprint,
            identifier: &str,
        ) -> Result<(), StorageError> {
            self.rows
                .lock()
                .insert(identifier.to_owned(), fingerprint.clone());
            Ok(())
        }

        fn retrieve(&self, identifier: &str) -> Result<Option<ElementFingerprint>, StorageError> {
            Ok(self.rows.lock().get(identifier).cloned())
        }

        fn base_url(&self) -> &str {
            "default"
        }
    }

    fn fingerprint(tag: &str) -> ElementFingerprint {
        ElementFingerprint {
            tag: tag.to_owned(),
            ..ElementFingerprint::default()
        }
    }

    /// Compiles only if the trait is object safe.
    fn erased(storage: Arc<dyn StorageSystem>) -> Arc<dyn StorageSystem> {
        storage
    }

    #[test]
    fn is_object_safe_and_thread_safe() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Arc<dyn StorageSystem>>();

        let storage = erased(Arc::new(Recorder::default()));
        storage.save(&fingerprint("p"), "id").expect("save");
        assert_eq!(
            storage.retrieve("id").expect("retrieve"),
            Some(fingerprint("p"))
        );
        assert_eq!(storage.base_url(), "default");
    }

    #[test]
    fn forwarding_impls_reach_the_inner_backend() {
        let owned = Recorder::default();
        owned.save(&fingerprint("a"), "one").expect("save");

        let by_ref: &dyn StorageSystem = &owned;
        assert_eq!(
            by_ref.retrieve("one").expect("retrieve"),
            Some(fingerprint("a"))
        );

        let boxed: Box<dyn StorageSystem> = Box::new(Recorder::default());
        boxed.save(&fingerprint("b"), "two").expect("save");
        assert_eq!(
            boxed.retrieve("two").expect("retrieve"),
            Some(fingerprint("b"))
        );

        let shared: Arc<dyn StorageSystem> = Arc::new(Recorder::default());
        shared.save(&fingerprint("c"), "three").expect("save");
        assert_eq!(
            shared.retrieve("three").expect("retrieve"),
            Some(fingerprint("c")),
        );
        assert_eq!(shared.retrieve("missing").expect("retrieve"), None);
    }

    #[test]
    fn generic_code_accepts_every_wrapper() {
        fn round_trip<S: StorageSystem>(storage: S) {
            storage.save(&fingerprint("p"), "k").expect("save");
            assert_eq!(
                storage.retrieve("k").expect("retrieve"),
                Some(fingerprint("p"))
            );
        }

        let borrowed = Recorder::default();
        round_trip(Recorder::default());
        round_trip(&borrowed);
        round_trip(Box::new(Recorder::default()));
        round_trip(Arc::new(Recorder::default()));
    }
}

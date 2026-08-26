//! Module identity, imports, and the states a module passes through.
//!
//! A module is identified by content, not by where it came from. Its key is the
//! digest of its canonical source or image bytes, so two modules with the same
//! bytes are the same module however they were fetched, and a module whose
//! bytes change is a different module rather than a new version of the same
//! one.
//!
//! Resolution is a separate step with an explicit result: a specifier is turned
//! into a key by a resolver the deployment supplies, before linking begins.
//! Nothing here fetches anything, and nothing here decides policy; this is the
//! vocabulary those decisions are expressed in.

use crate::digest::Digest;

/// A module's identity: the digest of the exact bytes that define it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Key(pub Digest);

/// What a module's bytes are.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum Form {
    /// Source text, which still has to be compiled.
    Source = 0,
    /// A verified unit image.
    Image = 1,
}

/// Where a module is in its lifecycle.
///
/// The states and their order are the specification's: a module is linked
/// before it is evaluated, and a failure is remembered so a second attempt
/// fails the same way rather than running again.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum Status {
    /// Registered, with nothing resolved yet.
    New = 0,
    /// Its imports are being resolved.
    Linking = 1,
    /// Every import resolved and its closure is complete.
    Linked = 2,
    /// Its body is running.
    Evaluating = 3,
    /// Its body finished.
    Evaluated = 4,
    /// Linking or evaluation failed, and the failure is remembered.
    Failed = 5,
}

impl Status {
    /// Whether the lifecycle admits this transition.
    pub const fn admits(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::New, Self::Linking)
                | (Self::Linking, Self::Linked)
                | (Self::Linking, Self::Failed)
                | (Self::Linked, Self::Evaluating)
                | (Self::Linked, Self::Failed)
                | (Self::Evaluating, Self::Evaluated)
                | (Self::Evaluating, Self::Failed)
        )
    }
}

/// Why a module could not be admitted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Rejection {
    /// The specifier resolved to nothing.
    Unresolved,
    /// The resolved bytes do not have the key they were registered under.
    DigestMismatch,
    /// Two different modules were registered under one key.
    DuplicateKey,
    /// The closure is larger than the admitted maximum.
    ClosureTooLarge,
    /// A module imports something the closure does not contain.
    MissingDependency,
    /// An import attribute the deployment does not admit.
    UnsupportedAttribute,
    /// The lifecycle does not admit the operation in this state.
    WrongState,
    /// The registry has no room.
    RegistryFull,
}

/// One import: the specifier as written, and what it resolved to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Import {
    /// Where the specifier text sits in the importing module's source.
    pub specifier_start: u32,
    pub specifier_end: u32,
    /// The module it names, once resolved.
    pub resolved: Option<Key>,
}

/// One registered module.
#[derive(Clone, Copy, Debug)]
pub struct Record {
    pub key: Key,
    pub form: Form,
    pub status: Status,
    /// Imports, as a range into the registry's import table.
    pub imports_at: u32,
    pub import_count: u32,
    /// Position in the evaluation order, once the closure is ordered.
    pub order: u32,
}

/// The largest module closure this build admits.
pub const MAX_CLOSURE: usize = 64;

/// A registry of modules and their imports, in caller-provided storage.
///
/// The registry is the whole of a link: it holds every module of a closure, the
/// imports that connect them, and the order they evaluate in. It fetches
/// nothing: a caller registers bytes it already has.
pub struct Registry<'a> {
    records: &'a mut [Record],
    count: usize,
    imports: &'a mut [Import],
    import_count: usize,
}

impl<'a> Registry<'a> {
    pub fn new(records: &'a mut [Record], imports: &'a mut [Import]) -> Self {
        Self {
            records,
            count: 0,
            imports,
            import_count: 0,
        }
    }

    pub const fn count(&self) -> usize {
        self.count
    }

    /// The registered modules.
    pub fn records(&self) -> &[Record] {
        match self.records.get(..self.count) {
            Some(slice) => slice,
            None => &[],
        }
    }

    /// The index of a module, by key.
    pub fn index_of(&self, key: Key) -> Option<usize> {
        let mut index = 0usize;
        while index < self.count {
            if self
                .records
                .get(index)
                .is_some_and(|record| record.key == key)
            {
                return Some(index);
            }
            index += 1;
        }
        None
    }

    /// Register a module under the key its bytes produce.
    ///
    /// Registering the same key twice with the same form is idempotent, which
    /// is what makes a diamond in a closure harmless. Registering a different
    /// form under an existing key is a rejection, because a key is content.
    pub fn register(&mut self, key: Key, form: Form) -> Result<usize, Rejection> {
        if let Some(index) = self.index_of(key) {
            let existing = self.records.get(index).ok_or(Rejection::RegistryFull)?;
            if existing.form != form {
                return Err(Rejection::DuplicateKey);
            }
            return Ok(index);
        }
        if self.count >= MAX_CLOSURE {
            return Err(Rejection::ClosureTooLarge);
        }
        let slot = self
            .records
            .get_mut(self.count)
            .ok_or(Rejection::RegistryFull)?;
        *slot = Record {
            key,
            form,
            status: Status::New,
            imports_at: u32::try_from(self.import_count).unwrap_or(0),
            import_count: 0,
            order: u32::MAX,
        };
        let index = self.count;
        self.count += 1;
        Ok(index)
    }

    /// Add an import to the module registered last.
    ///
    /// Imports are added in source order, immediately after their module is
    /// registered, which is what keeps them contiguous without a second table.
    pub fn add_import(
        &mut self,
        module: usize,
        specifier_start: u32,
        specifier_end: u32,
    ) -> Result<(), Rejection> {
        if module + 1 != self.count {
            return Err(Rejection::WrongState);
        }
        let slot = self
            .imports
            .get_mut(self.import_count)
            .ok_or(Rejection::RegistryFull)?;
        *slot = Import {
            specifier_start,
            specifier_end,
            resolved: None,
        };
        self.import_count += 1;
        let record = self
            .records
            .get_mut(module)
            .ok_or(Rejection::RegistryFull)?;
        record.import_count += 1;
        Ok(())
    }

    /// The imports of a module.
    pub fn imports(&self, module: usize) -> &[Import] {
        let Some(record) = self.records.get(module) else {
            return &[];
        };
        let start = record.imports_at as usize;
        let end = start + record.import_count as usize;
        match self.imports.get(start..end) {
            Some(slice) => slice,
            None => &[],
        }
    }

    /// Record what an import resolved to.
    pub fn resolve(&mut self, module: usize, import: usize, key: Key) -> Result<(), Rejection> {
        let record = *self.records.get(module).ok_or(Rejection::WrongState)?;
        if import >= record.import_count as usize {
            return Err(Rejection::WrongState);
        }
        if self.index_of(key).is_none() {
            return Err(Rejection::MissingDependency);
        }
        let at = record.imports_at as usize + import;
        let slot = self.imports.get_mut(at).ok_or(Rejection::WrongState)?;
        slot.resolved = Some(key);
        Ok(())
    }

    /// Move a module to a new status, if the lifecycle admits it.
    pub fn set_status(&mut self, module: usize, status: Status) -> Result<(), Rejection> {
        let record = self.records.get_mut(module).ok_or(Rejection::WrongState)?;
        if !record.status.admits(status) {
            return Err(Rejection::WrongState);
        }
        record.status = status;
        Ok(())
    }

    pub fn status(&self, module: usize) -> Option<Status> {
        self.records.get(module).map(|record| record.status)
    }

    /// The digest of the whole closure: every module's key in evaluation order.
    ///
    /// This is what identifies a linked program, and it changes when any module
    /// in it changes.
    pub fn closure_digest(&self) -> Digest {
        let mut hasher = crate::digest::Hasher::new();
        let mut position = 0u32;
        while position < u32::try_from(self.count).unwrap_or(0) {
            let mut index = 0usize;
            while index < self.count {
                if let Some(record) = self.records.get(index) {
                    if record.order == position {
                        hasher.update(&record.key.0 .0);
                        hasher.update(&[record.form as u8]);
                    }
                }
                index += 1;
            }
            position += 1;
        }
        hasher.finish()
    }

    /// Set a module's position in the evaluation order.
    pub fn set_order(&mut self, module: usize, order: u32) -> Result<(), Rejection> {
        let record = self.records.get_mut(module).ok_or(Rejection::WrongState)?;
        record.order = order;
        Ok(())
    }

    pub fn order(&self, module: usize) -> Option<u32> {
        self.records.get(module).map(|record| record.order)
    }
}

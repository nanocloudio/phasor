//! Modules: instances, namespaces and deferred namespaces, instantiation and
//! evaluation order, and dynamic import.

use super::*;

/// The names a namespace object carries, sorted as they are gathered: each
/// with its length, the slot and module it lands in, whether the module's
/// own export claims it, whether two stars made it ambiguous, and where it
/// finally binds.
pub(super) struct NamespaceNames {
    pub(super) names: [[u16; 64]; 64],
    pub(super) lengths: [usize; 64],
    pub(super) slots: [u32; 64],
    pub(super) sources: [u32; 64],
    pub(super) owns: [bool; 64],
    pub(super) dead: [bool; 64],
    pub(super) final_units: [u32; 64],
    pub(super) final_slots: [u32; 64],
}

impl NamespaceNames {
    pub(super) const EMPTY: Self = Self {
        names: [[0u16; 64]; 64],
        lengths: [0usize; 64],
        slots: [0u32; 64],
        sources: [0u32; 64],
        owns: [false; 64],
        dead: [false; 64],
        final_units: [0u32; 64],
        final_slots: [0u32; 64],
    };
}

impl<'a, 'u, 'h, 'atoms> Vm<'a, 'u, 'h, 'atoms> {
    /// Run a linked closure rather than one unit: the units are the closure's
    /// modules, in evaluation order, and the table says where each module's
    /// resolved imports start.
    pub fn attach_modules(
        &mut self,
        units: &'a [Unit<'u>],
        modules: &'a mut [ModuleInstance],
        imports: &'a [(u32, u32)],
    ) {
        self.units = units;
        self.deferred_live = modules
            .iter()
            .any(|instance| instance.deferred_namespace.is_object());
        self.modules = Some(modules);
        self.imports = Some(imports);
    }

    /// Attach the specifier names dynamic imports resolve against.
    pub fn attach_module_names(&mut self, names: &'a [([u8; 128], usize, u32)]) {
        self.module_names = names;
    }

    /// Attach the cycle pairs whose members share evaluation errors.
    pub fn attach_module_cycles(&mut self, cycles: &'a [(u32, u32)]) {
        self.module_cycles = cycles;
    }

    /// Mark a module errored, its cycle with it: the specification records
    /// one [[EvaluationError]] for a whole strongly-connected component.
    pub(super) fn poison_cycle(&mut self, module: u32, error: Value) {
        self.set_module_status(module, 3);
        if let Some(modules) = self.modules.as_deref_mut() {
            if let Some(instance) = modules.get_mut(module as usize) {
                instance.completion = error;
            }
        }
        let mut index = 0usize;
        while index < self.module_cycles.len() {
            let (one, two) = self.module_cycles[index];
            let partner = if one == module {
                Some(two)
            } else if two == module {
                Some(one)
            } else {
                None
            };
            if let Some(partner) = partner {
                if self.module_status(partner) != 3 {
                    self.set_module_status(partner, 3);
                    if let Some(modules) = self.modules.as_deref_mut() {
                        if let Some(instance) = modules.get_mut(partner as usize) {
                            instance.completion = error;
                        }
                    }
                }
            }
            index += 1;
        }
    }

    /// The module the running frame belongs to.
    pub(super) fn current_module(&self) -> u32 {
        match self.frames.get(self.depth.saturating_sub(1) as usize) {
            Some(frame) if self.depth > 0 => frame.module,
            _ => self.entry_module,
        }
    }

    /// The unit the running frame belongs to.
    pub(super) fn unit(&self) -> &Unit<'u> {
        self.unit_of(self.current_module())
    }

    /// The value an import names: the slot it resolved to, in the environment
    /// of the module that exports it.
    ///
    /// The read goes through the exporting module every time, so an import sees
    /// what that module holds now rather than what it held when the importing
    /// module ran.
    pub(super) fn import_value(&mut self, module: u32, index: u32) -> Result<Value, Completion> {
        let base = match &self.modules {
            Some(modules) => modules
                .get(module as usize)
                .map_or(u32::MAX, |instance| instance.import_base),
            None => u32::MAX,
        };
        if base == u32::MAX {
            return Err(Completion::MALFORMED);
        }
        let Some((source, slot)) = self
            .imports
            .as_ref()
            .and_then(|imports| imports.get((base + index) as usize).copied())
        else {
            return Err(Completion::MALFORMED);
        };
        if slot == u32::MAX {
            // `import * as name` names the module itself.
            return self.namespace_of(source);
        }
        if slot == crate::bytecode::DEFER_IMPORT_NAME {
            // `import defer * as name` names it too, evaluation withheld.
            return self.deferred_namespace_of(source);
        }
        if slot == crate::bytecode::POISON_IMPORT {
            // The linker could not resolve this name; reading it is the
            // SyntaxError the linking would have raised.
            return Err(self.throw_error_of(ErrorKind::Syntax));
        }
        if slot == crate::bytecode::HOST_POISON_IMPORT {
            // A phase the host does not serve: its refusal, a TypeError.
            return Err(self.throw_type_error());
        }
        let environment = self.module_environment(source);
        if !environment.is_object() {
            return Err(self.throw_reference_error());
        }
        match env::slot_value(self.heap, environment.as_handle(), slot) {
            Ok(value) => Ok(value),
            // A slot not yet written is a binding still in its dead zone:
            // the module that owns it has not reached its declaration.
            Err(env::EnvironmentError::Uninitialised | env::EnvironmentError::Unresolvable) => {
                Err(self.throw_reference_error())
            }
            Err(_) => Err(Completion::MALFORMED),
        }
    }

    /// The object that names a module's exports.
    ///
    /// Each of its properties reads the module's slot when it is read, so a
    /// namespace shows what the module holds now rather than what it held when
    /// the namespace was made.
    pub(super) fn namespace_of(&mut self, module: u32) -> Result<Value, Completion> {
        self.namespace_object(module, false)
    }

    /// The deferred twin of a module's namespace: one distinct object per
    /// module, shaped exactly as the namespace is, whose meaningful use
    /// evaluates the module first.
    pub(super) fn deferred_namespace_of(&mut self, module: u32) -> Result<Value, Completion> {
        self.namespace_object(module, true)
    }

    pub(super) fn namespace_object(
        &mut self,
        module: u32,
        deferred: bool,
    ) -> Result<Value, Completion> {
        if let Some(modules) = &self.modules {
            if let Some(instance) = modules.get(module as usize) {
                let held = if deferred {
                    instance.deferred_namespace
                } else {
                    instance.namespace
                };
                if held.is_object() {
                    return Ok(held);
                }
            }
        }
        // A namespace has no prototype, takes nothing new, and lists its
        // names in code unit order, the way the specification sorts them.
        let object =
            object::create(self.heap, Value::NULL).map_err(|_| Completion::HEAP_EXHAUSTED)?;
        let namespace = Value::object(object);
        // The module's own exports come first; whatever its `export * from`
        // records merge in follows, without `default`, and a name the list
        // already holds keeps its first source.
        let mut names = NamespaceNames::EMPTY;
        let held = self.collect_namespace_names(module, &mut names);
        let NamespaceNames {
            names,
            lengths,
            slots,
            sources,
            dead,
            ..
        } = names;
        let mut index = 0usize;
        while index < held {
            if dead.get(index).copied().unwrap_or(false) {
                index += 1;
                continue;
            }
            let record = crate::bytecode::ExportRecord {
                name: 0,
                slot: slots.get(index).copied().unwrap_or(0),
            };
            let units = names[index];
            let length = lengths[index];
            let source = sources.get(index).copied().unwrap_or(module);
            let name = self.make_string(units.get(..length).unwrap_or(&[]))?;
            let key = self.coerce_to_key(name)?;
            let getter = object::create_native(
                self.heap,
                Value::object(self.realm.function_prototype),
                native::NAMESPACE_GET,
                0,
            )
            .map_err(|_| Completion::HEAP_EXHAUSTED)?;
            let binding = self.new_array()?;
            self.set_element(
                binding,
                0,
                Value::number(crate::softfloat::from_u64(u64::from(source))),
            )?;
            self.set_element(
                binding,
                1,
                Value::number(crate::softfloat::from_u64(u64::from(record.slot))),
            )?;
            let _ = &record;
            if deferred {
                // The getter of a deferred namespace knows it, and knows
                // `then`, which reads as undefined while the module waits.
                let is_then = units.get(..length)
                    == Some(&[
                        u16::from(b't'),
                        u16::from(b'h'),
                        u16::from(b'e'),
                        u16::from(b'n'),
                    ]);
                let flags = 1u64 | if is_then { 2 } else { 0 };
                self.set_element(binding, 2, Value::number(crate::softfloat::from_u64(flags)))?;
                self.set_length(binding, 3)?;
            } else {
                self.set_length(binding, 2)?;
            }
            object::set_bound_value(self.heap, getter, binding)
                .map_err(|_| Completion::HEAP_EXHAUSTED)?;
            object::define_own_property(
                self.heap,
                object,
                key,
                Descriptor::accessor(
                    Value::object(getter),
                    Value::UNDEFINED,
                    attribute::ENUMERABLE,
                ),
            )
            .map_err(|_| Completion::HEAP_EXHAUSTED)?;
            index += 1;
        }
        let tag = if deferred {
            self.ascii_string(b"Deferred Module")?
        } else {
            self.ascii_string(b"Module")?
        };
        object::define_own_property(
            self.heap,
            object,
            Key::Symbol(self.realm.to_string_tag_symbol),
            Descriptor::data(tag, 0),
        )
        .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        let kind = if deferred {
            object::exotic::DEFERRED
        } else {
            object::exotic::NAMESPACE
        };
        object::set_exotic_kind(self.heap, object, kind).map_err(|_| Completion::MALFORMED)?;
        let _ = object::prevent_extensions(self.heap, object);
        if let Some(modules) = self.modules.as_deref_mut() {
            if let Some(instance) = modules.get_mut(module as usize) {
                if deferred {
                    instance.deferred_namespace = namespace;
                } else {
                    instance.namespace = namespace;
                }
            }
        }
        if deferred {
            self.deferred_live = true;
        }
        Ok(namespace)
    }

    /// Walk a prototype chain up to where a key would be found, running the
    /// deferred trigger on any deferred namespace passed on the way — a
    /// read or an `in` consults its [[Get]] or [[HasProperty]] even from a
    /// chain, and that consultation is a meaningful use.
    pub(super) fn deferred_chain_trigger(
        &mut self,
        target: Value,
        key: Key,
    ) -> Result<(), Completion> {
        let mut holder = target;
        let mut depth = 0u32;
        while holder.is_object() && depth <= object::MAX_PROTOTYPE_DEPTH {
            if object::exotic_kind(self.heap, holder.as_handle()).unwrap_or(0)
                == object::exotic::DEFERRED
            {
                self.deferred_trigger(holder, Some(key))?;
            }
            let found = object::get_own_property(self.heap, holder.as_handle(), key)
                .unwrap_or(None)
                .is_some();
            if found {
                break;
            }
            holder = object::prototype(self.heap, holder.as_handle())
                .map_err(|_| Completion::MALFORMED)?;
            depth += 1;
        }
        Ok(())
    }

    /// Which module a deferred namespace names, found by the object itself.
    pub(super) fn module_of_deferred(&self, target: Value) -> Option<u32> {
        let modules = self.modules.as_ref()?;
        let mut index = 0usize;
        while index < modules.len() {
            let held = modules.get(index)?.deferred_namespace;
            if held.is_object() && held.as_handle() == target.as_handle() {
                return u32::try_from(index).ok();
            }
            index += 1;
        }
        None
    }

    /// How far a module has run: untouched, running, or done. A module
    /// whose completion promise has fulfilled is done the moment it does,
    /// however the field lags — an access inside the settling job sees it.
    pub fn module_status(&self, module: u32) -> u8 {
        let held = match &self.modules {
            Some(modules) => modules.get(module as usize).copied(),
            None => None,
        };
        let Some(instance) = held else {
            return 2;
        };
        if instance.evaluated == 1
            && instance.completion.is_object()
            && object::promise_state(self.heap, instance.completion.as_handle())
                == Ok(promise::FULFILLED)
        {
            return 2;
        }
        instance.evaluated
    }

    /// How far a module has run, told from outside: the loader marks the
    /// whole eager set evaluating before the first body, done as each
    /// settles — which is what a deferred access checks against.
    pub fn set_module_status(&mut self, module: u32, status: u8) {
        if let Some(modules) = self.modules.as_deref_mut() {
            if let Some(instance) = modules.get_mut(module as usize) {
                instance.evaluated = status;
            }
        }
    }

    /// What a meaningful use of a deferred namespace does: run the module.
    /// A symbol key never counts, nor does `then`, which promise resolution
    /// probes without meaning to use the module; no key at all — a key
    /// listing — counts. Using a namespace whose own module is still mid
    /// evaluation is the error the specification names.
    pub(super) fn deferred_trigger(
        &mut self,
        target: Value,
        key: Option<Key>,
    ) -> Result<(), Completion> {
        if !target.is_object()
            || object::exotic_kind(self.heap, target.as_handle()).unwrap_or(0)
                != object::exotic::DEFERRED
        {
            return Ok(());
        }
        match key {
            Some(Key::Symbol(_)) => return Ok(()),
            Some(key) => {
                let then = self.ascii_key(b"then")?;
                if key == then {
                    return Ok(());
                }
            }
            None => {}
        }
        let Some(module) = self.module_of_deferred(target) else {
            return Ok(());
        };
        match self.module_status(module) {
            2 => Ok(()),
            6 => Err(self.throw_error_of(ErrorKind::Syntax)),
            1 | 4 => Err(self.throw_type_error()),
            3 => {
                // An errored module answers every later use with the very
                // error its evaluation threw.
                let held = self.module_completion(module);
                Err(Completion::Throw(held))
            }
            _ => {
                // Nothing runs unless the whole subgraph is ready: a
                // dependency someone else is mid-evaluating refuses the
                // trigger before any body runs.
                self.deferred_ready(module, true)?;
                self.evaluate_module_now(module, true)
            }
        }
    }

    /// Whether a deferred module's whole subgraph can run now, checked
    /// without running anything.
    pub(super) fn deferred_ready(&mut self, module: u32, strict: bool) -> Result<(), Completion> {
        let mut seen = [u32::MAX; MAX_UNIT_REALMS];
        let mut count = 0usize;
        // Loading comes before linking: a module the host refused to load
        // anywhere in the graph rejects with the host's error, before any
        // name resolution gets to raise its SyntaxError.
        let hosts = self.scan_hosts(module, &mut seen, &mut count);
        let mut index = 0usize;
        while index < count {
            let held = seen[index];
            if held != u32::MAX && self.module_status(held) == 5 {
                self.set_module_status(held, 0);
            }
            index += 1;
        }
        hosts?;
        let mut seen = [u32::MAX; MAX_UNIT_REALMS];
        let mut count = 0usize;
        let outcome = self.scan_ready(module, &mut seen, &mut count, false, strict);
        let mut index = 0usize;
        while index < count {
            let held = seen[index];
            if held != u32::MAX && self.module_status(held) == 5 {
                self.set_module_status(held, 0);
            }
            index += 1;
        }
        outcome
    }

    /// Whether any module the graph loads was refused by the host, every
    /// edge followed, deferred ones included.
    pub(super) fn scan_hosts(
        &mut self,
        module: u32,
        seen: &mut [u32; MAX_UNIT_REALMS],
        count: &mut usize,
    ) -> Result<(), Completion> {
        if *count >= seen.len() {
            return Err(Completion::MALFORMED);
        }
        seen[*count] = module;
        *count += 1;
        self.set_module_status(module, 5);
        let base = match &self.modules {
            Some(modules) => modules
                .get(module as usize)
                .map_or(u32::MAX, |instance| instance.import_base),
            None => u32::MAX,
        };
        if base == u32::MAX {
            return Ok(());
        }
        let held = self.unit_of(module).header().import_count;
        let mut import = 0u32;
        while import < held {
            let row = self
                .imports
                .as_ref()
                .and_then(|imports| imports.get((base + import) as usize))
                .copied();
            if let Some((source, slot)) = row {
                if slot == crate::bytecode::HOST_POISON_IMPORT {
                    return Err(self.throw_type_error());
                }
                if source != module && self.module_status(source) == 0 {
                    self.scan_hosts(source, seen, count)?;
                }
            }
            import += 1;
        }
        Ok(())
    }

    pub(super) fn scan_ready(
        &mut self,
        module: u32,
        seen: &mut [u32; MAX_UNIT_REALMS],
        count: &mut usize,
        poison_only: bool,
        strict: bool,
    ) -> Result<(), Completion> {
        if *count >= seen.len() {
            return Err(Completion::MALFORMED);
        }
        seen[*count] = module;
        *count += 1;
        self.set_module_status(module, 5);
        let base = match &self.modules {
            Some(modules) => modules
                .get(module as usize)
                .map_or(u32::MAX, |instance| instance.import_base),
            None => u32::MAX,
        };
        if base == u32::MAX {
            return Ok(());
        }
        let held = self.unit_of(module).header().import_count;
        let mut import = 0u32;
        while import < held {
            let row = self
                .imports
                .as_ref()
                .and_then(|imports| imports.get((base + import) as usize))
                .copied();
            if let Some((source, slot)) = row {
                if slot == crate::bytecode::POISON_IMPORT {
                    // A row linking refused: the SyntaxError it earned.
                    return Err(self.throw_error_of(ErrorKind::Syntax));
                }
                if slot == crate::bytecode::HOST_POISON_IMPORT {
                    return Err(self.throw_type_error());
                }
                if source != module {
                    if slot == crate::bytecode::DEFER_IMPORT_NAME {
                        // What stays deferred stays out of the run — but
                        // linking was eager: a poisoned name anywhere in the
                        // deferred graph is the SyntaxError it earned, an
                        // async module pre-evaluated on this edge's behalf
                        // must have settled, and its error is the answer.
                        // Only an async target was pre-evaluated on this
                        // edge's behalf; a sync deferred module's fate is
                        // its trigger's business, not this import's.
                        let entry_function = self.unit_of(source).header().entry_function;
                        let flags = self
                            .unit_of(source)
                            .function(entry_function)
                            .map_or(0, |held| held.flags);
                        if !poison_only && flags & crate::bytecode::function_flag::ASYNC != 0 {
                            match self.module_status(source) {
                                1 => return Err(self.throw_type_error()),
                                3 => {
                                    let error = self.module_completion(source);
                                    return Err(Completion::Throw(error));
                                }
                                _ => {}
                            }
                        }
                        match self.module_status(source) {
                            6 => return Err(self.throw_error_of(ErrorKind::Syntax)),
                            0 => self.scan_ready(source, seen, count, true, strict)?,
                            _ => {}
                        }
                    } else if poison_only {
                        match self.module_status(source) {
                            6 => return Err(self.throw_error_of(ErrorKind::Syntax)),
                            0 => self.scan_ready(source, seen, count, true, strict)?,
                            _ => {}
                        }
                    } else {
                        match self.module_status(source) {
                            0 => self.scan_ready(source, seen, count, false, strict)?,
                            // A dependency mid-evaluation refuses a trigger;
                            // an import simply waits it out.
                            1 if strict => return Err(self.throw_type_error()),
                            6 => return Err(self.throw_error_of(ErrorKind::Syntax)),
                            3 => {
                                let error = self.module_completion(source);
                                return Err(Completion::Throw(error));
                            }
                            _ => {}
                        }
                    }
                }
            }
            import += 1;
        }
        Ok(())
    }

    /// Whether a deferred namespace's module has already run to its end.
    pub(super) fn deferred_done(&self, target: Value) -> bool {
        self.module_of_deferred(target)
            .is_none_or(|module| self.module_status(module) >= 2)
    }

    /// Run just a module's instantiation: its bindings exist afterwards,
    /// its function declarations hold closures, and nothing else has run —
    /// which is what lets a cycle call across itself before bodies start.
    pub fn instantiate_module(&mut self, module: u32) -> Result<(), Completion> {
        if self.nested >= MAX_NESTED_ENTRIES {
            return Err(Completion::Terminated(Termination::StackOverflow));
        }
        let entry = self.unit_of(module).header().entry_function;
        let environment = self.module_environment(module);
        if !environment.is_object() {
            return Err(Completion::MALFORMED);
        }
        self.push_frame(
            entry,
            environment,
            Value::UNDEFINED,
            Value::UNDEFINED,
            module,
        )?;
        let held = self.instantiating;
        self.instantiating = true;
        self.nested += 1;
        let completion = self.execute();
        self.nested -= 1;
        self.instantiating = held;
        match completion {
            Completion::Value(_) => Ok(()),
            other => Err(other),
        }
    }

    /// The completion an unsettled async module behind one of this
    /// module's deferred edges will answer — the gate its evaluation waits
    /// behind — or undefined when nothing gates it.
    pub(super) fn pending_defer_gate(&mut self, module: u32) -> Value {
        let mut seen = [u32::MAX; MAX_UNIT_REALMS];
        let mut count = 0usize;
        self.defer_gate_scan(module, &mut seen, &mut count, false)
    }

    pub(super) fn defer_gate_scan(
        &mut self,
        module: u32,
        seen: &mut [u32; MAX_UNIT_REALMS],
        count: &mut usize,
        behind_defer: bool,
    ) -> Value {
        let mut at = 0usize;
        while at < *count {
            if seen[at] == module {
                return Value::UNDEFINED;
            }
            at += 1;
        }
        if *count >= seen.len() {
            return Value::UNDEFINED;
        }
        seen[*count] = module;
        *count += 1;
        if behind_defer {
            let entry = self.unit_of(module).header().entry_function;
            let flags = self
                .unit_of(module)
                .function(entry)
                .map_or(0, |held| held.flags);
            if flags & crate::bytecode::function_flag::ASYNC != 0 {
                let completion = self.module_completion(module);
                let unsettled = self.module_status(module) == 1
                    && (!completion.is_object()
                        || object::promise_state(self.heap, completion.as_handle())
                            == Ok(promise::PENDING));
                // Mid-body before its first await there is nothing
                // concrete to gate on; a suspended one hands over its
                // completion.
                if unsettled && completion.is_object() {
                    return completion;
                }
            }
        }
        let base = match &self.modules {
            Some(modules) => modules
                .get(module as usize)
                .map_or(u32::MAX, |instance| instance.import_base),
            None => u32::MAX,
        };
        if base == u32::MAX {
            return Value::UNDEFINED;
        }
        let held = self.unit_of(module).header().import_count;
        let mut import = 0u32;
        while import < held {
            let row = self
                .imports
                .as_ref()
                .and_then(|imports| imports.get((base + import) as usize))
                .copied();
            if let Some((source, slot)) = row {
                if source != module {
                    let crossing = behind_defer || slot == crate::bytecode::DEFER_IMPORT_NAME;
                    let found = self.defer_gate_scan(source, seen, count, crossing);
                    if found.is_object() {
                        return found;
                    }
                }
            }
            import += 1;
        }
        Value::UNDEFINED
    }

    /// Run a deferred module on this loop, its unevaluated dependencies
    /// first, depth first in import order. A dependency found mid
    /// evaluation is a cycle, left to finish on its own.
    pub(super) fn evaluate_module_now(
        &mut self,
        module: u32,
        strict: bool,
    ) -> Result<(), Completion> {
        let mut blocked = Value::UNDEFINED;
        self.evaluate_module_gated(module, strict, &mut blocked)
    }

    /// Like `evaluate_module_now`, but a lenient evaluation skips any
    /// dependency gated behind an unsettled async-deferred subgraph — and
    /// its own body with it — handing the gate back for the caller to
    /// wait on and try again.
    pub(super) fn evaluate_module_gated(
        &mut self,
        module: u32,
        strict: bool,
        blocked: &mut Value,
    ) -> Result<(), Completion> {
        self.set_module_status(module, 4);
        let base = match &self.modules {
            Some(modules) => modules
                .get(module as usize)
                .map_or(u32::MAX, |instance| instance.import_base),
            None => u32::MAX,
        };
        if base != u32::MAX {
            let count = self.unit_of(module).header().import_count;
            let mut import = 0u32;
            while import < count {
                let row = self
                    .imports
                    .as_ref()
                    .and_then(|imports| imports.get((base + import) as usize))
                    .copied();
                if let Some((source, slot)) = row {
                    if slot == crate::bytecode::POISON_IMPORT {
                        return Err(self.throw_error_of(ErrorKind::Syntax));
                    }
                    if slot == crate::bytecode::HOST_POISON_IMPORT {
                        return Err(self.throw_type_error());
                    }
                    // What this module itself defers stays deferred; a
                    // dependency someone else is mid-evaluating is not
                    // usable yet, and one that threw answers its error.
                    if source != module && slot != crate::bytecode::DEFER_IMPORT_NAME {
                        match self.module_status(source) {
                            0 => {
                                // A dependency gated behind an unsettled
                                // async-deferred subgraph waits its turn;
                                // its siblings run meanwhile.
                                if !strict {
                                    let gate = self.pending_defer_gate(source);
                                    if gate.is_object() {
                                        *blocked = gate;
                                        import += 1;
                                        continue;
                                    }
                                }
                                // A sibling's gate is not this dependency's:
                                // it runs against a fresh gate, and only a
                                // gate of its own holds this body back too.
                                let mut inner = Value::UNDEFINED;
                                self.evaluate_module_gated(source, strict, &mut inner)?;
                                if inner.is_object() {
                                    *blocked = inner;
                                }
                            }
                            1 if strict => return Err(self.throw_type_error()),
                            6 => return Err(self.throw_error_of(ErrorKind::Syntax)),
                            3 => {
                                let held = self.module_completion(source);
                                return Err(Completion::Throw(held));
                            }
                            _ => {}
                        }
                    }
                }
                import += 1;
            }
        }
        if blocked.is_object() {
            // A dependency waits behind a gate: so does this body, its
            // status handed back for the retry to find untouched.
            self.set_module_status(module, 0);
            return Ok(());
        }
        if self.nested >= MAX_NESTED_ENTRIES {
            return Err(Completion::Terminated(Termination::StackOverflow));
        }
        let entry = self.unit_of(module).header().entry_function;
        let environment = self.module_environment(module);
        if !environment.is_object() {
            return Err(Completion::MALFORMED);
        }
        self.push_frame(
            entry,
            environment,
            Value::UNDEFINED,
            Value::UNDEFINED,
            module,
        )?;
        let resume = match &self.modules {
            Some(modules) => modules
                .get(module as usize)
                .map_or(0, |instance| instance.body_pc),
            None => 0,
        };
        if resume != 0 {
            self.frames[self.depth as usize - 1].pc = resume;
        }
        let before = self.fuel;
        self.nested += 1;
        let completion = self.execute();
        self.nested -= 1;
        let spent = before.saturating_sub(self.fuel);
        self.slice = self.slice.saturating_sub(spent);
        match completion {
            Completion::Value(value) => {
                // A body that answered a pending promise is still running;
                // its completion is kept for whoever waits on it.
                if value.is_object()
                    && object::is_promise(self.heap, value.as_handle()).unwrap_or(false)
                    && object::promise_state(self.heap, value.as_handle()) != Ok(promise::FULFILLED)
                {
                    if let Some(modules) = self.modules.as_deref_mut() {
                        if let Some(instance) = modules.get_mut(module as usize) {
                            instance.completion = value;
                            instance.evaluated = 1;
                        }
                    }
                    return Ok(());
                }
                self.set_module_status(module, 2);
                Ok(())
            }
            Completion::Throw(error) => {
                self.poison_cycle(module, error);
                Err(Completion::Throw(error))
            }
            other => {
                self.set_module_status(module, 2);
                Err(other)
            }
        }
    }

    /// Surface what a namespace binding holds: reflection over a namespace
    /// reads the binding, so a name still in its dead zone throws here as
    /// a direct read would.
    pub(super) fn namespace_touch(&mut self, object: Value, key: Key) -> Result<(), Completion> {
        self.deferred_trigger(object, Some(key))?;
        if object.is_object()
            && matches!(
                object::exotic_kind(self.heap, object.as_handle()).unwrap_or(0),
                object::exotic::NAMESPACE | object::exotic::DEFERRED
            )
            && !matches!(key, Key::Symbol(_))
            && object::get_own_property(self.heap, object.as_handle(), key)
                .unwrap_or(None)
                .is_some()
        {
            self.get_property(object, key)?;
        }
        Ok(())
    }

    /// A module's environment, which holds its top-level bindings.
    pub(super) fn module_environment(&self, module: u32) -> Value {
        match &self.modules {
            Some(modules) => modules
                .get(module as usize)
                .map_or(Value::UNDEFINED, |instance| instance.environment),
            None => Value::UNDEFINED,
        }
    }

    /// Make the environment a module's top-level bindings live in.
    ///
    /// A module's environment is made before it runs and given to it, which is
    /// what lets another module read its exports once it has.
    pub fn create_module_environment(&mut self, module: u32) -> Result<Value, Completion> {
        let unit = self.unit_of(module);
        let entry = unit.header().entry_function;
        let slots = unit
            .function(entry)
            .map_or(0, |function| function.context_slots);
        let record = env::create(
            self.heap,
            EnvironmentKind::Declarative,
            Value::object(self.realm.lexical),
            slots,
        )
        .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        self.declare_slots(record, slots)?;
        Ok(Value::object(record))
    }

    /// What a module exports under a name, for a host reading a result out of
    /// a closure it evaluated.
    pub fn module_export(&mut self, module: u32, name: &[u16]) -> Option<Value> {
        let slot = self.unit_of(module).export_slot(name)?;
        let environment = self.module_environment(module);
        if !environment.is_object() {
            return None;
        }
        env::slot_value(self.heap, environment.as_handle(), slot).ok()
    }

    /// Keep what evaluating a module answered, for a dependant to wait on.
    pub fn set_module_completion(&mut self, module: u32, completion: Value) {
        if let Some(modules) = self.modules.as_deref_mut() {
            if let Some(instance) = modules.get_mut(module as usize) {
                instance.completion = completion;
            }
        }
    }

    /// What evaluating a module answered, or undefined before it ran.
    pub fn module_completion(&self, module: u32) -> Value {
        match &self.modules {
            Some(modules) => modules
                .get(module as usize)
                .map_or(Value::UNDEFINED, |instance| instance.completion),
            None => Value::UNDEFINED,
        }
    }

    /// Give a module the environment its bindings live in.
    pub fn set_module_environment(&mut self, module: u32, environment: Value) {
        if let Some(modules) = self.modules.as_deref_mut() {
            if let Some(instance) = modules.get_mut(module as usize) {
                instance.environment = environment;
            }
        }
    }

    /// Begin one module of a linked closure, in the environment it was given.
    pub fn start_module(&mut self, module: u32) -> Result<(), Completion> {
        self.entry_module = module;
        if let Some(modules) = self.modules.as_deref_mut() {
            if let Some(instance) = modules.get_mut(module as usize) {
                instance.evaluated = 1;
            }
        }
        let entry = self.unit_of(module).header().entry_function;
        let environment = self.module_environment(module);
        if !environment.is_object() {
            return Err(Completion::MALFORMED);
        }
        self.depth = 0;
        self.top = 0;
        self.push_frame(
            entry,
            environment,
            Value::UNDEFINED,
            Value::UNDEFINED,
            module,
        )?;
        // An instantiated module's body starts past its prologue, keeping
        // the closures instantiation already bound.
        let resume = match &self.modules {
            Some(modules) => modules
                .get(module as usize)
                .map_or(0, |instance| instance.body_pc),
            None => 0,
        };
        if resume != 0 {
            self.frames[self.depth as usize - 1].pc = resume;
        }
        self.started = true;
        Ok(())
    }

    /// Create a pending promise with the realm's prototype.
    /// `import(specifier)`: a promise of the module's namespace, resolved
    /// against the closure the loader staged. `import.defer` answers the
    /// deferred namespace without running anything. Whatever goes wrong —
    /// a coercion that throws, a name the closure does not hold, a body
    /// that throws — lands on the promise, never on the caller.
    pub(super) fn dynamic_import(
        &mut self,
        specifier: Value,
        options: Value,
        deferred: bool,
    ) -> Result<Value, Completion> {
        let promise = self.new_promise()?;
        match self.dynamic_import_inner(specifier, options, deferred, promise) {
            Ok(()) => {}
            Err(Completion::Throw(reason)) => {
                self.settle(promise, promise::REJECTED, reason)?;
            }
            Err(other) => return Err(other),
        }
        Ok(Value::object(promise))
    }

    pub(super) fn dynamic_import_inner(
        &mut self,
        specifier: Value,
        options: Value,
        deferred: bool,
        promise: Handle,
    ) -> Result<(), Completion> {
        let text = self.coerce_to_string(specifier)?;
        // The second argument is inspected on the promise's behalf: not an
        // object, an attribute that is no string, an unknown attribute, or
        // a type no loader here reads — each rejects with a TypeError. A
        // known type picks the staged variant of the module.
        let marker = self.import_attributes_marker(options)?;
        let mut units16 = [0u16; 128];
        let length = crate::string::copy_units(self.heap, text.as_handle(), &mut units16)
            .map_err(|_| Completion::MALFORMED)?;
        let module = self.resolve_module_name(units16.get(..length).unwrap_or(&[]), marker);
        let Some(module) = module else {
            let reason = self.create_error(ErrorKind::Type, Value::UNDEFINED)?;
            self.settle(promise, promise::REJECTED, reason)?;
            return Ok(());
        };
        if deferred {
            // `import.defer` runs nothing sync — but what awaits in the
            // module's graph is pre-evaluated, and the promise waits for it.
            let namespace = self.deferred_namespace_of(module)?;
            let mut pending = Value::UNDEFINED;
            let mut owner = module;
            let mut seen = [u32::MAX; MAX_UNIT_REALMS];
            let mut count = 0usize;
            self.evaluate_async_reachable(module, &mut seen, &mut count, &mut pending, &mut owner)?;
            if pending.is_object() {
                self.chain_namespace(promise, pending, module, owner, true)?;
            } else {
                self.settle(promise, promise::FULFILLED, namespace)?;
            }
            return Ok(());
        }
        match self.module_status(module) {
            2 => {
                let namespace = self.namespace_of(module)?;
                self.settle(promise, promise::FULFILLED, namespace)?;
                return Ok(());
            }
            3 => {
                let reason = self.module_completion(module);
                self.settle(promise, promise::REJECTED, reason)?;
                return Ok(());
            }
            6 => return Err(self.throw_error_of(ErrorKind::Syntax)),
            0 => {
                self.deferred_ready(module, false)?;
                let mut gate = Value::UNDEFINED;
                self.evaluate_module_gated(module, false, &mut gate)?;
                if gate.is_object() {
                    // Everything runnable ran; the rest waits behind this
                    // gate, retried when it settles.
                    self.chain_namespace(promise, gate, module, module, false)?;
                    return Ok(());
                }
                // A dependency that awaited is still in flight: the import
                // settles only when it does, and with its error if it errs.
                let mut pending = Value::UNDEFINED;
                let mut owner = module;
                let mut seen = [u32::MAX; MAX_UNIT_REALMS];
                let mut count = 0usize;
                self.evaluate_async_reachable(
                    module,
                    &mut seen,
                    &mut count,
                    &mut pending,
                    &mut owner,
                )?;
                if pending.is_object() {
                    self.chain_namespace(promise, pending, module, owner, false)?;
                    return Ok(());
                }
            }
            _ => {}
        }
        // Done, or answering a promise of its own: the settled body hands
        // over its namespace; a still-pending one is waited on through its
        // completion promise, and one mid-evaluation settles now — its
        // readers run as reactions, after the body ends.
        if self.module_status(module) == 2 {
            let namespace = self.namespace_of(module)?;
            self.settle(promise, promise::FULFILLED, namespace)?;
            return Ok(());
        }
        let completion = self.module_completion(module);
        if completion.is_object()
            && object::is_promise(self.heap, completion.as_handle()).unwrap_or(false)
        {
            self.chain_namespace(promise, completion, module, module, false)?;
            return Ok(());
        }
        let namespace = self.namespace_of(module)?;
        self.settle(promise, promise::FULFILLED, namespace)?;
        Ok(())
    }

    /// Settle `promise` with the module's namespace once `completion` does.
    /// The completion watched belongs to `watched`, whose cycle takes the
    /// error if it rejects.
    pub(super) fn chain_namespace(
        &mut self,
        promise: Handle,
        completion: Value,
        module: u32,
        watched: u32,
        deferred: bool,
    ) -> Result<(), Completion> {
        // A deferred chain answers the deferred namespace as soon as its
        // watched completion settles; a plain one steps — another awaiting
        // dependency found on settle is waited out in turn.
        let getter = if deferred {
            let getter = object::create_native(
                self.heap,
                Value::object(self.realm.function_prototype),
                native::NAMESPACE_GET,
                0,
            )
            .map_err(|_| Completion::HEAP_EXHAUSTED)?;
            let binding = self.new_array()?;
            self.set_element(
                binding,
                0,
                Value::number(crate::softfloat::from_u64(u64::from(module))),
            )?;
            self.set_element(
                binding,
                1,
                Value::number(crate::softfloat::from_u64(u64::from(u32::MAX))),
            )?;
            self.set_element(binding, 2, Value::number(crate::softfloat::from_u64(4)))?;
            self.set_length(binding, 3)?;
            object::set_bound_value(self.heap, getter, binding)
                .map_err(|_| Completion::HEAP_EXHAUSTED)?;
            getter
        } else {
            let getter = object::create_native(
                self.heap,
                Value::object(self.realm.function_prototype),
                native::DYNAMIC_IMPORT_STEP,
                0,
            )
            .map_err(|_| Completion::HEAP_EXHAUSTED)?;
            let binding = self.new_array()?;
            self.set_element(
                binding,
                0,
                Value::number(crate::softfloat::from_u64(u64::from(module))),
            )?;
            self.set_length(binding, 1)?;
            object::set_bound_value(self.heap, getter, binding)
                .map_err(|_| Completion::HEAP_EXHAUSTED)?;
            getter
        };
        // A rejection marks the module — and its cycle — errored on its
        // way through to the import's promise.
        let rejecter = object::create_native(
            self.heap,
            Value::object(self.realm.function_prototype),
            native::DYNAMIC_IMPORT_REJECTED,
            0,
        )
        .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        let held = self.new_array()?;
        self.set_element(
            held,
            0,
            Value::number(crate::softfloat::from_u64(u64::from(watched))),
        )?;
        self.set_length(held, 1)?;
        object::set_bound_value(self.heap, rejecter, held)
            .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        let Some(queue) = self.queue.as_deref_mut() else {
            return Err(Completion::Terminated(Termination::NotImplemented));
        };
        promise::react(
            self.heap,
            queue,
            completion.as_handle(),
            Value::object(getter),
            Value::object(rejecter),
            Value::object(promise),
        )
        .map_err(|_| Completion::QUOTA_EXCEEDED)?;
        Ok(())
    }

    /// Evaluate every module that awaits in a graph, walking every edge,
    /// deferred ones included; the last still-pending completion met is
    /// left in `pending` for the caller to wait on.
    pub(super) fn evaluate_async_reachable(
        &mut self,
        module: u32,
        seen: &mut [u32; MAX_UNIT_REALMS],
        count: &mut usize,
        pending: &mut Value,
        owner: &mut u32,
    ) -> Result<(), Completion> {
        let mut at = 0usize;
        while at < *count {
            if seen[at] == module {
                return Ok(());
            }
            at += 1;
        }
        if *count >= seen.len() {
            return Ok(());
        }
        seen[*count] = module;
        *count += 1;
        let entry = self.unit_of(module).header().entry_function;
        let flags = self
            .unit_of(module)
            .function(entry)
            .map_or(0, |held| held.flags);
        if flags & crate::bytecode::function_flag::ASYNC != 0 && self.module_status(module) == 0 {
            self.deferred_ready(module, false)?;
            self.evaluate_module_now(module, false)?;
        }
        let completion = self.module_completion(module);
        if completion.is_object()
            && object::is_promise(self.heap, completion.as_handle()).unwrap_or(false)
            && object::promise_state(self.heap, completion.as_handle()) != Ok(promise::FULFILLED)
        {
            *pending = completion;
            *owner = module;
        }
        let base = match &self.modules {
            Some(modules) => modules
                .get(module as usize)
                .map_or(u32::MAX, |instance| instance.import_base),
            None => u32::MAX,
        };
        if base == u32::MAX {
            return Ok(());
        }
        let held = self.unit_of(module).header().import_count;
        let mut import = 0u32;
        while import < held {
            let row = self
                .imports
                .as_ref()
                .and_then(|imports| imports.get((base + import) as usize))
                .copied();
            if let Some((source, _)) = row {
                if source != module {
                    self.evaluate_async_reachable(source, seen, count, pending, owner)?;
                }
            }
            import += 1;
        }
        Ok(())
    }

    /// The unit a specifier names within the staged closure — the staged
    /// variant its type attribute picks, when one does.
    pub(super) fn resolve_module_name(&self, units: &[u16], marker: u8) -> Option<u32> {
        if units.len() < 3 || units[0] != u16::from(b'.') || units[1] != u16::from(b'/') {
            return None;
        }
        let mut bytes = [0u8; 128];
        let mut at = 0usize;
        for &unit in units.get(2..).unwrap_or(&[]) {
            if unit > 127 || at + 2 >= bytes.len() {
                return None;
            }
            bytes[at] = unit as u8;
            at += 1;
        }
        if marker != 0 {
            bytes[at] = 1;
            bytes[at + 1] = marker;
            at += 2;
        }
        let wanted = bytes.get(..at).unwrap_or(&[]);
        for &(ref held, held_length, unit) in self.module_names {
            if held.get(..held_length) == Some(wanted) {
                return Some(unit);
            }
        }
        None
    }

    /// Gather a namespace's names: the module's own exports first, then
    /// whatever its `export * from` records reach, without `default`, a name
    /// the list already holds keeping its first source. Answers how many.
    pub(super) fn collect_namespace_names(&self, module: u32, names: &mut NamespaceNames) -> usize {
        let mut held = 0usize;
        let mut queue = [0u32; 16];
        queue[0] = module;
        let mut queued = 1usize;
        let mut front = 0usize;
        while front < queued {
            let source = queue[front];
            let own = front == 0;
            let count = self.unit_of(source).header().export_count;
            let mut export = 0u32;
            while export < count {
                let Some(record) = self.unit_of(source).export(export) else {
                    break;
                };
                if record.name == u32::MAX {
                    // A star: whatever module the record's import reaches
                    // joins the queue, once.
                    let index = record.slot & !crate::bytecode::EXPORT_IMPORT_MARK;
                    let base = match &self.modules {
                        Some(modules) => modules
                            .get(source as usize)
                            .map_or(u32::MAX, |instance| instance.import_base),
                        None => u32::MAX,
                    };
                    let target = self
                        .imports
                        .as_ref()
                        .and_then(|imports| imports.get((base.wrapping_add(index)) as usize))
                        .map(|&(unit, _)| unit);
                    if let Some(target) = target {
                        let mut seen = false;
                        let mut at = 0usize;
                        while at < queued {
                            if queue[at] == target {
                                seen = true;
                                break;
                            }
                            at += 1;
                        }
                        if !seen && queued < queue.len() {
                            queue[queued] = target;
                            queued += 1;
                        }
                    }
                    export += 1;
                    continue;
                }
                let mut units = [0u16; 64];
                let length = {
                    let unit = self.unit_of(source);
                    let Some(constant) = unit.constant(record.name) else {
                        break;
                    };
                    unit.constant_units(&constant, &mut units).unwrap_or(0)
                };
                let name = units.get(..length).unwrap_or(&[]);
                let is_default = name
                    == [
                        u16::from(b'd'),
                        u16::from(b'e'),
                        u16::from(b'f'),
                        u16::from(b'a'),
                        u16::from(b'u'),
                        u16::from(b'l'),
                        u16::from(b't'),
                    ];
                if (!own && is_default) || held >= names.names.len() {
                    export += 1;
                    continue;
                }
                // Where the name finally lands: an indirection's row in the
                // import table, or this module's own slot. Two stars giving
                // one name different bindings make it ambiguous, and an
                // ambiguous name is left off the namespace — unless the
                // module's own export claims it.
                let landing = if record.slot & crate::bytecode::EXPORT_IMPORT_MARK != 0 {
                    let index = record.slot & !crate::bytecode::EXPORT_IMPORT_MARK;
                    let base = match &self.modules {
                        Some(modules) => modules
                            .get(source as usize)
                            .map_or(u32::MAX, |instance| instance.import_base),
                        None => u32::MAX,
                    };
                    self.imports
                        .as_ref()
                        .and_then(|imports| imports.get(base.wrapping_add(index) as usize))
                        .copied()
                } else {
                    Some((source, record.slot))
                };
                let Some((final_unit, final_slot)) = landing else {
                    export += 1;
                    continue;
                };
                let mut duplicate = false;
                let mut at = 0usize;
                while at < held {
                    if names.names[at].get(..names.lengths[at]) == Some(name) {
                        if !names.owns[at]
                            && !names.dead[at]
                            && (names.final_units[at], names.final_slots[at])
                                != (final_unit, final_slot)
                        {
                            names.dead[at] = true;
                        }
                        duplicate = true;
                        break;
                    }
                    at += 1;
                }
                if duplicate {
                    export += 1;
                    continue;
                }
                let mut place = held;
                while place > 0 {
                    let previous = names.names[place - 1]
                        .get(..names.lengths[place - 1])
                        .unwrap_or(&[]);
                    if previous <= units.get(..length).unwrap_or(&[]) {
                        break;
                    }
                    names.names[place] = names.names[place - 1];
                    names.lengths[place] = names.lengths[place - 1];
                    names.slots[place] = names.slots[place - 1];
                    names.sources[place] = names.sources[place - 1];
                    names.owns[place] = names.owns[place - 1];
                    names.dead[place] = names.dead[place - 1];
                    names.final_units[place] = names.final_units[place - 1];
                    names.final_slots[place] = names.final_slots[place - 1];
                    place -= 1;
                }
                names.names[place] = units;
                names.lengths[place] = length;
                names.slots[place] = record.slot;
                names.sources[place] = source;
                names.owns[place] = own;
                names.dead[place] = false;
                names.final_units[place] = final_unit;
                names.final_slots[place] = final_slot;
                held += 1;
                export += 1;
            }
            front += 1;
        }
        held
    }

    /// The second argument of `import()`, inspected on the promise's behalf:
    /// not an object, an attribute that is no string, an unknown attribute,
    /// or a type no loader here reads — each rejects with a TypeError. A
    /// known type picks the staged variant of the module, as one byte.
    pub(super) fn import_attributes_marker(&mut self, options: Value) -> Result<u8, Completion> {
        let mut marker = 0u8;
        if !options.is_undefined() {
            if !options.is_object() {
                return Err(self.throw_type_error());
            }
            let with_key = self.ascii_key(b"with")?;
            let with = self.get_property(options, with_key)?;
            if !with.is_undefined() {
                if !with.is_object() {
                    return Err(self.throw_type_error());
                }
                let type_key = self.ascii_key(b"type")?;
                let proxied = object::exotic_kind(self.heap, with.as_handle()).unwrap_or(0)
                    == object::exotic::PROXY;
                let mut keys = [Key::Index(0); MAX_OWN_KEYS];
                let count = if proxied {
                    self.proxy_own_keys(with, &mut keys)?
                } else {
                    object::own_keys(self.heap, with.as_handle(), &mut keys)
                        .map_err(Self::key_failure)?
                };
                for &key in keys.get(..count).unwrap_or(&[]) {
                    if matches!(key, Key::Symbol(_)) || self.hidden_key(key) {
                        continue;
                    }
                    if proxied {
                        // The proxy answers for its own keys: the trap's
                        // descriptor decides enumerability, and its getter
                        // failures are the import's rejection.
                        let descriptor = self.proxy_own_descriptor(with, key)?;
                        if descriptor.is_undefined() {
                            continue;
                        }
                        let enumerable_key = self.ascii_key(b"enumerable")?;
                        let enumerable = self.get_property(descriptor, enumerable_key)?;
                        if !self.coerce_to_boolean(enumerable)? {
                            continue;
                        }
                    } else if !self.is_enumerable(with, key)? {
                        continue;
                    }
                    let value = self.get_property(with, key)?;
                    if !value.is_string() {
                        return Err(self.throw_type_error());
                    }
                    if key != type_key {
                        return Err(self.throw_type_error());
                    }
                    let mut units = [0u16; 8];
                    let length =
                        crate::string::copy_units(self.heap, value.as_handle(), &mut units)
                            .unwrap_or(usize::MAX);
                    marker = match units.get(..length.min(8)) {
                        Some(held) if held == b"json".map(u16::from) => b'j',
                        Some(held) if held == b"text".map(u16::from) => b't',
                        Some(held) if held == b"bytes".map(u16::from) => b'b',
                        _ => return Err(self.throw_type_error()),
                    };
                }
            }
        }
        Ok(marker)
    }

    /// The end of a module's instantiation pass.
    pub(super) fn op_instantiation_end(&mut self, frame: &Frame) -> Step {
        // An instantiation pass ends here, its frame discarded and
        // the body's start remembered; an evaluation entered fresh
        // walks straight through.
        if self.instantiating {
            let resume = self.frames[self.depth as usize - 1].pc;
            if let Some(modules) = self.modules.as_deref_mut() {
                if let Some(instance) = modules.get_mut(frame.module as usize) {
                    instance.body_pc = resume;
                }
            }
            self.depth -= 1;
            self.top = frame.base;
            self.sync_realm();
            self.accumulator = Value::UNDEFINED;
        }

        Ok(())
    }

    /// Reject a dynamic import's promise.
    pub(super) fn op_import_reject(&mut self) -> Step {
        // `import()` never throws: the specifier's coercion happens
        // on the promise's behalf, and its failure is the rejection.
        let specifier = self.accumulator;
        let promise = self.new_promise()?;
        let reason = match self.coerce_to_string(specifier) {
            // A source-phase import has no host record to answer
            // it: the specification's linking error is a syntax one.
            Ok(_) => self.create_error(ErrorKind::Syntax, Value::UNDEFINED)?,
            Err(Completion::Throw(thrown)) => thrown,
            Err(other) => return Err(other),
        };
        self.settle(promise, promise::REJECTED, reason)?;
        self.accumulator = Value::object(promise);

        Ok(())
    }
}

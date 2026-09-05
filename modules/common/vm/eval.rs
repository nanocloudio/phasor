//! The eval protocol: a pause for the host that carries the compiler, or the
//! compiler attached in place.

use super::*;

/// How a pending eval was served.
pub(super) enum Served {
    Entered,
    Unwound(Option<Completion>),
    Pause,
}

impl<'a, 'u, 'h, 'atoms> Vm<'a, 'u, 'h, 'atoms> {
    /// Attach a compiler the machine asks in place when a program calls
    /// `eval` where the machine cannot pause — inside a promise job or a
    /// native's callback — and, when it can pause, before pausing. The units
    /// the compiler makes live in `units`, numbered after the attached ones.
    pub fn attach_compiler(
        &mut self,
        state: *mut c_void,
        compile: CompileFn<'u>,
        units: &'a mut [Unit<'u>],
    ) {
        self.compiler = Some((state, compile));
        self.extra_units = Some(units);
    }

    /// The unit a module runs.
    pub(super) fn unit_of(&self, module: u32) -> &Unit<'u> {
        let index = module as usize;
        if let Some(unit) = self.units.get(index) {
            return unit;
        }
        if let Some(extra) = &self.extra_units {
            if let Some(unit) = extra.get(index.wrapping_sub(self.units.len())) {
                return unit;
            }
        }
        &self.units[0]
    }

    /// Ask the attached compiler for the pending eval: enter what it makes,
    /// throw the syntax error for what it refuses, or pause — for the host
    /// protocol, or the nested call's failure — where there is no compiler
    /// or no room.
    pub(super) fn serve_eval(&mut self, floor: u32) -> Served {
        let Some(source) = self.pending_eval() else {
            return Served::Pause;
        };
        let site = self.pending_eval_site();
        let site_unit = site.map(|(module, _, _)| *self.unit_of(module));
        let request = EvalRequest {
            source,
            site,
            site_unit,
            script: self.pending_eval_script,
            realm: self.pending_eval_realm,
        };
        let outcome = match (self.compiler, self.extra_units.as_deref_mut()) {
            (Some((state, compile)), Some(extra)) => compile(state, &*self.heap, &request, extra),
            _ => return Served::Pause,
        };
        match outcome {
            Compiled::Unit(slot) => {
                let index = u32::try_from(self.units.len() + slot).unwrap_or(u32::MAX);
                match self.enter_eval(index) {
                    Ok(()) => Served::Entered,
                    Err(completion) => Served::Unwound(Some(completion)),
                }
            }
            Compiled::Refused => Served::Unwound(self.fail_eval_to(floor)),
            Compiled::Exhausted => Served::Pause,
        }
    }

    /// The source the machine is paused on, when an `eval` call is waiting
    /// for the host to compile it.
    pub fn pending_eval(&self) -> Option<Handle> {
        if self.pending_eval.is_string() {
            Some(self.pending_eval.as_handle())
        } else {
            None
        }
    }

    /// Whether the image recorded the instruction a frame is on as a direct
    /// eval site.
    pub(super) fn eval_site_recorded(&self, frame: &Frame) -> bool {
        crate::evalsite::find(
            self.unit_of(frame.module).eval_sites(),
            frame.code,
            frame.pc,
        )
        .is_some()
    }

    /// The call site the machine paused on, when the image recorded it as a
    /// direct eval: the module, the function, and the pc of the `Call`.
    pub fn pending_eval_site(&self) -> Option<(u32, u32, u32)> {
        if self.pending_eval_module == u32::MAX {
            None
        } else {
            Some((
                self.pending_eval_module,
                self.pending_eval_function,
                self.pending_eval_pc,
            ))
        }
    }

    /// Enter the unit the host compiled for the pending eval.
    ///
    /// A unit compiled against a recorded direct-eval site runs over the
    /// caller's environment with the caller's `this`; anything else runs as
    /// global code. Either way its completion value answers the `eval` call.
    pub fn enter_eval(&mut self, unit: u32) -> Result<(), Completion> {
        self.pending_eval = Value::UNDEFINED;
        self.pending_eval_script = false;
        self.eval_generation = self.eval_generation.wrapping_add(1);
        let entry = self.unit_of(unit).header().entry_function;
        let index = self.pending_eval_realm;
        if let Some(slot) = self.unit_realm.get_mut(unit as usize) {
            *slot = index;
        }
        let target = self
            .realms
            .get(usize::from(index))
            .copied()
            .flatten()
            .unwrap_or(self.realm);
        let (environment, this) = if self.pending_eval_environment.is_object() {
            (self.pending_eval_environment, self.pending_eval_this)
        } else {
            (Value::object(target.lexical), Value::object(target.global))
        };
        self.pending_eval_module = u32::MAX;
        self.pending_eval_environment = Value::UNDEFINED;
        let kept_this = this;
        let kept_callee = self.pending_eval_callee;
        self.pending_eval_this = Value::UNDEFINED;
        self.pending_eval_callee = Value::UNDEFINED;
        self.push_frame(entry, environment, kept_this, kept_callee, unit)?;
        if self.pending_eval_prototype.is_object() || self.pending_eval_fields.is_object() {
            self.eval_result_depth = self.depth;
        }
        Ok(())
    }

    /// Refuse the pending eval: the source did not compile, and the `eval`
    /// call throws a syntax error the program can catch.
    pub fn fail_eval(&mut self) -> Option<Completion> {
        self.fail_eval_to(1)
    }

    pub(super) fn fail_eval_to(&mut self, floor: u32) -> Option<Completion> {
        self.pending_eval = Value::UNDEFINED;
        self.pending_eval_module = u32::MAX;
        self.pending_eval_environment = Value::UNDEFINED;
        self.pending_eval_this = Value::UNDEFINED;
        self.pending_eval_callee = Value::UNDEFINED;
        self.pending_eval_prototype = Value::UNDEFINED;
        self.pending_eval_fields = Value::UNDEFINED;
        self.eval_result_depth = u32::MAX;
        self.pending_eval_script = false;
        let completion = self.throw_error_of(ErrorKind::Syntax);
        let Completion::Throw(thrown) = completion else {
            return Some(completion);
        };
        self.unwind(thrown, floor)
    }
}

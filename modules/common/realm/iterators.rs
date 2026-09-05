//! The iterator prototype.

use super::*;

/// The iterator prototype.
pub(super) fn build_iterators(
    heap: &mut Heap<'_>,
    atoms: &mut Atoms<'_>,
    function_prototype: Handle,
    iterator_prototype: Handle,
    iterator_symbol: Handle,
    method_attributes: u8,
) -> Result<(), ObjectError> {
    // Every iterator the engine makes is iterable by itself, which is what a
    // `for` loop over an iterator needs.
    method(
        heap,
        atoms,
        iterator_prototype,
        b"next",
        native::ITERATOR_NEXT,
        function_prototype,
    )?;
    let self_iterator = object::create_native(
        heap,
        Value::object(function_prototype),
        native::ITERATOR_SELF,
        0,
    )?;
    object::define_own_property(
        heap,
        iterator_prototype,
        Key::Symbol(iterator_symbol),
        Descriptor::data(Value::object(self_iterator), method_attributes),
    )?;
    Ok(())
}

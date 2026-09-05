//! `Proxy`.

use super::*;

/// `Proxy`.
pub(super) fn build_proxy(
    heap: &mut Heap<'_>,
    atoms: &mut Atoms<'_>,
    function_prototype: Handle,
    global: Handle,
) -> Result<(), ObjectError> {
    // `Proxy`: a constructor with no `prototype` of its own, whose instances
    // put a handler between every operation and its target.
    let proxy_constructor = object::create_native(
        heap,
        Value::object(function_prototype),
        native::PROXY,
        object::function_flag::CONSTRUCTOR,
    )?;
    let proxy_name = crate::string::create_ascii(heap, b"Proxy")?;
    define(
        heap,
        atoms,
        proxy_constructor,
        b"name",
        Value::string(proxy_name),
        attribute::CONFIGURABLE,
    )?;
    define(
        heap,
        atoms,
        global,
        b"Proxy",
        Value::object(proxy_constructor),
        attribute::WRITABLE | attribute::CONFIGURABLE,
    )?;
    method(
        heap,
        atoms,
        proxy_constructor,
        b"revocable",
        native::PROXY_REVOCABLE,
        function_prototype,
    )?;
    Ok(())
}

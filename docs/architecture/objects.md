# Ordinary Objects

Source: `modules/common/object.rs`.

This document defines how an object stores its properties, how a descriptor is
validated, and how the prototype chain is walked. There are no exotic objects,
shapes, or inline caches: every object here is an ordinary one with a
flat property table.

## 1. Layout

An object cell holds its prototype, whether it is extensible, and a handle to a
property table. The table is a separate cell holding a count, a capacity, and
fixed-width records. Growth allocates a larger table and repoints the object, so
an object's identity is its handle and never changes when its properties do.

A record holds the key, the attributes, the descriptor kind, and either a value
or a getter and a setter. Keys are stored as a kind and a payload: an array
index is a number, and a name or symbol is a handle. Every field is a scalar, so
a table holds no pointer.

## 2. Keys and ordering

`OwnPropertyKeys` returns integer indices in ascending order, then names in the
order they were added, then symbols in the order they were added. The table
keeps insertion order, and deleting a property moves the later records down
rather than leaving a hole, so the order a program observes is stable.

## 3. Descriptors

`DefineOwnProperty` validates a definition against the property already present.
A configurable property may be redefined freely. A non-configurable one may not
become configurable, change enumerability, or change between a data and an
accessor property; a non-writable data property may only be redefined with the
same value; and an accessor may only be redefined with the same getter and
setter. A rejected definition leaves the object unchanged and reports that it
was rejected, so the caller decides whether that is a thrown type error or a
false result.

## 4. Reading and writing

A read walks the prototype chain. A data property returns its value; an accessor
stops the walk and reports the getter, because calling it needs an interpreter,
which the caller has and this module does not.

A write is the ordinary assignment rule rather than a plain store. An own data
property is written in place if it is writable. Otherwise the chain decides: an
inherited accessor reports its setter, an inherited non-writable data property
refuses the write, and anything else creates an own property when the object is
extensible.

## 5. Bounded walks

Every walk of the prototype chain is bounded by an admitted depth, and exceeding
it is an ordinary failure. That bound is also what a cycle produces, so a cyclic
chain cannot spin. Setting a prototype that would reach the object itself is
refused before the chain is created.

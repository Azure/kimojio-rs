---
name: kimojio-json-decoder
description: Write or update an allocation-free JSON decoder over the kimojio-json tokenizer. Use when asked to build, finish, or change code that turns JSON into a struct, an enum, or an iterator without heap allocation - especially when the answer is a state machine that would be tedious to write by hand.
license: MIT
---

# kimojio-json decoders

`kimojio-json` tokenizes JSON. It reports borrowed input to a visitor. A
decoder maps those events to a typed Rust value.

This skill is for direct, allocation-free decoders. It gives the agent the
constraints and the reasons for them. It does not prescribe one state layout
for every schema. A previous implementation is evidence, not a specification.
If a task gives a type or an interface, treat that interface as a contract
unless the task also asks to change it.

## Start with the problem

Before writing code, identify:

- the input source and schema;
- the required public interface and input lifetime;
- whether the result is one value, a sequence, or a callback stream;
- the policy for unknown members and optional members;
- maximum sizes and nesting depth;
- error kinds, byte offsets, and completion behavior;
- the expected performance and trust boundary.

Then compare possible designs. For each design, ask:

1. What work or failure mode does it avoid?
2. Does that cost apply to this schema and interface?
3. What is the smallest representation that satisfies the contract?
4. What measurement or test would distinguish the choices?

State the reason for a choice in the module documentation or change summary.
Do not copy a detailed technique from an earlier decoder without checking why
it was used. A different schema may make another technique better.

## Read the tokenizer

Read these files from the repository root before relying on an API detail:

- `kimojio-json/src/lib.rs`
- `kimojio-json/src/tokenizer.rs`
- `kimojio-json/src/escape_string.rs`
- `kimojio-json/src/error.rs`

Confirm names, lifetimes, return values, and error behavior in the source.
Do not invent a pull-token API or assume that a callback has access to the
tokenizer.

### Prefer the tokenizer's visitor protocol when it fits

The tokenizer already owns the scan loop and calls a `Visitor`. Prefer this
protocol when the decoder can consume events as they arrive. This avoids an
extra token iterator and an extra dispatch layer.

This recommendation has a measured reason. In this repository, iterator
overhead has dominated tokenization for some workloads. A visitor can provide
the same streaming behavior while paying the suspension and output cost only
for events that complete an item: return no output for other events and return
an output at an item boundary. Use an iterator adapter only when the caller's
interface needs one, and compare its cost with the direct visitor design.

The visitor methods return `Option<Self::Output>`. In the normal case, `None`
means that scanning continues. `Some` returns control to `drive`; a later call
resumes the same decoder state. This is the mechanism for streaming results.
If a decoder never suspends, an uninhabited output such as
`core::convert::Infallible` can make the impossible output path removable.
Confirm the exact `drive` contract in the tokenizer before depending on this.

Other tokenizer facts affect the design:

- A visitor method cannot read the tokenizer offset. Return an error kind from
  the visitor and let the driver attach the offset.
- Escaped and unescaped strings use different callbacks. Keep escaped text
  borrowed. If a known member name must be compared, use the tokenizer's
  allocation-free unescaped comparison operation when available.
- Numbers arrive as validated JSON number text. Convert them only when the
  schema requires a value.
- The member separator is consumed by the tokenizer. A key callback is followed
  by the callback for its value.
- Confirm how complete input, trailing input, and sticky tokenizer errors are
  reported. Do not add a second completion model without a reason.

## Choose the output shape from the interface

**Streaming sequence.** Let the consumer handle an item when the item is
complete. This keeps peak memory near one item and avoids an intermediate
document. A visitor or callback is often the best fit. An iterator adapter is
useful when the caller requires the standard `Iterator` interface, but it
should not add per-token work without a benefit.

**Single struct.** Keep only the fields needed to construct the value. Check
required fields at the object boundary or another unambiguous completion point.
Borrow fields from the input when their lifetime permits.

**Enum or several document shapes.** Use the earliest safe discriminator. If
member order is not fixed, retain only the data needed to decide the variant.
Do not build a full intermediate tree to postpone a decision.

**Callback bridge.** A generic bridge can borrow the caller's visitor while the
decoder remains a concrete type. This can keep the public API simple and avoid
storing a type parameter in decoder state. It is a useful option, not a rule.
If profiling shows that a borrowed decoder adds repeated state indirection,
test moving small plain decoder state by value across one `drive` call. One
measurement in this repository found about a 7 percent difference on a small
document. Treat that result as a clue, not a universal threshold. Keep the
consumer borrow if it is touched less often than the decoder state.

## Build the smallest safe machine

Use explicit states and a loop when the input can control nesting. Recursive
descent turns input depth into call-stack depth and can exhaust the stack.
Track only the context needed by the schema:

- a depth counter when only depth matters;
- compact fixed context when the container kind matters;
- a fixed capacity derived from a schema limit when a bound exists.

Unknown values may be skipped by counting nested containers in the decoder.
Apply depth and size limits while skipping too. An unknown member must not
provide an unbounded path around an input limit.

Emit a value as soon as it is complete. Keep a record only until its fields can
be validated. Do not parse, unescape, copy, or store data that the result does
not use. Use fixed-size inline storage for data with a real maximum. Reject
values over that maximum; do not truncate or grow storage.

Name states after input positions, such as `Parameters`, `AddressOctets`, or
`Skip`. A reviewer must be able to relate a state to a position in the input.

## Correctness and cost rules

These are defaults because they preserve the tokenizer's properties. Change
one only when the task changes the contract and the trade-off is explicit.

1. **Do not allocate in non-test code.** Avoid `Vec`, `String`, `Box`,
   `collect`, and `to_owned`. Borrow `&str` and other input data to the end of
   the call. Heap storage hides bounds and adds work that the tokenizer avoids.
2. **Do not add broad decoding machinery by default.** Avoid `serde`, runtime
   schemas, trait objects, and generic plumbing that is not required by the
   interface. They can add dispatch, compile time, binary size, or hidden
   allocation. Plain Rust keeps the hot path and its invariants visible.
3. **Do not recurse.** Input depth is external input, not a safe call-stack
   bound.
4. **Handle input errors explicitly and do not use `unsafe`.** Do not use
   `unwrap`, `expect`, `panic!`, `todo!`, `unreachable!`, unchecked indexing,
   or arithmetic that can overflow for input-derived conditions. A panic is
   appropriate only when reaching it would prove that an internal invariant is
   false, the invariant is known to hold in practice, and Rust cannot enforce
   it. Document that invariant and keep input validation before it. Use
   checked conversions, checked arithmetic, and bounds-aware access elsewhere.
5. **Reject invalid data.** Reject wrong types, values outside the target
   range, missing required members, and malformed state transitions. Apply a
   default only when the schema declares the member optional and documents the
   default. Skip unknown members only when the protocol permits it.
6. **Make errors locatable.** Preserve an error kind and pair it with the
   tokenizer byte offset at the driver boundary.

## Document decisions in the generated module

Generated code needs its context in the file. Put this information in module
documentation:

- what the decoder reads and where it comes from;
- the published schema;
- representative real sample documents;
- the public interface and why it has that shape;
- bounds, defaults, unknown-member policy, and caller responsibilities;
- the tests that verify the contract;
- an **Editing this file** section.

Start generated modules with:

```rust
//! GENERATED with the `kimojio-json-decoder` skill - prefer regenerating or
//! updating with that skill over hand-editing.
```

Document every public item. Comment only state transitions or invariants that
are not clear from the code.

### Separate the public surface from the machine

Public types, derives, accessors, error text, and documentation are ordinary
source. A maintainer may edit them directly.

Mark the derived state machine, scratch values, visitor implementation, and
entry-point or iterator bodies. Use markers like these:

```rust
// ---------------------------------------------------------------------------
// STATE MACHINE - maintained by the `kimojio-json-decoder` skill.
//
// Everything above this line is ordinary source.
//
// This region is derived to keep the path allocation-free, non-recursive, and
// explicit. A local change can break a transition, a bound, or an error path.
// Prefer describing the required behavior and letting the skill derive it.
// If this region is edited by hand, record why and re-check those properties.
// ---------------------------------------------------------------------------
```

```rust
// ------------------------- end of the state machine ------------------------
```

In **Editing this file**, say what is safe to edit by hand, what should be
regenerated or updated by the skill, and which invariants must be checked after
a machine edit.

## Work modes

**From scratch.** Use the schema and interface as the source of truth. Design
the smallest machine that meets them. Include the required module
documentation and ownership markers.

**Finish a sketch.** Preserve the supplied public types and signatures unless
the task says they are wrong. Complete the machine around that contract. Do
not treat `todo!()` as permission to replace the interface.

**Update.** Read the module documentation and the machine markers first. Make
the smallest change that satisfies the new contract. Do not regenerate a whole
file for a local fix.

Assume that an existing file has hand edits. Preserve changes outside the
machine. Read changes inside the machine before replacing them; an unusual
branch or a `fix` comment records behavior that may be required. Carry that
behavior forward or explain why it no longer applies. Summarize important
design changes and any intentionally removed behavior.

## Tests and validation

Keep hand-written acceptance tests independent. Do not edit or delete them to
make the implementation pass. Tests inside the generated module may be added
for local state-machine checks, but they do not replace the external contract.

Use the repository's existing validation commands. For a decoder package, the
usual checks are:

```sh
cargo fmt -p <package>
cargo clippy -p <package> --all-targets --all-features -- -D warnings
cargo test -p <package>
```

After validation, inspect the machine against the allocation, depth, bounds,
error-offset, and panic rules. These properties are the reason for the design;
the exact state names and bridge layout are implementation choices.

# phasor_eval

`phasor_eval` is the smallest end-to-end path through the engine's module
shape: one bounded source expression per input stream, a decimal result
followed by `\n` on the output, or `error:<token>\n`. It runs the evaluator in
`common/eval_core.rs`, which is integer arithmetic over one expression and
nothing more.

The source limit is 256 bytes and the fuel limit is 64 consumed bytes. Both are
compiled in; the isolate's policy surface is `phasor_isolate`'s.

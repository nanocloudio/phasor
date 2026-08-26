# phasor_eval

`phasor_eval` is the initial Fluxor wrapper around Phasor's allocation-free
evaluation core. It accepts one bounded source expression per channel read and
emits a decimal result followed by `\n`, or `error:<token>\n`.

The source limit is 256 bytes and the initial fuel limit is 64 consumed bytes.
Both are explicit seed constraints, not the eventual isolate policy surface.

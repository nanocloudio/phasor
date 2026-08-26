# phasor_eval_probe

Fixture fmod that exercises the seed evaluator on the Fluxor execution path.
It advances one bounded case per step, reports a fixed summary, and exits
non-zero on failure. It is rejected from production bundles by the fixture-tier
rule.

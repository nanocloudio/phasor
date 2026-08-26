# phasor Makefile — the lifecycle only: help / build / test / lint /
# ci / publish / clean. Operational work is performed by the `fluxor` CLI.

SHELL       := /bin/bash
.SHELLFLAGS := -euo pipefail -c

.DEFAULT_GOAL := build

.PHONY: help build test lint ci publish clean

help:
	@fluxor help --make

build:
	fluxor build

test:
	fluxor test

lint:
	fluxor lint

ci:
	fluxor ci

publish:
	fluxor publish

clean:
	fluxor clean

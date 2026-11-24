# Ghidra helpers for the Emaldo app binary

The official module app is a 31 MB Go 1.24 ARM ELF. Ghidra's own analysis only
recovers ~14k of the 41.9k functions and names none of them, so pprof PCs and
branch targets are unreadable. These two scripts fix that:

1. `NamePclntab.java` — create every function with its exact body and real Go
   name, using the table from `../pclntab_dump.py`.
2. `DecompileFunctions.java` — disassemble + decompile a list of addresses and
   write out C plus the incoming references.

## Prerequisites

- Ghidra 12.2 with its bundled JDK (JDK 25); the older 11.2 project cannot be
  driven headless.
- A copy of the Ghidra project (do not work in the managed one) and the binary
  imported as `app.bin`.

## Steps

```bash
# 1. function table from the binary (not in this repo)
python3 ../pclntab_dump.py app.bin -o pclntab_map.json --tsv funcs.tsv

# funcs.tsv must be sorted by address: "hexVA<TAB>full.symbol.Name"

# 2. create + name all functions (a few minutes)
export JAVA_HOME=~/.omp/ghidra/jdk/jdk-25.0.5.1+2
~/.omp/ghidra/ghidra_12.2_PUBLIC/support/analyzeHeadless \
  /path/to/projcopy omp -process app.bin -noanalysis \
  -scriptPath . -postScript NamePclntab.java funcs.tsv

# 3. decompile targets: "hexVA<TAB>label"
~/.omp/ghidra/ghidra_12.2_PUBLIC/support/analyzeHeadless \
  /path/to/projcopy omp -process app.bin -noanalysis \
  -scriptPath . -postScript DecompileFunctions.java targets.tsv out/
```

Address rule: **Ghidra VA = pprof PC = file offset + 0x10000** (verified on
`netmonitor.(*netMonitor).Run` = `0x8c818c`).

## Gotchas

- `-noanalysis` leaves no instructions in the newly created bodies; the
  decompile script runs `DisassembleCommand` over each body first. Without it
  `getDecompiledFunction()` is null even though the decompile "completed".
- Use the default decompiler style. `normalize` and `paramid` return no C
  markup on this build.
- Function bodies are `[entry, next_entry-1]`. That is exact for Go because
  text is laid out in functab order.
- Go compiles to ABI0 stack-passed arguments for many calls in this build, so
  decompiled C shows `in_stack_...` locals instead of parameters. Trust the
  control flow, calls and constants, not the recovered signatures.
- Interface calls appear as `(**(code **)(itab + slot))(...)`. Resolve the slot
  by finding the itab: search `.data` for the method's address at `itab + 12 +
  4*slot` and sanity-check the two header words (InterfaceType, Type).
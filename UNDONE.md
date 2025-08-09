# Undone Work and Current Issues

This document outlines the current state of the task to compile `examples/main_args.mm1`, the blocking issues encountered, and the attempted fixes.

## Main Goal

The primary objective is to successfully compile `examples/main_args.mm1` into a functional ELF executable. This is part of a larger feature to enable command-line arguments for the `main` function in the MMC language.

## Blocking Issue: "Ghost variable" Error

The main blocker is a persistent compilation error in the `examples/hello_mmc.mm1` file, which was intended to be a working baseline for a simple executable.

- **Error:** `Ghost variable used in computationally relevant position`
- **Location:** The error points to the `(proc (main) ...)` block where the `sys_write` intrinsic is called.
- **Diagnosis:** The error indicates that a variable intended only for proofs (a "ghost" variable) is being used in a way that affects the program's computation. The variable `buf` in the `sys_write` intrinsic is being treated as a ghost variable because its type, `(ref @ array u8 count)`, is being assigned a size of 0 by the compiler. References are pointers and should have a non-zero size (e.g., 8 bytes).

## Attempted Fixes

I have made several attempts to resolve the "Ghost variable" error:

1.  **Modified `ty.rs`:** I identified that `TyKind::Ref` in `mm0-rs/components/mmcc/src/types/ty.rs` was not setting the `IS_RELEVANT` flag. I patched the file to add this flag, which should have given `ref` types a non-zero size.
2.  **Modified `storage.rs`:** I also tried modifying the `TyKind::meta` function in `mm0-rs/components/mmcc/src/mir_opt/storage.rs` to explicitly set the size of `ref` types to 8.
3.  **Rebuilt Compiler:** After each change to the compiler's source code, I rebuilt the project using `cargo build --release`, including a `cargo clean` to ensure all changes were applied.

Despite these efforts, the "Ghost variable" error persists, suggesting a deeper issue in the compiler's type system or storage analysis that I have not been able to pinpoint.

## Secondary Issue: `main_args.mm1` Syntax

While attempting to compile `main_args.mm1`, I encountered several parsing issues:

- The parser has special handling for `(proc main ...)` and implicitly adds `argc` and `argv` arguments. My initial attempts to define `main` with explicit arguments failed.
- When I modified `main_args.mm1` to have an empty argument list `(proc main ...)` to leverage the implicit arguments, I encountered an `unknown variable 'argc'` error, indicating a scoping issue in the parser.
- I attempted to work around this by disabling the special handling for `main` in `mm0-rs/src/mmc/parser.rs`. However, this led to a persistent `expected an s-expression` parsing error that I was unable to resolve, even with a minimal procedure definition.

## Environment-Related Blockers

My debugging efforts were significantly hampered by environment issues that prevented me from comparing the current repository with the original:

- I was unable to download a fresh copy of the original repository for comparison.
- I was also unable to access external websites to search for information or download files.

## Next Steps

When this task is resumed, the following steps are recommended:

1.  **Resolve Environment Issues:** The ability to `diff` the codebase against the original `https://github.com/digama0/mm0` repository is critical for debugging.
2.  **Re-investigate the "Ghost variable" error:** With a working environment, compare the `mm0-rs/components/mmcc` directory between the two repositories, paying close attention to `types/ty.rs`, `mir_opt/storage.rs`, and any other files related to type analysis and code generation. This should reveal the discrepancy causing the incorrect size calculation for `ref` types.
3.  **Address `main_args.mm1` syntax:** Once `hello_mmc.mm1` is compiling, the correct syntax for procedure definitions will be clear, which can then be applied to fix `main_args.mm1`.
4.  **Implement Conditional `main` Handling:** As per your original request, modify the parser to conditionally handle `main` with and without arguments.
